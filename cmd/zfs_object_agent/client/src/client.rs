// This file is not used in production.
#![allow(clippy::print_stderr)]
#![allow(clippy::print_stdout)]

use nvpair::NvEncoding;
use nvpair::NvList;
use nvpair::NvListRef;
use semver::Version;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::unix::OwnedWriteHalf;
use tokio::task::JoinHandle;
use util::message::*;
use zettacache::base_types::*;
use zettaobject::base_types::*;

pub struct Client {
    input: Option<OwnedReadHalf>,
    output: OwnedWriteHalf,
    pub version: Version,
}

impl Client {
    pub async fn connect() -> Client {
        let s = tokio::net::UnixStream::connect("/etc/zfs/zfs_root_socket")
            .await
            .unwrap();

        let (mut r, mut w) = s.into_split();

        let mut vers_req_nvlist = NvList::new_unique_names();
        vers_req_nvlist
            .insert(AGENT_REQUEST_TYPE, TYPE_VERSION)
            .unwrap();
        vers_req_nvlist.insert("version", "^1").unwrap();
        Self::send_request_impl(&mut w, vers_req_nvlist.as_ref()).await;
        let response = Self::get_next_response_impl(&mut r).await;
        assert!(
            response
                .lookup_string(AGENT_RESPONSE_TYPE)
                .unwrap()
                .to_str()
                == Ok(TYPE_VERSION)
        );
        let vers_nvl = response.lookup_nvlist("version").unwrap();
        let version = Version::new(
            vers_nvl.lookup_uint64("major").unwrap(),
            vers_nvl.lookup_uint64("minor").unwrap(),
            vers_nvl.lookup_uint64("patch").unwrap(),
        );

        Client {
            input: Some(r), // None while get_responses_initiate() is running
            output: w,
            version,
        }
    }

    async fn get_next_response_impl(input: &mut OwnedReadHalf) -> NvList {
        let header = MessageHeader::read(input).await.unwrap();
        let mut v = Vec::new();
        v.resize(header.payload_len as usize, 0);
        input.read_exact(v.as_mut()).await.unwrap();
        let nvl = NvList::try_unpack(v.as_ref()).unwrap();
        println!("got response: {:?}", nvl);
        nvl
    }

    pub async fn get_next_response(&mut self) -> NvList {
        Self::get_next_response_impl(self.input.as_mut().unwrap()).await
    }

    /// Only one of these can be running at a time; call get_responses_join() on
    /// the returned value to wait.
    pub fn get_responses_initiate(&mut self, num: usize) -> JoinHandle<OwnedReadHalf> {
        let mut input = self.input.take().unwrap();
        tokio::spawn(async move {
            for _ in 0..num {
                Self::get_next_response_impl(&mut input).await;
            }
            input
        })
    }

    // If we wanted to get fancy, this could return a Vec<NvList> of the responses
    pub async fn get_responses_join(&mut self, handle: JoinHandle<OwnedReadHalf>) {
        self.input = Some(handle.await.unwrap());
    }

    async fn send_request_impl(output: &mut OwnedWriteHalf, nvl: &NvListRef) {
        println!("sending request: {:?}", nvl);
        let buf = nvl.pack(NvEncoding::Native).unwrap();

        MessageHeader::new_nvlist(buf.len())
            .write(output)
            .await
            .unwrap();
        output.write_all(buf.as_ref()).await.unwrap();
    }

    async fn send_request(&mut self, nvl: &NvListRef) {
        Self::send_request_impl(&mut self.output, nvl).await
    }

    pub async fn create_pool(
        &mut self,
        region: &str,
        endpoint: &str,
        bucket_name: &str,
        guid: PoolGuid,
        name: &str,
    ) {
        let mut nvl = NvList::new_unique_names();

        nvl.insert(AGENT_REQUEST_TYPE, TYPE_CREATE_POOL).unwrap();
        nvl.insert("region", region).unwrap();
        nvl.insert("endpoint", endpoint).unwrap();
        nvl.insert("bucket", bucket_name).unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        nvl.insert("name", name).unwrap();

        self.send_request(nvl.as_ref()).await;
    }

    pub async fn open_pool(
        &mut self,
        region: &str,
        endpoint: &str,
        bucket_name: &str,
        guid: PoolGuid,
    ) {
        let mut nvl = NvList::new_unique_names();

        nvl.insert(AGENT_REQUEST_TYPE, TYPE_OPEN_POOL).unwrap();
        nvl.insert("region", region).unwrap();
        nvl.insert("endpoint", endpoint).unwrap();
        nvl.insert("bucket", bucket_name).unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        self.send_request(nvl.as_ref()).await;
    }

    // XXX dead code?
    pub async fn read_block(&mut self, guid: PoolGuid, block: BlockId) {
        let mut nvl = NvList::new_unique_names();
        nvl.insert("request_type", "read block").unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        nvl.insert("block", &block.0).unwrap();
        nvl.insert("request_id", &1234u64).unwrap();
        self.send_request(nvl.as_ref()).await;
    }

    // XXX dead code?
    pub async fn write_block(&mut self, guid: PoolGuid, block: BlockId, data: &[u8]) {
        let mut nvl = NvList::new_unique_names();
        nvl.insert("request_type", "write block").unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        nvl.insert("block", &block.0).unwrap();
        nvl.insert("data", data).unwrap();
        self.send_request(nvl.as_ref()).await;
    }

    // XXX dead code?
    pub async fn free_block(&mut self, guid: PoolGuid, block: BlockId) {
        let mut nvl = NvList::new_unique_names();
        nvl.insert("request_type", "free block").unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        nvl.insert("block", &block.0).unwrap();
        self.send_request(nvl.as_ref()).await;
    }

    pub async fn begin_txg(&mut self, guid: PoolGuid, txg: Txg) {
        let mut nvl = NvList::new_unique_names();
        nvl.insert(AGENT_REQUEST_TYPE, TYPE_BEGIN_TXG).unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        nvl.insert("txg", &txg.0).unwrap();
        self.send_request(nvl.as_ref()).await;
    }

    pub async fn end_txg(&mut self, guid: PoolGuid, uberblock: &[u8]) {
        let mut nvl = NvList::new_unique_names();
        nvl.insert(AGENT_REQUEST_TYPE, TYPE_END_TXG).unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        nvl.insert("data", uberblock).unwrap();
        self.send_request(nvl.as_ref()).await;
    }

    pub async fn flush_writes(&mut self, guid: PoolGuid) {
        let mut nvl = NvList::new_unique_names();
        nvl.insert(AGENT_REQUEST_TYPE, TYPE_FLUSH_WRITES).unwrap();
        nvl.insert("guid", &guid.0).unwrap();
        self.send_request(nvl.as_ref()).await;
    }
}
