use std::time::Duration;

use rand::Rng;
use rusoto_core::RusotoError;
use serde::Deserialize;

use crate::OAError;
use crate::ObjectAccess;
use crate::ObjectAccessOpType;

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
        Err(OAError::RequestError(RusotoError::Unknown(bhr))) => {
            match serde_xml_rs::from_str::<Error>(bhr.body_as_str()) {
                Ok(error) => Err(format!(
                    "Connectivity test failed: {}: {}",
                    error.code, error.message
                )),
                Err(_) => {
                    // If the error string can not be deserialized as xml, return the enterity of
                    // the error back.
                    Err(format!("Connectivity test failed: {}", bhr.body_as_str()))
                }
            }
        }
        Err(OAError::TimeoutError(_)) => {
            Err("Connectivity test failed with a timeout.".to_string())
        }
        Err(OAError::RequestError(RusotoError::Service(err))) => Err(format!(
            "Connectivity test failed due to a service error: {}",
            err
        )),
        Err(OAError::RequestError(RusotoError::Credentials(err))) => Err(format!(
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
            let client = if aws_instance_profile {
                ObjectAccess::get_client_with_instance_profile(&endpoint, &region)
            } else {
                // Both aws_access_key_id and aws_secret_access_key should also be specified.
                ObjectAccess::get_client_with_creds(
                    &endpoint,
                    &region,
                    aws_access_key_id.unwrap().as_str(),
                    aws_secret_access_key.unwrap().as_str(),
                )
            };
            let object_access =
                ObjectAccess::from_client(client, &bucket, false, &endpoint, &region);

            std::process::exit(match do_test_connectivity(&object_access).await {
                Err(err) => {
                    eprintln!("{}", err);
                    1
                }
                Ok(_) => {
                    println!("Connectivity test succeeded.");
                    0
                }
            });
        });
}
