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

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use azure_core::HttpError;
use azure_identity::token_credentials::ImdsManagedIdentityCredential;
use azure_identity::token_credentials::TokenCredential;
use azure_storage::clients::AsStorageClient;
use azure_storage::clients::StorageAccountClient;
use azure_storage::clients::StorageClient;
use azure_storage_blobs::prelude::AsBlobClient;
use azure_storage_blobs::prelude::AsBlobServiceClient;
use azure_storage_blobs::prelude::AsContainerClient;
use azure_storage_blobs::prelude::BlobServiceClient;
use azure_storage_blobs::prelude::ContainerClient;
use bytes::Bytes;
use bytes::BytesMut;
use chrono::DateTime;
use chrono::Utc;
use enum_map::EnumMap;
use futures::Stream;
use futures::StreamExt;
use http::Response;
use http::StatusCode;
use ini::Ini;
use log::*;
use more_asserts::assert_le;
use rusoto_core::ByteStream;
use tokio::io::AsyncReadExt;
use tokio::sync::RwLock;
use util::tunable;

use super::retry;
use super::BlobCredentials;
use super::BucketAccessTrait;
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

tunable! {
    // Buffer period to use while determining if credentials have expired.
    // The default value of 15 minutes corresponds to the maximum clock skew that Azure tolerates.
    static ref BLOB_CREDENTIALS_BUFFER_DURATION: chrono::Duration = chrono::Duration::minutes(15);
}

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
            HttpError::StatusCode {
                status: StatusCode::NOT_FOUND,
                body,
            } => Ok(PutError::NoSuchContainer(body)),
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
            } => Ok(ObjectStoreError::NoSuchKey),
            HttpError::StatusCode { status: _, body } => Ok(ObjectStoreError::Other(body)),
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

struct BlobBucketClient {
    blob_service: Arc<BlobServiceClient>,
    expires_on: Option<DateTime<Utc>>,
}

impl BlobBucketClient {
    async fn new(credentials: BlobCredentials) -> Result<Self> {
        let (storage_client, expires_on) = get_azure_storage_client(credentials).await?;
        let blob_service = storage_client.as_blob_service_client();
        Ok(Self {
            blob_service,
            expires_on,
        })
    }

    fn is_expired(&self) -> bool {
        match self.expires_on {
            Some(expiry) => {
                trace!("BlobServiceClient credential tokens expires on {}", expiry);
                expiry < Utc::now() + *BLOB_CREDENTIALS_BUFFER_DURATION
            }
            None => false,
        }
    }
}

pub struct BlobBucketAccess {
    blob_bucket_client: RwLock<BlobBucketClient>,
    credentials: BlobCredentials,
}

impl BlobBucketAccess {
    pub async fn new(credentials: BlobCredentials) -> Result<Self> {
        let blob_bucket_client = BlobBucketClient::new(credentials.clone()).await?;
        Ok(Self {
            blob_bucket_client: RwLock::new(blob_bucket_client),
            credentials,
        })
    }

    async fn update_bucket_client(&self) -> Arc<BlobServiceClient> {
        let mut blob_bucket_client = self.blob_bucket_client.write().await;
        // Expiry might have been checked earlier but we check again after taking the write lock.
        if blob_bucket_client.is_expired() {
            match BlobBucketClient::new(self.credentials.clone()).await {
                Ok(new_blob_bucket_client) => {
                    info!("BlobServiceClient refreshed after the credential tokens expired");
                    *blob_bucket_client = new_blob_bucket_client;
                }
                Err(err) => {
                    // We consider the token to be exipired 15 minutes before actual expiry.
                    // So, the existing BlobServiceClient might still be valid. We drive on in
                    // the hope that the next time we issue an op, we will retry this and
                    // perhaps succceed. When the token actually expires, the ops will start
                    // failing and we will keep retrying until whatever error that is causing
                    // the failure is resolved.
                    error!("Refreshing BlobServiceClient failed {:?}", err);
                }
            };
        }
        blob_bucket_client.blob_service.clone()
    }

    async fn get_bucket_client(&self) -> Arc<BlobServiceClient> {
        // If the client has not expired, return it; else return an updated client.
        {
            let blob_bucket_client = self.blob_bucket_client.read().await;
            if !blob_bucket_client.is_expired() {
                return blob_bucket_client.blob_service.clone();
            }
        } // drop blob_bucket_client so that we don't deadlock when update_bucket_client() acquires
          // the lock for writer.

        self.update_bucket_client().await
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
impl BucketAccessTrait for BlobBucketAccess {
    async fn list_buckets(&self) -> Vec<String> {
        let msg = "list_buckets";
        let list_output = retry(msg, None, || async {
            let result = self
                .get_bucket_client()
                .await
                .list_containers()
                .execute()
                .await
                .map_err(|e| {
                    debug!("{}: {}", msg, e);
                    Self::convert_error::<ObjectStoreError>(e)
                });

            result
        })
        .await;

        list_output
            .unwrap()
            .incomplete_vector
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }
}

struct BlobContainerClient {
    container_client: Arc<ContainerClient>,
    expires_on: Option<DateTime<Utc>>,
}

impl BlobContainerClient {
    async fn new(bucket: &str, credentials: BlobCredentials) -> Result<Self> {
        let (storage_account_client, expires_on) =
            get_azure_storage_client(credentials.clone()).await?;
        let container_client = storage_account_client.as_container_client(bucket);

        Ok(Self {
            container_client,
            expires_on,
        })
    }

    fn is_expired(&self) -> bool {
        match self.expires_on {
            Some(expiry) => {
                trace!("ContainerClient credential tokens expire on {}", expiry);
                expiry < Utc::now() + *BLOB_CREDENTIALS_BUFFER_DURATION
            }
            None => false,
        }
    }
}

pub struct BlobObjectAccess {
    blob_container_client: RwLock<BlobContainerClient>,
    bucket: String,
    credentials: BlobCredentials,
    access_stats: ObjectAccessStats,
    outstanding_ops: EnumMap<ObjectAccessOpType, OutstandingOps>,
}

impl BlobObjectAccess {
    async fn update_container_client(&self) -> Arc<ContainerClient> {
        let mut blob_container_client = self.blob_container_client.write().await;
        // Expiry might have been checked earlier but we check again after taking the write lock.
        if blob_container_client.is_expired() {
            match BlobContainerClient::new(&self.bucket, self.credentials.clone()).await {
                Ok(new_container_client) => {
                    info!("ContainerClient refreshed after the credential tokens expired");
                    *blob_container_client = new_container_client;
                }
                Err(err) => {
                    // We consider the token to be exipired 15 minutes before actual expiry.
                    // So, the existing ContainerClient might still be valid. We drive on in
                    // the hope that the next time we issue an op, we will retry this and
                    // perhaps succceed. When the token actually expires, the ops will start
                    // failing and we will keep retrying until whatever error that is causing
                    // the failure is resolved.
                    error!("Refreshing ContainerClient failed {:?}", err);
                }
            };
        }

        blob_container_client.container_client.clone()
    }

    async fn get_container_client(&self) -> Arc<ContainerClient> {
        // If the client has not expired, return it; else return an updated client.
        {
            let blob_container_client = self.blob_container_client.read().await;
            if !blob_container_client.is_expired() {
                return blob_container_client.container_client.clone();
            }
        } // drop blob_container_client so that we don't deadlock when update_container_client()
          // acquires the lock for writer.

        self.update_container_client().await
    }

    pub async fn new(bucket: &str, credentials: BlobCredentials) -> Result<Self> {
        let blob_client = BlobContainerClient::new(bucket, credentials.clone()).await?;

        Ok(Self {
            blob_container_client: RwLock::new(blob_client),
            access_stats: Default::default(),
            outstanding_ops: Default::default(),
            bucket: bucket.to_string(),
            credentials,
        })
    }

    pub fn bucket(&self) -> String {
        self.bucket.clone()
    }

    pub fn credentials_profile(&self) -> Option<String> {
        if let BlobCredentials::Profile(profile) = &self.credentials {
            Some(profile.clone())
        } else {
            None
        }
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
            let blob_client = self
                .get_container_client()
                .await
                .as_blob_client(key.clone());
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

        let result = retry(&format!("put {}", key), timeout, || async {
            let blob_client = self
                .get_container_client()
                .await
                .as_blob_client(key.clone());

            let (stream, len) = streamfunc();

            // XXX not streaming put yet; does put_page_blob fit the bill?
            let mut buf = BytesMut::with_capacity(len);
            buf.resize(len, 0);
            stream.into_async_read().read_exact(&mut buf).await.unwrap();

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
                        let blob_client = self
                            .get_container_client()
                            .await
                            .as_blob_client(key.clone());
                        match blob_client.delete().execute().await {
                            Err(e) => {
                                debug!("error while deleting: {}", e);
                                let err = Self::convert_error::<ObjectStoreError>(e);
                                if let OAError::RequestError(RequestError::Service(
                                    ObjectStoreError::NoSuchKey,
                                )) = err
                                {
                                    Ok(None)
                                } else {
                                    Err(err)
                                }
                            }
                            Ok(res) => {
                                trace!("deleted {} in {}ms", key, begin.elapsed().as_millis());
                                Ok(Some(res))
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

    async fn stat_object(&self, key: String) -> Option<ObjectStat> {
        let msg = format!("head {}", key);
        retry(&msg, None, || async {
            let blob_client = self
                .get_container_client()
                .await
                .as_blob_client(key.clone());
            match blob_client.get_properties().execute().await {
                Err(e) => {
                    debug!("{}: {}", &msg, e);
                    Err(Self::convert_error::<ObjectStoreError>(e))
                }
                Ok(res) => Ok(res),
            }
        })
        .await
        .ok()
        .map(|prop| ObjectStat {
            last_modified: Some(DateTime::from(prop.blob.properties.last_modified)),
        })
    }

    fn list(
        &self,
        prefix: String,
        start_after: Option<String>,
        use_delimiter: bool,
        list_prefixes: bool,
    ) -> Pin<Box<dyn Stream<Item = String> + Send + '_>> {
        let msg = format!("list {} (after {:?})", prefix, start_after);
        let list_prefix = prefix;

        let stream_result = stream! {
            let output = retry(&msg, None, || async {
                let container_client = self.get_container_client().await;
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
                        debug!("{}: {}", &msg, e);
                        Err(Self::convert_error::<ObjectStoreError>(e))
                    }
                    Ok(res) => Ok(res),
                }
            })
            .await.unwrap();

            // XXX The performance of this is likely to be quite bad. We need a better solution. DOSE-1215
            let initial = start_after.unwrap_or("".to_string());
            if list_prefixes {
                if let Some(prefixes) = output.blobs.blob_prefix {
                    for blob_prefix in prefixes {
                        if initial < blob_prefix.name {
                            yield blob_prefix.name;
                        }
                    }
                }
            } else {
                for blob in output.blobs.blobs {
                    if initial < blob.name {
                        yield blob.name;
                    }
                }
            }
        };

        Box::pin(stream_result)
    }
}

// Creation of a BlobObjectAccess object with invalid credentials can cause a crash as the azure sdk
// calls unwrap() while decoding the credentials. To avoid this, we validate the credentials
// before passing it to the azure sdk.
fn validate_azure_key(azure_key: &str) -> Result<()> {
    match base64::decode(&azure_key) {
        Ok(_) => Ok(()),
        Err(err) => Err(anyhow!("Invalid credentials: {:?}", err)),
    }
}

async fn get_azure_storage_client_with_managed_key_profile(
    profile: String,
) -> Result<(Arc<StorageClient>, Option<DateTime<Utc>>)> {
    let ini_file = get_credentials_file()?;

    let azure_account = match ini_file.get_from(Some(&profile), "AZURE_ACCOUNT") {
        None => {
            return Err(anyhow!(
                "AZURE_ACCOUNT not found in {:?} profile in ~/.azure/credentials file.",
                profile
            ));
        }
        Some(azure_account) => azure_account,
    };

    get_azure_storage_client_with_managed_key(azure_account).await
}

async fn get_azure_storage_client_with_managed_key(
    azure_account: &str,
) -> Result<(Arc<StorageClient>, Option<DateTime<Utc>>)> {
    // azure-sdk-for-net checks for an optional env variable "IDENTITY_HEADER" and calls unwrap on
    // it. Until this bug is fixed, we have to workaround it by setting this variable.
    // See: https://github.com/Azure/azure-sdk-for-rust/issues/420
    env::set_var("IDENTITY_HEADER", "");

    let http_client = azure_core::new_http_client();

    // There is a new AutoRefreshingTokenCredential wrapper in the repo that has not been released
    // yet. Once it is released, we should consider using it.
    // See: https://github.com/Azure/azure-sdk-for-rust/pull/673
    let creds = ImdsManagedIdentityCredential {};

    let bearer_token = creds.get_token("https://storage.azure.com/").await?;
    let expires_on = bearer_token.expires_on;
    let client = StorageAccountClient::new_bearer_token(
        http_client.clone(),
        azure_account,
        bearer_token.token.secret(),
    )
    .as_storage_client();

    Ok((client, Some(expires_on)))
}

fn get_azure_storage_client_from_key(
    azure_account: &str,
    azure_key: &str,
) -> Result<(Arc<StorageClient>, Option<DateTime<Utc>>)> {
    let http_client = azure_core::new_http_client();
    validate_azure_key(azure_key)?;

    Ok((
        StorageAccountClient::new_access_key(http_client, azure_account, azure_key)
            .as_storage_client(),
        None,
    ))
}

fn get_azure_storage_client_from_env() -> Result<(Arc<StorageClient>, Option<DateTime<Utc>>)> {
    let http_client = azure_core::new_http_client();
    let storage_client = match env::var("AZURE_CONNECTION_STRING") {
        Ok(connection_string) => {
            StorageAccountClient::new_connection_string(http_client.clone(), &connection_string)?
                .as_storage_client()
        }
        Err(_) => {
            let azure_account = env::var("AZURE_ACCOUNT")?;
            let azure_key = env::var("AZURE_KEY")?;

            validate_azure_key(&azure_key)?;
            StorageAccountClient::new_access_key(http_client, azure_account, azure_key)
                .as_storage_client()
        }
    };

    Ok((storage_client, None))
}
fn get_credentials_file() -> Result<Ini> {
    let home_dir = dirs_next::home_dir();
    if home_dir.is_none() {
        return Err(anyhow!("Unable to determine home directory."));
    }
    let mut file_path = home_dir.unwrap();
    file_path.push(".azure");
    file_path.push("credentials");

    let credentials_file = file_path.to_str().unwrap();

    trace!("Reading credentials from {}", credentials_file);
    match fs::metadata(credentials_file) {
        Ok(file) => {
            if !file.is_file() {
                return Err(anyhow!(
                    "credentials file {} is not a regular file",
                    credentials_file
                ));
            }
        }
        Err(err) => {
            return Err(anyhow!(
                "credentials file {} not found. {:?}",
                credentials_file,
                err
            ));
        }
    }

    Ok(ini::Ini::load_from_file(credentials_file)?)
}

fn get_azure_storage_client_from_profile_key(
    credentials_profile: String,
) -> Result<(Arc<StorageClient>, Option<DateTime<Utc>>)> {
    let ini_file = get_credentials_file()?;

    let azure_account = match ini_file.get_from(Some(credentials_profile.clone()), "AZURE_ACCOUNT")
    {
        None => {
            return Err(anyhow!(
                "AZURE_ACCOUNT not found in {:?} profile in ~/.azure/credentials file.",
                credentials_profile
            ));
        }
        Some(azure_account) => azure_account,
    };
    let azure_key = match ini_file.get_from(Some(credentials_profile.clone()), "AZURE_KEY") {
        None => {
            return Err(anyhow!(
                "AZURE_KEY not found in {:?} profile in ~/.azure/credentials file",
                credentials_profile
            ));
        }
        Some(azure_key) => azure_key,
    };

    validate_azure_key(azure_key)?;

    let http_client = azure_core::new_http_client();
    Ok((
        StorageAccountClient::new_access_key(http_client, azure_account, azure_key)
            .as_storage_client(),
        None,
    ))
}

/// Create a StorageClient after getting credentials the following sources in order:
/// 1. Environment variables
/// 2. ~/.azure/credentials file
/// 3. managed identities.
/// Once credentials have been successfully obtained from a source, we do not try the rest of the
/// sources even if the credentials are invalid.
async fn get_azure_storage_client_automatic() -> Result<(Arc<StorageClient>, Option<DateTime<Utc>>)>
{
    match get_azure_storage_client_from_env()
        .or_else(|_| get_azure_storage_client_from_profile_key("default".to_string()))
    {
        Ok(tuple) => Ok(tuple),
        Err(_) => get_azure_storage_client_with_managed_key_profile("default".to_string()).await,
    }
}

async fn get_azure_storage_client(
    credentials: BlobCredentials,
) -> Result<(Arc<StorageClient>, Option<DateTime<Utc>>)> {
    match credentials {
        BlobCredentials::Profile(profile) => {
            // BlobCredentials::Profile is for getting credentials from a profile in an ini file.
            // The credentials could be directly specified as a pair of azure_account and azure_key.
            // Alternatively, the profile could just reference an azure_account and the key may then
            // be fetched via Managed Identity Credential. This are similar  to
            // BlobCredentials::Key and BlobCredentials::ManagedCredentials respectively, except for
            // the fact that it is passed via an ini file. We have to try both methods.
            match get_azure_storage_client_from_profile_key(profile.clone()) {
                Ok(tuple) => Ok(tuple),
                Err(_) => get_azure_storage_client_with_managed_key_profile(profile).await,
            }
        }
        BlobCredentials::Key {
            azure_account,
            azure_key,
        } => Ok(get_azure_storage_client_from_key(
            &azure_account,
            &azure_key,
        )?),
        BlobCredentials::ManagedCredentials { azure_account } => {
            Ok(get_azure_storage_client_with_managed_key(&azure_account).await?)
        }
        BlobCredentials::Automatic => Ok(get_azure_storage_client_automatic().await?),
    }
}
