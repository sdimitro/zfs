use std::time::Duration;

use rand::Rng;
use serde::Deserialize;
use util::writeln_stderr;
use util::writeln_stdout;

use crate::access_stats::ObjectAccessOpType;
use crate::object_access::OAError;
use crate::object_access::ObjectAccess;
use crate::object_access::ObjectAccessCredentials;
use crate::object_access::ObjectAccessProtocol;
use crate::object_access::RequestError;

#[derive(Debug, Deserialize)]
struct Error {
    #[serde(rename = "Code")]
    code: String,
    #[serde(rename = "Message")]
    message: String,
}

// Test by writing and deleting an object.
async fn do_test_connectivity(object_access: &ObjectAccess) -> Result<(), String> {
    let num: u64 = rand::thread_rng().gen();
    let file = format!("test/test_connectivity_{}", num);
    let content = "test connectivity to S3".as_bytes().to_vec();

    match object_access
        .put_object_timed(
            file.clone(),
            content.into(),
            ObjectAccessOpType::MetadataPut,
            Some(Duration::from_secs(30)),
        )
        .await
    {
        Err(OAError::RequestError(RequestError::Unknown(response))) => {
            match serde_xml_rs::from_str::<Error>(std::str::from_utf8(response.body()).unwrap()) {
                Ok(error) => Err(format!(
                    "Connectivity test failed: {}: {}",
                    error.code, error.message
                )),
                Err(_) => {
                    // If the error string can not be deserialized as xml, return the enterity of
                    // the error back.
                    Err(format!(
                        "Connectivity test failed: {}",
                        std::str::from_utf8(response.body()).unwrap()
                    ))
                }
            }
        }
        Err(OAError::TimeoutError(_)) => {
            Err("Connectivity test failed with a timeout.".to_string())
        }
        Err(OAError::RequestError(RequestError::Service(err))) => Err(format!(
            "Connectivity test failed due to a service error: {}",
            err
        )),
        Err(OAError::RequestError(RequestError::Credentials(err))) => Err(format!(
            "Connectivity test failed due to a credentials error: {}",
            err
        )),
        Err(err) => Err(format!("Connectivity test failed: {}", err)),
        Ok(_) => {
            object_access.delete_object(file).await;
            Ok(())
        }
    }
}

pub fn test_connectivity(
    endpoint: String,
    region: String,
    bucket: String,
    aws_access_key_id: Option<String>,
    aws_secret_access_key: Option<String>,
    aws_instance_profile: bool,
) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .thread_name("zoa_test_connectivity")
        .build()
        .unwrap()
        .block_on(async move {
            let credentials = if aws_instance_profile {
                ObjectAccessCredentials::ManagedCredentials
            } else {
                ObjectAccessCredentials::Key {
                    access_key_id: aws_access_key_id.unwrap(),
                    secret_access_key: aws_secret_access_key.unwrap(),
                }
            };
            let object_access = ObjectAccess::new(
                ObjectAccessProtocol::S3 {
                    endpoint: endpoint.to_string(),
                    region: region.to_string(),
                },
                bucket.to_string(),
                credentials,
                false,
            );

            std::process::exit(match do_test_connectivity(&object_access).await {
                Err(err) => {
                    writeln_stderr!("{}", err);
                    1
                }
                Ok(_) => {
                    writeln_stdout!("Connectivity test succeeded.");
                    0
                }
            });
        });
}
