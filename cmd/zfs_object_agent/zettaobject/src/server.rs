//! This module provides a "server" which accepts and manages connections on a
//! unix-domain socket, using serialized nvlists to encode requests and
//! responses.  Request handlers are registered with `register_handler()` (for
//! operations that are processed concurrently, but don't concurrently modify
//! the connection's shared state), or `register_serial_handler()` (for
//! operations that "block the world" while they are being processed, and can
//! modify the connection-specific state).  See the method-level documentation
//! for more details.

use anyhow::anyhow;
use anyhow::Result;
use bytes::Bytes;
use futures::{future, Future, FutureExt};
use lazy_static::lazy_static;
use log::*;
use nvpair::{NvEncoding, NvList};
use safer_ffi::prelude::*;
use semver::Version;
use semver::VersionReq;
use std::collections::HashMap;
use std::fmt::Debug;
use std::fs;
use std::os::unix::prelude::PermissionsExt;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::io::BufWriter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use util::get_tunable;
use util::message::struct_to_slice;
use util::message::MessageHeader;
use util::message::MessageType;
use util::message::MAX_STRUCT_LEN;
use util::super_trace;
use util::with_alloctag_hf;
use util::AlignedVec;

lazy_static! {
    // max zfs block size is 16MB
    pub static ref UNREASONABLE_REQUEST_SIZE: u32 =
        get_tunable("unreasonable_request_size", 20_000_000);
}

// Ss: ServerState (consumer's state associated with the server)
// Cs: ConnectionState (consumer's state associated with the connection)
pub struct Server<Ss, Cs> {
    socket_path: String,
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
        socket_path: &str,
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
    /// When a connection receives a request with "Type" = request_type, the
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
        let socket_path_tmp = format!("{}.tmp", server.socket_path);

        let _ = std::fs::remove_file(&socket_path_tmp);
        let _ = std::fs::remove_file(&server.socket_path);

        let listener = UnixListener::bind(&socket_path_tmp).unwrap();

        let mut perms = fs::metadata(&socket_path_tmp).unwrap().permissions();
        perms.set_mode(server.socket_permission);
        fs::set_permissions(&socket_path_tmp, perms).unwrap();
        fs::rename(socket_path_tmp, &server.socket_path).unwrap();

        info!("Listening on: {}", server.socket_path);

        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        info!("accepted connection on {}", server.socket_path);
                        let connection_state = (server.connection_handler)(&server.state);
                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server.start_connection(stream, connection_state).await
                            {
                                error!("closing connection due to error: {:?}", e);
                            }
                        });
                    }
                    Err(e) => {
                        warn!("accept() on {} failed: {}", server.socket_path, e);
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

        let mut payload_vec = with_alloctag_hf("get_next_request()", || {
            // XXX hardcoded 512; should be based on zettacache sector size
            AlignedVec::with_capacity(header.payload_len as usize, 512)
        });
        // XXX Would be nice if we didn't have to zero it out.
        // probably need to use OwnedReadHalf::try_read_buf()?
        payload_vec.resize(header.payload_len as usize);
        input.read_exact(payload_vec.as_mut_slice()).await?;

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
        let request_type_cstr = nvl.lookup_string("Type")?;
        let request_type = request_type_cstr.to_str()?;
        if request_type != "version" {
            return Err(anyhow!("Negotiation failed, no version request received"));
        }
        let version_req_string = nvl.lookup_string("version")?.into_string()?;
        let version_req = VersionReq::parse(&version_req_string)?;
        for version in versions.iter().rev() {
            if version_req.matches(version) {
                let mut response = NvList::new_unique_names();
                response.insert("Type", "version")?;
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

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move { Responder::response_task(output, rx).await });

        let responder = Responder::new(tx);

        let version =
            Self::negotiate_version(&self.version_list, responder.clone(), &mut input).await?;
        info!("Version selected for connection: {:?}", version);
        state.set_version(version);

        let (error_tx, mut error_rx) = mpsc::channel(1);

        loop {
            if let Some(Some(e)) = error_rx.recv().now_or_never() {
                // an async (spawned) task produced an error
                return Err(e);
            }
            let (request_type, struct_array, struct_len, payload_vec) =
                Self::get_next_request(&mut input).await?;
            let struct_slice = &struct_array[..struct_len];

            if request_type == MessageType::NvList {
                assert_eq!(struct_slice.len(), 0);
                let nvl = NvList::try_unpack(payload_vec.as_slice()).unwrap();
                super_trace!("got nvlist request {:?}", nvl);
                let request_type_cstr =
                    with_alloctag_hf("Server::start_connection() NvList::lookup_string()", || {
                        nvl.lookup_string("Type")
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

    async fn write_response<W: AsyncWriteExt + Unpin>(output: &mut W, message: ResponseMessage) {
        let struct_slice = &message.struct_array[..message.struct_len];
        let header = MessageHeader {
            message_type: message.message_type,
            struct_len: u32::try_from(struct_slice.len()).unwrap(),
            payload_len: u32::try_from(message.payload.len()).unwrap(),
        };

        super_trace!("sending response {:?}", header);

        header.write(output).await.unwrap();
        if !struct_slice.is_empty() {
            output.write_all(struct_slice).await.unwrap();
        }
        if !message.payload.is_empty() {
            output.write_all(&message.payload).await.unwrap();
        }
    }

    async fn response_task(
        output: OwnedWriteHalf,
        mut rx: mpsc::UnboundedReceiver<ResponseMessage>,
    ) {
        let mut output = BufWriter::with_capacity(1024 * 1024, output);
        while let Some(message) = rx.recv().await {
            Self::write_response(&mut output, message).await;

            // drain the channel before flushing
            while let Some(Some(message)) = rx.recv().now_or_never() {
                Self::write_response(&mut output, message).await;
            }

            output.flush().await.unwrap();
        }
    }
}

/// Helper function to produce the value that should be returned from a handler
/// that does not need to do any async work.
pub fn handler_return_ok(response: Option<NvList>) -> HandlerReturn {
    Ok(Box::pin(future::ready(Ok(response))))
}
