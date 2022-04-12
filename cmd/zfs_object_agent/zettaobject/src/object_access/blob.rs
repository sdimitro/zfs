use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt::Display;
use std::fs;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use azure_core::HttpError;
use azure_storage::clients::AsStorageClient;
use azure_storage::clients::StorageAccountClient;
use azure_storage::clients::StorageClient;
use azure_storage_blobs::prelude::AsBlobClient;
use azure_storage_blobs::prelude::AsContainerClient;
use azure_storage_blobs::prelude::ContainerClient;
use bytes::Bytes;
use bytes::BytesMut;
use enum_map::EnumMap;
use futures::Stream;
use futures::StreamExt;
use http::Response;
use http::StatusCode;
use log::debug;
use log::trace;
use more_asserts::assert_le;
use rusoto_core::ByteStream;
use tokio::io::AsyncReadExt;

use super::retry;
use super::RequestError;
use super::OBJECT_DELETION_BATCH_SIZE;
use crate::access_stats::ObjectAccessOpType;
use crate::access_stats::ObjectAccessStats;
use crate::access_stats::OutstandingOps;
use crate::access_stats::StatMapValue;
use crate::object_access::GetError;
use crate::object_access::OAError;
use crate::object_access::ObjectAccessTrait;
use crate::object_access::ObjectStat;
use crate::object_access::ObjectStoreError;
use crate::object_access::PutError;

/// MaybeFrom is basically just TryFrom that restricts the Err type to be the
/// From type. This allows us to consume the from value on success, and return it
/// on failure. This could probably also be done using TryFrom in combination with
/// GAT, once that feature is in stable.
pub trait MaybeFrom<F>: Sized {
    fn maybe_from(from: F) -> core::result::Result<Self, F>;
}

impl MaybeFrom<HttpError> for GetError {
    fn maybe_from(value: HttpError) -> Result<Self, HttpError> {
        match value {
            HttpError::StatusCode {
                status: StatusCode::NOT_FOUND,
                body,
            } => Ok(Self(format!("No such key: {}", body))),
            HttpError::StatusCode {
                status: StatusCode::CONFLICT,
                body,
            } => Ok(Self(format!("Invalid state: {}", body))),
            HttpError::StatusCode { status: _, body: _ } => Err(value),
            HttpError::ExecuteRequest(_) => todo!(),
            _ => Err(value),
        }
    }
}

impl MaybeFrom<HttpError> for PutError {
    fn maybe_from(value: HttpError) -> Result<Self, HttpError> {
        match value {
            HttpError::StatusCode { status, body: _ } if status.is_client_error() => {
                Ok(PutError {})
            }
            _ => Err(value),
        }
    }
}

impl MaybeFrom<HttpError> for ObjectStoreError {
    fn maybe_from(value: HttpError) -> Result<Self, HttpError> {
        match value {
            HttpError::StatusCode {
                status: StatusCode::NOT_FOUND,
                body: _,
            } => Ok(ObjectStoreError("No such key".to_string())),
            HttpError::StatusCode { status: _, body } => Ok(ObjectStoreError(body)),
            HttpError::ExecuteRequest(_) => todo!(),
            _ => Err(value),
        }
    }
}

impl<E> From<HttpError> for RequestError<E>
where
    E: MaybeFrom<HttpError> + Display,
{
    fn from(e: azure_core::HttpError) -> Self {
        match e {
            HttpError::StatusCode { status, body: _ } => {
                if status == StatusCode::FORBIDDEN {
                    return Self::InvalidCredentials;
                }
                /*
                 * XXX we need logic here to handle contentful errors that aren't
                 * specific to E, like credential and validation issues.
                 */
                match E::maybe_from(e) {
                    Ok(err) => Self::Service(err),
                    Err(HttpError::StatusCode { status, body }) => Self::Unknown(
                        Response::builder()
                            .status(status)
                            .body(Bytes::from(body))
                            .unwrap(),
                    ),
                    Err(_) => panic!("Type changed during maybe_from"),
                }
            }
            HttpError::Utf8(err) => Self::InternalError(err.to_string()),
            /*
             * XXX Long term, we probably want to handle this, but for now this error
             * probably only happens if we mess up our code for building requests. Panic
             * to make it easier to debug and develop.
             */
            HttpError::BuildClientRequest(err) => panic!("{}", err),
            HttpError::ExecuteRequest(err) => Self::InternalError(err.to_string()),
            HttpError::ReadBytes(err) => Self::InternalError(err.to_string()),
            HttpError::BuildResponse(err) => Self::InternalError(err.to_string()),
            /*
             * XXX Long term, we probably want to handle this, but for now this error
             * probably only happens if we mess up our code for building requests. Panic
             * to make it easier to debug and develop.
             */
            HttpError::StreamReset(err) => panic!("{}", err),
            _ => todo!(),
        }
    }
}
impl<E> From<HttpError> for OAError<E>
where
    E: MaybeFrom<HttpError> + Display,
{
    fn from(e: HttpError) -> Self {
        Self::RequestError(RequestError::from(e))
    }
}

pub struct BlobObjectAccess {
    container_client: Arc<ContainerClient>,
    bucket_str: String,
    credentials_profile: Option<String>,
    access_stats: ObjectAccessStats,
    outstanding_ops: EnumMap<ObjectAccessOpType, OutstandingOps>,
}

impl BlobObjectAccess {
    async fn get_azure_storage_client_from_env() -> Result<Arc<StorageClient>, Box<dyn Error>> {
        let http_client = azure_core::new_http_client();
        let storage_client = match env::var("AZURE_CONNECTION_STRING") {
            Ok(connection_string) => StorageAccountClient::new_connection_string(
                http_client.clone(),
                &connection_string,
            )?
            .as_storage_client(),
            Err(_) => {
                let azure_account = env::var("AZURE_ACCOUNT")?;
                let azure_key = env::var("AZURE_KEY")?;

                StorageAccountClient::new_access_key(http_client, azure_account, azure_key)
                    .as_storage_client()
            }
        };

        Ok(storage_client)
    }

    async fn get_azure_storage_client_from_file(
        config_file: &str,
        credentials_profile: Option<String>,
    ) -> Result<Arc<StorageClient>, Box<dyn Error>> {
        #![allow(clippy::print_stderr)] // XXX remove before production

        let http_client = azure_core::new_http_client();
        match fs::metadata(config_file) {
            Ok(file) => {
                if !file.is_file() {
                    return Err(
                        format!("credentials file {} is not a regular file", config_file).into(),
                    );
                }
            }
            Err(err) => {
                return Err(format!("credentials file {} not found. {:?}", config_file, err).into())
            }
        }

        let ini_file = ini::Ini::load_from_file(config_file)?;
        let azure_account = ini_file.get_from(credentials_profile.clone(), "AZURE_ACCOUNT");
        let azure_key = ini_file.get_from(credentials_profile.clone(), "AZURE_KEY");

        if azure_account.is_none() {
            eprintln!(
                "AZURE_ACCOUNT not found in {:?} profile in config file {}",
                credentials_profile, config_file
            );
            return Err(format!(
                "AZURE_ACCOUNT not found in {:?} profile in config file {}",
                credentials_profile, config_file
            )
            .into());
        }
        if azure_key.is_none() {
            eprintln!(
                "AZURE_KEY not found in {:?} profile in config file {}",
                credentials_profile, config_file
            );
            return Err(format!(
                "AZURE_KEY not found in {:?} profile in config file {}",
                credentials_profile, config_file
            )
            .into());
        }

        Ok(StorageAccountClient::new_access_key(
            http_client,
            azure_account.unwrap(),
            azure_key.unwrap(),
        )
        .as_storage_client())
    }

    // XXX Public because zoa_test uses it.
    pub async fn get_azure_storage_client(
        credentials_profile: Option<String>,
    ) -> Result<Arc<StorageClient>, Box<dyn Error>> {
        let credentials_file = match dirs_next::home_dir() {
            Some(mut home_path) => {
                home_path.push(".azure");
                home_path.push("credentials");
                home_path
            }
            None => {
                panic!("Unable to determine home directory.");
            }
        };

        match Self::get_azure_storage_client_from_env().await {
            Ok(storage_client) => Ok(storage_client),
            Err(_) => Ok(Self::get_azure_storage_client_from_file(
                credentials_file.to_str().unwrap(),
                credentials_profile,
            )
            .await?),
        }
    }

    pub async fn new(bucket: &str, credentials_profile: Option<String>) -> Self {
        let storage_account_client = Self::get_azure_storage_client(credentials_profile.clone())
            .await
            .unwrap();
        let container_client = storage_account_client.as_container_client(bucket);

        Self {
            container_client,
            access_stats: Default::default(),
            outstanding_ops: Default::default(),
            bucket_str: bucket.to_string(),
            credentials_profile,
        }
    }

    pub fn bucket(&self) -> String {
        self.bucket_str.clone()
    }

    pub fn credentials_profile(&self) -> Option<String> {
        self.credentials_profile.clone()
    }

    fn convert_error<T>(e: Box<dyn Error + Send + Sync>) -> OAError<T>
    where
        T: MaybeFrom<HttpError> + Display,
    {
        let http_error: Box<HttpError> = e.downcast().unwrap();
        OAError::from(*http_error)
    }
}

#[async_trait]
impl ObjectAccessTrait for BlobObjectAccess {
    async fn get_object(
        &self,
        key: String,
        stat_type: ObjectAccessOpType,
        range: Option<Range<usize>>,
    ) -> Result<Bytes> {
        let _permit = self.outstanding_ops[stat_type].acquire().await.unwrap();
        let op = self.access_stats.begin(stat_type);

        let msg = format!("get {}", key);
        let bytes = retry(&msg, None, || async {
            let begin = Instant::now();
            let blob_client = self.container_client.as_blob_client(key.clone());

            let get_builder = blob_client.get();
            range.as_ref().map(|r| get_builder.range(r.clone()));
            match get_builder.execute().await {
                Err(e) => {
                    debug!("{}: error while reading ByteStream: {}", msg, e);
                    Err(Self::convert_error::<GetError>(e))
                }
                Ok(res) => {
                    trace!(
                        "{}: got {} bytes of data ({:?}) in {}ms",
                        msg,
                        res.data.len(),
                        res.content_range,
                        begin.elapsed().as_millis()
                    );
                    if let Some(range) = &range {
                        assert_le!(res.data.len(), range.end - range.start);
                    }
                    Ok(res.data)
                }
            }
        })
        .await
        .with_context(|| format!("Failed to {}", msg))?;

        op.end(bytes.len() as u64);
        Ok(bytes)
    }

    fn collect_stats(&self) -> HashMap<String, StatMapValue> {
        self.access_stats.collect_stats()
    }

    async fn put_object_stream(
        &self,
        key: String,
        streamfunc: &(dyn Fn() -> (ByteStream, usize) + Send + Sync),
        stat_type: ObjectAccessOpType,
        timeout: Option<Duration>,
    ) -> Result<(), OAError<PutError>> {
        let _permit = self.outstanding_ops[stat_type].acquire().await.unwrap();
        let op = self.access_stats.begin(stat_type);
        let blob_client = self.container_client.as_blob_client(key.clone());

        let result = retry(&format!("put {}", key), timeout, || async {
            let (stream, len) = streamfunc();

            // XXX not streaming put yet; does put_page_blob fit the bill?
            let mut buf = BytesMut::with_capacity(len);
            stream.into_async_read().read_buf(&mut buf).await.unwrap();

            match blob_client.put_block_blob(buf).execute().await {
                Err(e) => {
                    debug!("error during put_block_blob {:?}", e);
                    Err(Self::convert_error::<PutError>(e))
                }
                Ok(_) => Ok(len),
            }
        })
        .await;

        op.end(result.as_ref().map(|len| *len).unwrap_or_default() as u64);

        result.map(|_| ())
    }

    async fn delete_objects(&self, stream: &mut (dyn Stream<Item = String> + Send + Unpin)) {
        stream
            .chunks(*OBJECT_DELETION_BATCH_SIZE)
            .for_each(|chunk| async move {
                let op = self.access_stats.begin(ObjectAccessOpType::ObjectDelete);
                let msg = format!("delete {} objects including {}", chunk.len(), &chunk[0]);
                for key in chunk.iter() {
                    retry(&msg, None, || async {
                        let begin = Instant::now();
                        let blob_client = self.container_client.as_blob_client(key.clone());
                        match blob_client.delete().execute().await {
                            Err(e) => {
                                debug!("error while deleting: {}", e);
                                Err(Self::convert_error::<ObjectStoreError>(e))
                            }
                            Ok(res) => {
                                trace!("deleted {} in {}ms", key, begin.elapsed().as_millis());
                                Ok(res)
                            }
                        }
                    })
                    .await
                    .unwrap();
                }
                op.end_multiple(0, chunk.len() as u64);
            })
            .await;
    }

    async fn stat_object(&self, _key: String) -> Option<ObjectStat> {
        todo!()
    }

    fn list(
        &self,
        prefix: String,
        start_after: Option<String>,
        use_delimiter: bool,
        list_prefixes: bool,
    ) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        let container_client = self.container_client.clone();
        let msg = format!("list {} (after {:?})", prefix, start_after);
        let list_prefix = prefix;

        let stream_result = stream! {

            let output = retry(&msg, None, || async {
                let list_builder = match use_delimiter {
                    true =>
                        container_client
                            .list_blobs()
                            .prefix(list_prefix.as_str())
                            .delimiter("/"),
                    false =>
                        container_client
                            .list_blobs()
                            .prefix(list_prefix.as_str())
                };
                match list_builder.execute().await
                {
                    Err(e) => {
                        debug!("{}: error while listing blogs: {}", &msg, e);
                        Err(Self::convert_error::<ObjectStoreError>(e))
                    }
                    Ok(res) => Ok(res),
                }
            })
            .await.unwrap();

            if list_prefixes {
                if let Some(prefixes) = output.blobs.blob_prefix {
                    for blob_prefix in prefixes {
                        yield blob_prefix.name;
                    }
                }
            } else {
                for blob in output.blobs.blobs {
                    yield blob.name;
                }
            }
        };

        Box::pin(stream_result)
    }
}
