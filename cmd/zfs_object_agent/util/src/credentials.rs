use async_trait::async_trait;
use chrono::Duration;
use chrono::Utc;
use log::*;
use rusoto_credential::AwsCredentials;
use rusoto_credential::CredentialsError;
use rusoto_credential::ProvideAwsCredentials;
use tokio::sync::Mutex;

use crate::tunable;

tunable! {
    // Buffer period to use while determining if cached credentials have expired.
    // The default value is 15 minutes, the maximum clock skew allowed for s3 requests.
    static ref S3_CREDENTIALS_BUFFER_DURATION: Duration = Duration::minutes(15);
}

/// The `ResilientCredentialsProvider` is a wrapper over another `ProvideAwsCredentials`. It caches
/// the credentials until they expire. On expiry, it auto-refreshes the credentials. This primarily
/// differs from `rusoto_credential::AutoRefreshingProvider` in that it uses a tunable buffer period
/// which defaults to 15 minutes instead of a hard-coded 20 seconds.
#[derive(Debug)]
pub struct ResilientCredentialsProvider<P: ProvideAwsCredentials> {
    credentials_provider: P,
    cached_credentials: Mutex<Option<AwsCredentials>>,
}

impl<P: ProvideAwsCredentials> ResilientCredentialsProvider<P> {
    pub fn new(
        credentials_provider: P,
    ) -> Result<ResilientCredentialsProvider<P>, CredentialsError> {
        Ok(ResilientCredentialsProvider {
            credentials_provider,
            cached_credentials: Mutex::new(None),
        })
    }
}

#[async_trait]
impl<P: ProvideAwsCredentials + Send + Sync> ProvideAwsCredentials
    for ResilientCredentialsProvider<P>
{
    async fn credentials(&self) -> Result<AwsCredentials, CredentialsError> {
        let mut cred_guard = self.cached_credentials.lock().await;

        // Has the cached credentials expired?
        if let Some(creds) = cred_guard.as_ref() {
            let expired = match creds.expires_at() {
                // We incorporate a buffer period of 15 minutes, which is the max clock skew that is
                // tolerated by AWS while determining if the credentials have expired.
                Some(cred_expiry) => *cred_expiry - *S3_CREDENTIALS_BUFFER_DURATION < Utc::now(),
                None => false,
            };
            if !expired {
                return Ok(creds.clone());
            }
        }

        trace!("Getting credentials from provider.");
        let result = self.credentials_provider.credentials().await;
        if let Ok(new_creds) = &result {
            *cred_guard = Some(new_creds.clone());
        }

        result
    }
}
