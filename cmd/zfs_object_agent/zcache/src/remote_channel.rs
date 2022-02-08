use anyhow::{anyhow, Result};
use log::*;
use nvpair::{NvEncoding, NvList};
use semver::Version;
use std::thread::sleep;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use util::writeln_stderr;
use util::From64;

#[derive(Debug)]
pub enum RemoteError {
    ResultError(NvList),
    Other(anyhow::Error),
}

const ZOA_MAX_RETRIES: usize = 30;

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RemoteError::ResultError(list) => write!(f, "Remote error: {:?}", list),
            RemoteError::Other(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for RemoteError {}

impl From<anyhow::Error> for RemoteError {
    fn from(e: anyhow::Error) -> Self {
        RemoteError::Other(e)
    }
}

impl From<std::io::Error> for RemoteError {
    fn from(e: std::io::Error) -> Self {
        RemoteError::Other(anyhow!(e))
    }
}

impl From<std::str::Utf8Error> for RemoteError {
    fn from(e: std::str::Utf8Error) -> Self {
        RemoteError::Other(anyhow!(e))
    }
}

pub struct RemoteChannel {
    stream: UnixStream,
    socket_path: String,
    pub version: Version,
}

impl RemoteChannel {
    async fn open(socket_path: &str) -> Result<UnixStream> {
        // Retry until a connection is established
        // TODO for the initial command launch can we shorten this?
        let mut reconnect_retries = 0;
        loop {
            match UnixStream::connect(socket_path).await {
                Ok(stream) => {
                    info!("opened socket {}", socket_path);
                    return Ok(stream);
                }
                Err(e) => {
                    if reconnect_retries > ZOA_MAX_RETRIES {
                        info!(
                            "cannot connect after {} attempts to zfs object agent {}",
                            reconnect_retries,
                            e.to_string()
                        );
                        writeln_stderr!("cannot connect to zfs object agent");
                        return Err(anyhow!(e));
                    }
                    info!("open socket failed {}", e.to_string());
                    sleep(Duration::from_millis(500));
                    reconnect_retries += 1;
                    continue;
                }
            }
        }
    }

    /// Create a new RemoteChannel and establish a remote connection to the object agent.
    pub async fn new(need_priv: bool) -> Result<Self> {
        let socket_path = if need_priv {
            "/etc/zfs/zfs_root_socket".to_string()
        } else {
            "/etc/zfs/zfs_public_socket".to_string()
        };

        let mut stream = RemoteChannel::open(&socket_path).await?;
        let mut vers_req_nvlist = NvList::new_unique_names();
        vers_req_nvlist.insert("Type", "version")?;
        vers_req_nvlist.insert("version", "^1")?;
        Self::send(&mut stream, vers_req_nvlist).await?;
        let response = Self::receive(&mut stream).await?;
        assert!(response.lookup_string("Type")?.to_str() == Ok("version"));
        let vers_nvl = response.lookup_nvlist("version")?;
        let version = Version::new(
            vers_nvl.lookup_uint64("major")?,
            vers_nvl.lookup_uint64("minor")?,
            vers_nvl.lookup_uint64("patch")?,
        );

        Ok(Self {
            stream,
            socket_path,
            version,
        })
    }

    async fn send(stream: &mut UnixStream, message: NvList) -> Result<()> {
        // convert to packed nvlist and send...
        let buf = message.pack(NvEncoding::Native).unwrap();
        let len64 = buf.len() as u64;
        stream.write_u64_le(len64).await?;
        stream.write_all(buf.as_slice()).await?;
        Ok(())
    }

    async fn receive(stream: &mut UnixStream) -> Result<NvList> {
        // receive a packed nvlist and unpack it...
        let len64 = stream.read_u64_le().await?;
        let mut v: Vec<u8> = vec![0; usize::from64(len64)];
        stream.read_exact(v.as_mut()).await?;
        Ok(NvList::try_unpack(v.as_ref()).unwrap())
    }

    /// Send a request to the object agent and wait for a response. If the agent is restarted
    /// before completing this request, it will reconnect and resend the request. Therefore,
    /// the request must be idempotent (i.e. executing the request more than once has the same
    /// effect as executing it only once).
    pub async fn call(
        &mut self,
        request: &str,
        args: Option<NvList>,
    ) -> Result<NvList, RemoteError> {
        loop {
            // send request, retrying as needed
            let mut nvlist = args.clone().unwrap_or_else(NvList::new_unique_names);
            nvlist.insert("Type", request).unwrap();
            match Self::send(&mut self.stream, nvlist).await {
                Ok(_) => {}
                Err(e) => {
                    // reopen the channel and resend the request
                    info!("send: object agent restarted: {}", e);
                    self.stream = RemoteChannel::open(&self.socket_path).await?;
                    continue;
                }
            }
            debug!("sent {} request, now waiting for response...", request);

            // receive response, retrying as needed
            let response: NvList = match Self::receive(&mut self.stream).await {
                Ok(response) => response,
                Err(e) => {
                    // reopen the channel and resend the request
                    info!("receive: object agent restarted: {}", e);
                    self.stream = RemoteChannel::open(&self.socket_path).await?;
                    continue;
                }
            };
            debug!("received response: {:?}", response);

            let response_type = response.lookup_string("Type")?;
            let response_type = response_type.to_str()?;
            if response_type != request {
                return Err(RemoteError::Other(anyhow!(
                    "expected response type \"{}\", got \"{}\"",
                    request,
                    response_type
                )));
            }

            let result = response.lookup_string("result")?;
            let result = result.to_str()?;
            return match result {
                "ok" => Ok(response),
                "err" => Err(RemoteError::ResultError(response)),
                _ => Err(RemoteError::Other(anyhow!(
                    "expected \"ok\" or \"err\" for result, got \"{}\"",
                    result
                ))),
            };
        }
    }
}
