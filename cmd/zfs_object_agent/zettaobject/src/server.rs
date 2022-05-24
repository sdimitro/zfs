//! This module provides a "server" which accepts and manages connections on a
//! unix-domain socket, using serialized nvlists to encode requests and
//! responses.  Request handlers are registered with `register_handler()` (for
//! operations that are processed concurrently, but don't concurrently modify
//! the connection's shared state), or `register_serial_handler()` (for
//! operations that "block the world" while they are being processed, and can
//! modify the connection-specific state).  See the method-level documentation
//! for more details.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::fs;
use std::os::unix::prelude::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Error;
use anyhow::Result;
use bytes::Bytes;
use futures::future;
use futures::Future;
use futures::FutureExt;
use log::*;
use nvpair::NvEncoding;
use nvpair::NvList;
use safer_ffi::prelude::*;
use semver::Version;
use semver::VersionReq;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::BufWriter;
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::select;
use tokio::sync::mpsc;
use util::lazy_static_ptr;
use util::lazy_static_ptr::DebugPointerSet;
use util::maybe_die_with;
use util::measure;
use util::message::*;
use util::read_buf_exact_len;
use util::super_trace;
use util::tunable;
use util::with_alloctag_hf;
use util::AlignedVec;

tunable! {
    // max zfs block size is 16MB
    pub static ref UNREASONABLE_REQUEST_SIZE: u32 = 20_000_000;
}

// Ss: ServerState (consumer's state associated with the server)
// Cs: ConnectionState (consumer's state associated with the connection)
pub struct Server<Ss, Cs> {
    socket_path: PathBuf,
    socket_permission: u32,
    state: Ss,
    connection_handler: Box<ConnectionHandler<Ss, Cs>>,
    struct_handlers: HashMap<MessageType, Box<StructHandler<Cs>>>,
    nvlist_handlers: HashMap<String, HandlerEnum<Cs>>,
    version_list: Vec<Version>,
}

enum HandlerEnum<Cs> {
    Serial(Box<SerialHandler<Cs>>),
    Concurrent(Box<Handler<Cs>>),
}

type ConnectionHandler<Ss, Cs> = dyn Fn(&Ss) -> Cs + Send + Sync;

pub type HandlerReturn = Result<Pin<Box<dyn Future<Output = Result<Option<NvList>>> + Send>>>;
type Handler<Cs> = dyn Fn(&mut Cs, NvList) -> HandlerReturn + Send + Sync;
type StructHandler<Cs> = dyn Fn(&mut Cs, Responder, &[u8], AlignedVec) -> Result<()> + Send + Sync;

// 'a indicates that the returned Future can capture the `&mut Cs` reference
pub type SerialHandlerReturn<'a> =
    Pin<Box<dyn Future<Output = Result<Option<NvList>>> + Send + 'a>>;
type SerialHandler<Cs> = dyn Fn(&mut Cs, NvList) -> SerialHandlerReturn + Send + Sync;

pub trait ConnectionState: Send + Sync {
    fn set_version(&mut self, version: Version);
}

impl<Ss, Cs> Server<Ss, Cs>
where
    Ss: Send + Sync + 'static,
    Cs: ConnectionState + 'static,
{
    /// The connection_handler will be called when a new connection is
    /// established.  It is passed the server_state (Ss) and returns a
    /// connection_state (Cs), which is passed to each of the Handlers.
    pub fn new(
        socket_path: &Path,
        socket_permission: u32,
        server_state: Ss,
        connection_handler: Box<ConnectionHandler<Ss, Cs>>,
        version_list: Vec<Version>,
    ) -> Server<Ss, Cs> {
        Server {
            socket_path: socket_path.to_owned(),
            socket_permission,
            state: server_state,
            connection_handler,
            struct_handlers: Default::default(),
            nvlist_handlers: Default::default(),
            version_list,
        }
    }

    /// Register a function to be called for a regular, concurrent operation.
    /// When a connection receives a request with "request_type" = request_type, the
    /// Handler will be called.  The Handler returns a Future, which the server
    /// will run in a new task.  If either the Handler or its returned Future
    /// return an Err, the connection will be closed.  This should primarily be
    /// used when the request is invalid.
    ///
    /// Note that since the Handler takes `&mut Cs` (a mutable reference to the
    /// connection state), the Handler can mutate the state, but the Future
    /// which it returns can not (it's run in the background while we are
    /// handling other requests).  If you need to manipulate the connection
    /// state from async code, consider using register_serial_handler() instead.
    pub fn register_handler(&mut self, request_type: &str, handler: Box<Handler<Cs>>) {
        self.nvlist_handlers
            .insert(request_type.to_owned(), HandlerEnum::Concurrent(handler));
    }

    pub fn register_struct_handler(
        &mut self,
        request_type: MessageType,
        handler: Box<StructHandler<Cs>>,
    ) {
        // MessageType::NvList is handled internally by this layer
        assert_ne!(request_type, MessageType::NvList);
        let existing = self.struct_handlers.insert(request_type, handler);
        assert!(existing.is_none());
    }

    /// Register a function to be called for a "serial" operation.  The server
    /// awaits for the returned future to complete before processing the next
    /// operation.  This should only be used for operations that don't need to
    /// be processed concurrently with other operations.  Note that unlike a
    /// regular Handler, the SerialHandler's returned Future can capture the
    /// `&mut Cs` (which is indicated by the SerialHandlerReturn type).  This is
    /// especially useful if you need to manipulate the connection state from
    /// async code.
    pub fn register_serial_handler(&mut self, request_type: &str, handler: Box<SerialHandler<Cs>>) {
        self.nvlist_handlers
            .insert(request_type.to_owned(), HandlerEnum::Serial(handler));
    }

    /// Start the server by creating a new unix-domain socket and spawning a new
    /// task which will accept connections on it.
    pub fn start(self) {
        let server = Arc::new(self);

        // Create a temp socket file, set permissions and move it to the correct location.
        let mut socket_path_tmp = server.socket_path.clone();
        socket_path_tmp.set_extension("tmp");

        let _ = std::fs::remove_file(&socket_path_tmp);
        let _ = std::fs::remove_file(&server.socket_path);

        let listener = UnixListener::bind(&socket_path_tmp).unwrap();

        let mut perms = fs::metadata(&socket_path_tmp).unwrap().permissions();
        perms.set_mode(server.socket_permission);
        fs::set_permissions(&socket_path_tmp, perms).unwrap();
        fs::rename(socket_path_tmp, &server.socket_path).unwrap();

        info!("Listening on: {:?}", server.socket_path);

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        info!("accepted connection on {:?}", server.socket_path);
                        let connection_state = (server.connection_handler)(&server.state);
                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server.start_connection(stream, connection_state).await
                            {
                                info!("closing connection: {e:?}");
                            }
                        });
                    }
                    Err(e) => {
                        warn!("accept() on {:?} failed: {e}", server.socket_path);
                    }
                }
            }
        });
    }

    /// returns (message_type, struct_array, struct_len, payload_vec)
    async fn get_next_request<R: AsyncReadExt + Unpin>(
        input: &mut R,
    ) -> tokio::io::Result<(MessageType, [u8; MAX_STRUCT_LEN], usize, AlignedVec)> {
        let header = MessageHeader::read(input).await?;

        super_trace!("got request header: {:?}", header);
        if header.struct_len as usize > MAX_STRUCT_LEN {
            panic!(
                "got invalid struct length {} ({:#x})",
                header.struct_len, header.struct_len
            );
        }
        if header.payload_len > *UNREASONABLE_REQUEST_SIZE {
            panic!(
                "got unreasonable payload length {} ({:#x})",
                header.payload_len, header.payload_len
            );
        }

        let mut struct_array: [u8; MAX_STRUCT_LEN] = [0; MAX_STRUCT_LEN];
        let struct_slice = &mut struct_array[..header.struct_len as usize];
        input.read_exact(struct_slice).await?;

        let payload_len = header.payload_len as usize;
        let mut payload_vec = with_alloctag_hf("get_next_request()", || {
            // XXX hardcoded 512; should be based on zettacache sector size
            AlignedVec::with_capacity(payload_len, 512)
        });
        read_buf_exact_len(input, &mut payload_vec, payload_len).await?;

        Ok((
            header.message_type,
            struct_array,
            header.struct_len as usize,
            payload_vec,
        ))
    }

    fn version_to_nvlist(version: &Version) -> NvList {
        let mut version_nvl = NvList::new_unique_names();
        version_nvl.insert("major", &version.major).unwrap();
        version_nvl.insert("minor", &version.minor).unwrap();
        version_nvl.insert("patch", &version.patch).unwrap();
        version_nvl
    }

    async fn negotiate_version<R: AsyncReadExt + Unpin>(
        versions: &[Version],
        responder: Responder,
        input: &mut R,
    ) -> Result<Version> {
        let (request_type, _, struct_len, payload_vec) = Self::get_next_request(input).await?;

        if request_type != MessageType::NvList {
            return Err(anyhow!("Negotiation failed, non-nvlist request received"));
        }

        assert_eq!(struct_len, 0);
        let nvl = NvList::try_unpack(payload_vec.as_slice()).unwrap();
        let request_type_cstr = nvl.lookup_string(AGENT_REQUEST_TYPE)?;
        let request_type = request_type_cstr.to_str()?;
        if request_type != TYPE_VERSION {
            return Err(anyhow!("Negotiation failed, no version request received"));
        }
        let version_req_string = nvl.lookup_string("version")?.into_string()?;
        let version_req = VersionReq::parse(&version_req_string)?;
        for version in versions.iter().rev() {
            if version_req.matches(version) {
                let mut response = NvList::new_unique_names();
                response.insert(AGENT_RESPONSE_TYPE, TYPE_VERSION)?;
                response.insert("version", Self::version_to_nvlist(version).as_ref())?;
                responder.respond_with_nvlist(response);
                return Ok(version.clone());
            }
        }
        Err(anyhow!("No compatible versions detected: {}", version_req))
    }

    async fn start_connection(&self, stream: UnixStream, mut state: Cs) -> Result<()> {
        let (input, output) = stream.into_split();

        let mut input = BufReader::with_capacity(1024 * 1024, input);

        let (error_tx, mut error_rx) = mpsc::channel(1);

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(Responder::response_task(output, rx, error_tx.clone()));
        let responder = Responder::new(tx);

        let version =
            Self::negotiate_version(&self.version_list, responder.clone(), &mut input).await?;
        info!("Version selected for connection: {:?}", version);
        state.set_version(version);

        loop {
            let (request_type, struct_array, struct_len, payload_vec) = select! {
                Some(e) = error_rx.recv() => {
                    // an async (spawned) task produced an error
                    return Err(e);
                },
                /*
                 * While get_next_request isn't cancellation safe, this is OK because any time it is
                 * cancelled we're going to terminate the connection, and won't resume the
                 * get_next_request call.
                 */
                ret = Self::get_next_request(&mut input) => ret
            }?;
            let struct_slice = &struct_array[..struct_len];

            if request_type == MessageType::NvList {
                assert_eq!(struct_slice.len(), 0);
                let nvl = NvList::try_unpack(payload_vec.as_slice()).unwrap();
                super_trace!("got nvlist request {:?}", nvl);
                let request_type_cstr =
                    with_alloctag_hf("Server::start_connection() NvList::lookup_string()", || {
                        nvl.lookup_string(AGENT_REQUEST_TYPE)
                    })?;
                let request_type = request_type_cstr.to_str()?;
                match self.nvlist_handlers.get(request_type) {
                    Some(HandlerEnum::Serial(handler)) => {
                        let response_opt = handler(&mut state, nvl).await?;
                        if let Some(response) = response_opt {
                            responder.respond_with_nvlist(response);
                        }
                    }
                    Some(HandlerEnum::Concurrent(handler)) => {
                        let fut = handler(&mut state, nvl)?;
                        let error_tx = error_tx.clone();
                        let responder = responder.clone();
                        tokio::spawn(async move {
                            match fut.await {
                                Ok(Some(response)) => {
                                    responder.respond_with_nvlist(response);
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    error_tx.send(e).await.unwrap();
                                }
                            }
                        });
                    }
                    None => {
                        return Err(anyhow!("bad type {:?} in request {:?}", request_type, nvl));
                    }
                }
            } else {
                super_trace!("got struct request type {:?}", request_type);
                match self.struct_handlers.get(&request_type) {
                    Some(handler) => {
                        handler(&mut state, responder.clone(), struct_slice, payload_vec)?;
                    }
                    None => {
                        error!("bad request type {:?}", request_type);
                        return Err(anyhow!("bad request type {:?}", request_type));
                    }
                }
            }
        }
    }
}

struct ResponseMessage {
    message_type: MessageType,
    struct_array: [u8; MAX_STRUCT_LEN],
    struct_len: usize,
    payload: Bytes,
}

#[derive(Clone)]
pub struct Responder {
    tx: mpsc::UnboundedSender<ResponseMessage>,
}

impl Responder {
    fn new(tx: mpsc::UnboundedSender<ResponseMessage>) -> Self {
        Self { tx }
    }

    pub fn respond_with_struct<T: ReprC + Debug>(
        &self,
        message_type: MessageType,
        struct_ref: &T,
        payload: Bytes,
    ) {
        super_trace!(
            "sending {:?} {:?} payload={} bytes",
            message_type,
            struct_ref,
            payload.len()
        );
        let struct_slice = struct_to_slice(struct_ref);
        self.send_response(message_type, struct_slice, payload)
    }

    pub fn respond_with_nvlist(&self, nvl: NvList) {
        let buf = with_alloctag_hf("Server::send_response() NvList.pack()", || {
            nvl.pack(NvEncoding::Native).unwrap()
        });
        self.send_response(MessageType::NvList, &[0; 0], buf.into())
    }

    fn send_response(&self, message_type: MessageType, struct_slice: &[u8], payload: Bytes) {
        let mut struct_array: [u8; MAX_STRUCT_LEN] = [0; MAX_STRUCT_LEN];
        struct_array[..struct_slice.len()].copy_from_slice(struct_slice);

        self.tx
            .send(ResponseMessage {
                message_type,
                struct_array,
                struct_len: struct_slice.len(),
                payload,
            })
            .unwrap_or_else(|e| panic!("couldn't send: {}", e));
    }

    async fn write_response<W: AsyncWriteExt + Unpin>(
        output: &mut W,
        message: ResponseMessage,
    ) -> Result<()> {
        let struct_slice = &message.struct_array[..message.struct_len];
        let header = MessageHeader {
            message_type: message.message_type,
            struct_len: u32::try_from(struct_slice.len()).unwrap(),
            payload_len: u32::try_from(message.payload.len()).unwrap(),
        };

        super_trace!("sending response {:?}", header);

        header.write(output).await?;
        if !struct_slice.is_empty() {
            output.write_all(struct_slice).await?;
        }
        if !message.payload.is_empty() {
            output.write_all(&message.payload).await?;
        }
        Ok(())
    }

    async fn response_task_impl<W: AsyncWriteExt + Unpin>(
        output: &mut W,
        rx: &mut mpsc::UnboundedReceiver<ResponseMessage>,
    ) -> Result<()> {
        while let Some(message) = measure!("Responder::response_task() recv")
            .fut(rx.recv())
            .await
        {
            let m = measure!("Responder::response_task() write_response");
            m.fut(Self::write_response(output, message)).await?;

            // drain the channel before flushing
            while let Some(Some(message)) = rx.recv().now_or_never() {
                m.fut(Self::write_response(output, message)).await?;
            }

            measure!("Responder::response_task() flush")
                .fut(output.flush())
                .await?;
        }
        Ok(())
    }

    fn response_task(
        output: OwnedWriteHalf,
        rx: mpsc::UnboundedReceiver<ResponseMessage>,
        error_tx: mpsc::Sender<Error>,
    ) -> impl Future<Output = ()> {
        let output = BufWriter::with_capacity(1024 * 1024, output);

        // It would improve readability if we used a struct rather than a tuple for the state we
        // are saving in the DebugPointerSet.  However, the debugger can't cast to a struct type
        // defined here, because the fully-qualified type name would contain {braces}.  The
        // alternative would be to declare the struct at the top level, but having the internal
        // details of this method spread to the surrounding state seems worse than the tuple.
        //
        // Similarly, declaring RESPOND_RECEIVERS inside an async closure or function would cause
        // its fully-qualified symbol name to contain `{{closure}}`, so we couldn't name it in
        // the debugger.
        lazy_static_ptr! {
            static ref RESPOND_RECEIVERS:
                DebugPointerSet<(
                    BufWriter<OwnedWriteHalf>,
                    mpsc::UnboundedReceiver<ResponseMessage>,
                )> = Default::default();
        }

        // Save our rx and output in the global debug state, so that we can find them from the
        // debugger.
        let mut state = RESPOND_RECEIVERS.insert((output, rx));

        async move {
            // destructure the tuple back into the rx/output
            let (output, rx) = &mut *state;
            if let Err(e) = Self::response_task_impl(output, rx).await {
                error_tx.send(e).await.ok();
            }
        }
    }
}

/// Helper function to produce the value that should be returned from a handler
/// that does not need to do any async work.
pub fn handler_return_ok(response: Option<NvList>) -> HandlerReturn {
    Ok(Box::pin(future::ready(Ok(response))))
}

#[derive(Serialize)]
#[serde(tag = "err")]
pub enum FailureMessage {
    Other { message: String },
}
impl Debug for FailureMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            FailureMessage::Other { message } => write!(f, "{}", message),
        }
    }
}
impl FailureMessage {
    pub fn new(error: Error) -> Self {
        FailureMessage::Other {
            // Note that we use Debug formatting so that the cause/context of the anyhow::Error
            // will be included.
            message: format!("{error:?}"),
        }
    }
}

/// Create and return an NvList appropriate to use as a response to the client.
///
/// For extensibility and consistency, E should be an enum with `#[serde(tag =
/// "err")]`.  This way all failure responses will have a pair "err" ->
/// "EnumVariantName".  All variants of E should be struct-like or unit-like
/// (not tuple-like).  FailureMessage is an example.
///
/// The response nvlist will have the following nvpairs:
/// * "response_type" -> response_type (string)
/// * fields from R
/// * if result.is_ok(), fields from O
/// * if result.is_err(), "err" -> EnumVariantName (string)
/// * if result.is_err(), "errstr" -> stringified error (string)
/// * if result.is_err(), fields from the varant of E
pub fn return_result<R, O, E>(
    response_type: &str,
    request_id: R,
    result: Result<O, E>,
    debug: bool,
) -> Result<Option<NvList>>
where
    R: Debug + Serialize,
    O: Debug + Serialize,
    E: Debug + Serialize,
{
    #[derive(Debug, Serialize)]
    struct Response<'a, R, O, E> {
        response_type: &'a str,
        #[serde(flatten)]
        request: R,
        #[serde(flatten)]
        ok: Option<O>,
        #[serde(flatten)]
        err: Option<E>,
        errstr: Option<String>,
    }

    if let Err(e) = &result {
        error!("sending failure: {:?}", e);
    }

    let (ok, errstr, err) = match result {
        Ok(o) => (Some(o), None, None),
        Err(e) => (None, Some(format!("{:?}", e)), Some(e)),
    };

    let response = Response {
        response_type,
        request: request_id,
        ok,
        err,
        errstr,
    };

    if debug {
        trace!("sending response: {:?}", response);
    } else {
        super_trace!("sending response: {:?}", response);
    }

    let nvl = nvpair::to_nvlist(&response)?;

    // The type E should be an enum with `#[serde(tag = "err")]`.
    // This ensures that all failures have the "err" nvpair present.
    if response.err.is_some() {
        assert!(nvl.exists("err"));
    }

    if debug {
        maybe_die_with(|| format!("before sending response: {:?}", nvl));
        debug!("sending response nvl: {:?}", nvl);
    } else {
        super_trace!("sending response nvl: {:?}", nvl);
    }
    Ok(Some(nvl))
}

pub fn return_ok<O>(response_type: &str, response: O, debug: bool) -> Result<Option<NvList>>
where
    O: Debug + Serialize,
{
    return_result(response_type, (), Ok::<_, ()>(response), debug)
}
