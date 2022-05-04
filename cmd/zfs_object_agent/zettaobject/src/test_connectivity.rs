use std::time::Duration;

use rand::Rng;
use serde::Deserialize;
use util::writeln_stderr;
use util::writeln_stdout;

use crate::access_stats::ObjectAccessOpType;
use crate::object_access::OAError;
use crate::object_access::ObjectAccess;
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
            /*
             * The Byte-Order-Mark (or BOM), is a special marker added at the very beginning of
             * an Unicode file encoded in UTF-8, UTF-16 or UTF-32. It is used to indicate whether
             * the file uses the big-endian or little-endian byte order.
             * Azure-Blob returns xml with a BOM prefix. Since serde_xml_rs does not deal with
             * it, it needs to be trimmed first.
             */
            let body = std::str::from_utf8(response.body()).unwrap();
            if let Some(index) = body.find("<?xml") {
                if let Ok(error) = serde_xml_rs::from_str::<Error>(&body[index..]) {
                    return Err(format!(
                        "Connectivity test failed: {}: {}",
                        error.code, error.message
                    ));
                }
            }

            // If the error string can not be deserialized as xml, return the entirety of
            // the error back.
            Err(format!("Connectivity test failed: {}", body))
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

pub async fn test_connectivity(protocol: ObjectAccessProtocol, bucket: String) {
    let object_access = match ObjectAccess::new(protocol, bucket, false).await {
        Ok(oa) => oa,
        Err(err) => {
            writeln_stderr!("Connectivity test failed: {}", err);
            std::process::exit(1);
        }
    };

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
}
