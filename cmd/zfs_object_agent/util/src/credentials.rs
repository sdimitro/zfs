use async_trait::async_trait;
use chrono::Duration;
use chrono::Utc;
use lazy_static::lazy_static;
use log::*;
use rusoto_credential::{AwsCredentials, CredentialsError, ProvideAwsCredentials};
use tokio::sync::Mutex;

use crate::get_tunable;

lazy_static! {
    /// Buffer period to use while determining if cached credentials have expired.
    /// The default value is 15 minutes, the maximum clock skew allowed for s3 requests.
    static ref CREDENTIALS_BUFFER_SECONDS: Duration = Duration::seconds(get_tunable("credential_buffer_secs", 15 * 60));
}

/// The `ResilientCredentialsProvider` is a wrapper over another `ProvideAwsCredentials`. It caches the credentials
/// until they expire. On expiry, it auto-refreshes the credentials. This primarily differs from
/// `rusoto_credential::AutoRefreshingProvider` in that it uses a tunable buffer period which defaults to 15 minutes
/// instead of a hard-coded 20 seconds.
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
                // We incorporate a buffer period of 15 minutes, which is the max clock skew that is tolerated by AWS
                // while determining if the credentials have expired.
                Some(cred_expiry) => *cred_expiry - *CREDENTIALS_BUFFER_SECONDS < Utc::now(),
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
