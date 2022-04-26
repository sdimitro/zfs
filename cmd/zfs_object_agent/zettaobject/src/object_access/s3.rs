use core::time::Duration;
use std::collections::HashMap;
use std::fmt::Display;
use std::ops::Range;
use std::pin::Pin;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use bytes::Bytes;
use bytes::BytesMut;
use chrono::DateTime;
use enum_map::EnumMap;
use futures::future;
use futures::StreamExt;
use futures::TryStreamExt;
use futures_core::Stream;
use http::response::Builder;
use http::StatusCode;
use log::*;
use more_asserts::assert_le;
use rusoto_core::ByteStream;
use rusoto_core::RusotoError;
use rusoto_credential::ChainProvider;
use rusoto_credential::InstanceMetadataProvider;
use rusoto_credential::ProfileProvider;
use rusoto_s3::Delete;
use rusoto_s3::DeleteObjectsRequest;
use rusoto_s3::GetObjectError;
use rusoto_s3::GetObjectRequest;
use rusoto_s3::HeadObjectRequest;
use rusoto_s3::ListObjectsV2Request;
use rusoto_s3::ObjectIdentifier;
use rusoto_s3::PutObjectError;
use rusoto_s3::PutObjectRequest;
use rusoto_s3::S3Client;
use rusoto_s3::S3;
use util::with_alloctag;

use super::BucketAccessTrait;
use super::GetError;
use super::ObjectAccessCredentials;
use super::RequestError;
use super::OBJECT_DELETION_BATCH_SIZE;
use crate::access_stats::ObjectAccessOpType;
use crate::access_stats::ObjectAccessStats;
use crate::access_stats::OutstandingOps;
use crate::access_stats::StatMapValue;
use crate::object_access::retry;
use crate::object_access::OAError;
use crate::object_access::ObjectAccessTrait;
use crate::object_access::ObjectStat;
use crate::object_access::PutError;

impl From<GetObjectError> for GetError {
    fn from(e: GetObjectError) -> Self {
        match e {
            GetObjectError::InvalidObjectState(s) => Self(format!("Invalid state: {}", s)),
            GetObjectError::NoSuchKey(s) => Self(format!("No such key: {}", s)),
        }
    }
}

impl From<PutObjectError> for PutError {
    fn from(_: PutObjectError) -> Self {
        // XXX S3 put errors are always returned as Unknown, at present.
        PutError {}
    }
}

impl<E: Display> From<RusotoError<E>> for RequestError<E> {
    fn from(e: RusotoError<E>) -> Self {
        match e {
            RusotoError::Service(e) => Self::Service(e),
            RusotoError::HttpDispatch(error) => Self::InternalError(error.to_string()),
            RusotoError::Credentials(error) => Self::Credentials(error.to_string()),
            RusotoError::Validation(s) => Self::InternalError(s),
            RusotoError::ParseError(s) => Self::InternalError(s),
            RusotoError::Unknown(response) => {
                match response.status {
                    StatusCode::BAD_REQUEST => {
                        if response.body_as_str().contains("ExpiredToken") {
                            return Self::ExpiredCredentials;
                        }
                    }
                    StatusCode::FORBIDDEN => {
                        if response.body_as_str().contains("RequestTimeTooSkewed") {
                            return Self::TimeSkew;
                        }
                        return Self::InvalidCredentials;
                    }
                    _ => {}
                };
                let mut builder = Builder::new().status(response.status);
                for (name, value) in response.headers.iter() {
                    builder = builder.header(name.clone(), value);
                }
                Self::Unknown(builder.body(response.body).unwrap())
            }
            RusotoError::Blocking => {
                Self::InternalError("Failed to execute blocking future".to_string())
            }
        }
    }
}
impl<E: Display> From<RusotoError<E>> for OAError<E> {
    fn from(e: RusotoError<E>) -> Self {
        Self::RequestError(RequestError::from(e))
    }
}

pub struct S3BucketAccess {
    client: rusoto_s3::S3Client,
}

impl S3BucketAccess {
    pub fn new(
        endpoint: &str,
        region: &str,
        credentials_profile: Option<String>,
    ) -> anyhow::Result<Self> {
        Ok(S3BucketAccess {
            client: S3ObjectAccess::get_client(endpoint, region, credentials_profile),
        })
    }
}

#[async_trait]
impl BucketAccessTrait for S3BucketAccess {
    async fn list_buckets(&self) -> Vec<String> {
        let list_output = retry("list_buckets", None, || async {
            Ok(self.client.list_buckets().await?)
        })
        .await;

        list_output
            .unwrap()
            .buckets
            .unwrap()
            .into_iter()
            .map(|b| b.name.unwrap())
            .collect()
    }
}

pub struct S3ObjectAccess {
    client: rusoto_s3::S3Client,
    bucket: String,
    region: String,
    endpoint: String,
    credentials_profile: Option<String>,
    access_stats: ObjectAccessStats,
    outstanding_ops: EnumMap<ObjectAccessOpType, OutstandingOps>,
}

impl S3ObjectAccess {
    fn convert_error<F: Display, T: Display>(e: RusotoError<F>) -> RequestError<T>
    where
        T: From<F> + Display,
    {
        match RequestError::from(e) {
            RequestError::Service(err) => RequestError::Service(T::from(err)),
            RequestError::Unknown(r) => RequestError::Unknown(r),
            RequestError::InternalError(s) => RequestError::InternalError(s),
            RequestError::Credentials(s) => RequestError::Credentials(s),
            RequestError::ExpiredCredentials => RequestError::ExpiredCredentials,
            RequestError::InvalidCredentials => RequestError::InvalidCredentials,
            RequestError::TimeSkew => RequestError::TimeSkew,
        }
    }

    fn get_custom_region(endpoint: &str, region: &str) -> rusoto_core::Region {
        rusoto_core::Region::Custom {
            name: region.to_owned(),
            endpoint: endpoint.to_owned(),
        }
    }

    /// Get client by checking in order, the following sources for credentials.
    /// 1. Environment variables
    /// 2. AWS credentials file
    /// 3. IAM instance profile.
    fn get_client(endpoint: &str, region: &str, credentials_profile: Option<String>) -> S3Client {
        info!("region: {}", region);
        info!("Endpoint: {}", endpoint);
        info!("Profile: {:?}", credentials_profile);

        let provider =
            util::ResilientCredentialsProvider::new(ChainProvider::with_profile_provider(
                ProfileProvider::with_default_credentials(
                    credentials_profile.unwrap_or_else(|| "default".to_owned()),
                )
                .unwrap(),
            ))
            .unwrap();

        let http_client = rusoto_core::HttpClient::new().unwrap();
        let region = S3ObjectAccess::get_custom_region(endpoint, region);
        rusoto_s3::S3Client::new_with(http_client, provider, region)
    }

    fn from_client(
        client: rusoto_s3::S3Client,
        bucket: &str,
        endpoint: &str,
        region: &str,
    ) -> Self {
        S3ObjectAccess {
            client,
            bucket: bucket.to_string(),
            region: region.to_string(),
            endpoint: endpoint.to_string(),
            credentials_profile: None,
            access_stats: Default::default(),
            outstanding_ops: Default::default(),
        }
    }

    fn new_with_key(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> Self {
        info!("region: {:?}", region);
        info!("Endpoint: {}", endpoint);

        let http_client = rusoto_core::HttpClient::new().unwrap();
        let creds = rusoto_core::credential::StaticProvider::new(
            access_key_id.to_string(),
            secret_access_key.to_string(),
            None,
            None,
        );
        let s3_region = S3ObjectAccess::get_custom_region(endpoint, region);
        let client = rusoto_s3::S3Client::new_with(http_client, creds, s3_region);

        Self::from_client(client, bucket, endpoint, region)
    }

    /// Create S3 object access with instance metadata provider and ignoring all other sources of
    /// credentials.
    fn new_with_instance_profile(endpoint: &str, region: &str, bucket: &str) -> Self {
        let http_client = rusoto_core::HttpClient::new().unwrap();
        let creds = InstanceMetadataProvider::new();
        let s3_region = S3ObjectAccess::get_custom_region(endpoint, region);
        let client = rusoto_s3::S3Client::new_with(http_client, creds, s3_region);

        Self::from_client(client, bucket, endpoint, region)
    }

    pub fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        credentials: ObjectAccessCredentials,
    ) -> Self {
        match credentials {
            ObjectAccessCredentials::Profile { profile } => Self {
                client: S3ObjectAccess::get_client(endpoint, region, profile.clone()),
                bucket: bucket.to_string(),
                region: region.to_string(),
                endpoint: endpoint.to_string(),
                credentials_profile: profile,
                access_stats: Default::default(),
                outstanding_ops: Default::default(),
            },
            ObjectAccessCredentials::Key {
                access_key_id,
                secret_access_key,
            } => Self::new_with_key(endpoint, region, bucket, &access_key_id, &secret_access_key),
            ObjectAccessCredentials::ManagedCredentials => {
                Self::new_with_instance_profile(endpoint, region, bucket)
            }
        }
    }

    pub fn bucket(&self) -> String {
        self.bucket.clone()
    }

    pub fn region(&self) -> String {
        self.region.clone()
    }

    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }

    pub fn credentials_profile(&self) -> Option<String> {
        self.credentials_profile.clone()
    }
}

#[async_trait]
impl ObjectAccessTrait for S3ObjectAccess {
    async fn get_object(
        &self,
        key: String,
        stat_type: ObjectAccessOpType,
        range: Option<Range<usize>>,
    ) -> Result<Bytes> {
        let _permit = self.outstanding_ops[stat_type].acquire().await.unwrap();
        let op = self.access_stats.begin(stat_type);
        let msg = format!("get {}", key);
        let range_string = range
            .as_ref()
            .map(|r| format!("bytes={}-{}", r.start, r.end - 1));
        let bytes = retry(&msg, None, || async {
            let req = GetObjectRequest {
                bucket: self.bucket.clone(),
                key: key.clone(),
                range: range_string.clone(),
                ..Default::default()
            };
            let output = self.client.get_object(req).await?;
            let begin = Instant::now();
            let mut v = with_alloctag("ObjectAccess::get_object_impl()", || {
                BytesMut::with_capacity(
                    usize::try_from(output.content_length.unwrap_or(0)).unwrap(),
                )
            });
            let mut count: u32 = 0;
            match output
                .body
                .unwrap()
                .try_for_each(|b| {
                    // XXX This memory copy is expensive.  Redesign this to return a bytes::Buf
                    // that chains together all of the Bytes provided here?
                    v.extend_from_slice(&b);
                    count += 1;
                    future::ready(Ok(()))
                })
                .await
            {
                Err(e) => {
                    debug!("{}: error while reading ByteStream: {}", msg, e);
                    Err(OAError::RequestError(e.into()))
                }
                Ok(_) => {
                    trace!(
                        "{}: got {} bytes of data ({:?}) in {} chunks in {}ms",
                        msg,
                        v.len(),
                        output.content_range,
                        count,
                        begin.elapsed().as_millis()
                    );
                    if let Some(range) = &range {
                        assert_le!(v.len(), range.end - range.start);
                    }
                    Ok(v)
                }
            }
        })
        .await
        .with_context(|| format!("Failed to {}", msg))?;

        op.end(bytes.len() as u64);
        Ok(bytes.into())
    }

    async fn stat_object(&self, key: String) -> Option<ObjectStat> {
        let res = retry(&format!("head {}", key), None, || async {
            let req = HeadObjectRequest {
                bucket: self.bucket.clone(),
                key: key.clone(),
                ..Default::default()
            };
            // Note: Ok(...?) converts the RusotoError to an OAError for us
            Ok(self.client.head_object(req).await?)
        })
        .await;
        res.ok().map(|out| ObjectStat {
            last_modified: out
                .last_modified
                .and_then(|x| DateTime::parse_from_rfc2822(x.as_str()).ok()),
        })
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
            // We want to only call streamfunc() once in the common case,
            // because for DataObject::put() it needs to clone (bump the
            // refcount) of every block's Bytes.  Therefore we don't want to
            // call streamfunc() for the sole purpuse of getting the size_hint.
            // Instead, get it here and return it.
            let (stream, len) = streamfunc();
            let req = PutObjectRequest {
                bucket: self.bucket.clone(),
                key: key.clone(),
                body: Some(stream),
                ..Default::default()
            };
            match self.client.put_object(req).await {
                Err(e) => {
                    debug!("error during put_block_s3 {:?}", e);
                    Err(OAError::RequestError(Self::convert_error(e)))
                }
                Ok(_) => Ok(len),
            }
        })
        .await;
        op.end(result.as_ref().map(|len| *len).unwrap_or_default() as u64);

        result.map(|_| ())
    }

    // Note: Stream is of raw keys (with prefix)
    async fn delete_objects(&self, stream: &mut (dyn Stream<Item = String> + Send + Unpin)) {
        // Note: we intentionally issue the delete calls serially because it
        // doesn't seem to improve performance if we issue them in parallel
        // (using StreamExt::for_each_concurrent()).
        stream
            .chunks(*OBJECT_DELETION_BATCH_SIZE)
            .for_each(|chunk| async move {
                let msg = format!("delete {} objects including {}", chunk.len(), &chunk[0]);
                let op = self.access_stats.begin(ObjectAccessOpType::ObjectDelete);

                retry(&msg, None, || async {
                    let req = DeleteObjectsRequest {
                        bucket: self.bucket.clone(),
                        delete: Delete {
                            objects: chunk
                                .iter()
                                .map(|key| ObjectIdentifier {
                                    key: key.clone(),
                                    ..Default::default()
                                })
                                .collect(),
                            quiet: Some(true),
                        },
                        ..Default::default()
                    };
                    let output = self.client.delete_objects(req).await?;
                    match output.errors {
                        Some(errs) => match errs.get(0) {
                            Some(e) => Err(OAError::Other(anyhow!("{:?}", e))),
                            None => Ok(()),
                        },
                        None => Ok(()),
                    }
                })
                .await
                .unwrap();
                op.end_multiple(0, chunk.len() as u64);
            })
            .await;
    }

    fn list(
        &self,
        prefix: String,
        start_after: Option<String>,
        use_delimiter: bool,
        list_prefixes: bool,
    ) -> Pin<Box<dyn Stream<Item = String> + Send>> {
        let mut continuation_token = None;
        // XXX ObjectAccess should really be refcounted (behind Arc)
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let delimiter = match use_delimiter {
            true => Some("/".to_string()),
            false => None,
        };
        Box::pin(stream! {
            loop {
                let output = retry(
                    &format!("list {} (after {:?})", prefix, start_after),
                    None,
                    || async {
                        let req = ListObjectsV2Request {
                            bucket: bucket.clone(),
                            continuation_token: continuation_token.clone(),
                            delimiter: delimiter.clone(),
                            fetch_owner: Some(false),
                            prefix: Some(prefix.clone()),
                            start_after: start_after.clone(),
                            ..Default::default()
                        };
                        // Note: Ok(...?) converts the RusotoError to an OAError for us
                        Ok(client.list_objects_v2(req).await?)
                    },
                )
                .await
                .unwrap();

                if list_prefixes {
                    if let Some(prefixes) = output.common_prefixes {
                        for prefix in prefixes {
                            yield prefix.prefix.unwrap();
                        }
                    }
                } else {
                    if let Some(objects) = output.contents {
                        for object in objects {
                            yield object.key.unwrap();
                        }
                    }
                }
                if output.next_continuation_token.is_none() {
                    break;
                }
                continuation_token = output.next_continuation_token;
            }
        })
    }
}
