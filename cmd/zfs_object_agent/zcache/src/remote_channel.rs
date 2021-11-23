use anyhow::{anyhow, Context, Result};
use log::*;
use nvpair::{NvEncoding, NvList};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use util::From64;

#[derive(Debug)]
pub enum RemoteError {
    ResultError(NvList),
    Other(anyhow::Error),
}

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
}

impl RemoteChannel {
    pub async fn new(need_priv: bool) -> Result<Self> {
        let socket_path = if need_priv {
            "/etc/zfs/zfs_root_socket"
        } else {
            "/etc/zfs/zfs_public_socket"
        };
        // XXX - do we need retrys here?
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("Could not connect to {}", socket_path))?;
        Ok(Self { stream })
    }

    async fn send(&mut self, message: NvList) -> Result<()> {
        // convert to packed nvlist and send...
        let buf = message.pack(NvEncoding::Native).unwrap();
        let len64 = buf.len() as u64;
        self.stream.write_u64_le(len64).await?;
        self.stream.write_all(buf.as_slice()).await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<NvList> {
        // recieve a packed nvlist and unpack it...
        let len64 = self.stream.read_u64_le().await?;
        let mut v: Vec<u8> = vec![0; usize::from64(len64)];
        self.stream.read_exact(v.as_mut()).await?;
        Ok(NvList::try_unpack(v.as_ref()).unwrap())
    }

    pub async fn call(
        &mut self,
        request: &str,
        args: Option<NvList>,
    ) -> Result<NvList, RemoteError> {
        // send request
        let mut nvlist = args.unwrap_or_else(NvList::new_unique_names);
        nvlist.insert("Type", request).unwrap();
        self.send(nvlist).await?;
        debug!("sent {} request, now waiting for response...", request);
        // receive response
        let response = self.receive().await?;
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
        match result {
            "ok" => Ok(response),
            "err" => Err(RemoteError::ResultError(response)),
            _ => Err(RemoteError::Other(anyhow!(
                "expected \"ok\" or \"err\" for result, got \"{}\"",
                result
            ))),
        }
    }
}
