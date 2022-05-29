use std::time::Duration;

use futures::TryStreamExt;
use rand::Rng;
use serde::Deserialize;
use util::writeln_stderr;
use util::writeln_stdout;

use crate::access_stats::ObjectAccessOpType;
use crate::object_access::OAError;
use crate::object_access::ObjectAccess;
use crate::object_access::ObjectAccessProtocol;
use crate::object_access::PutError;
use crate::object_access::RequestError;

#[derive(Debug, Deserialize)]
struct Error {
    #[serde(rename = "Code")]
    code: String,
    #[serde(rename = "Message")]
    message: String,
}

async fn create_object_test(object_access: &ObjectAccess, key: String) -> Result<(), String> {
    let content = "test connectivity to object storage".as_bytes().to_vec();

    match object_access
        .put_object_timed(
            key.clone(),
            content.into(),
            ObjectAccessOpType::MetadataPut,
            Some(Duration::from_secs(30)),
        )
        .await
    {
        Err(OAError::RequestError(RequestError::Service(e @ PutError::NoSuchContainer(_)))) => {
            Err(format!("Connectivity test failed: {}", e))
        }
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
                        "unable to create: {}, {}",
                        error.code, error.message
                    ));
                }
            }

            // If the error string can not be deserialized as xml, return the entirety of
            // the error back.
            Err(format!("unable to create: {}", body))
        }
        Err(OAError::TimeoutError(_)) => Err("connection timed out.".to_string()),
        Err(OAError::RequestError(RequestError::Credentials(err))) => {
            Err(format!("credentials error: {}", err))
        }
        Err(err) => Err(format!("{}", err)),
        Ok(_) => Ok(()),
    }
}

// Test by writing and deleting an object.
async fn do_test_connectivity(object_access: &ObjectAccess) -> Result<(), String> {
    let num: u64 = rand::thread_rng().gen();
    let prefix = String::from("test/");
    let file = format!("{}test_connectivity_{}", prefix, num);

    create_object_test(object_access, file.clone()).await?;

    if let Err(e) = object_access
        .get_object(file.clone(), ObjectAccessOpType::MetadataGet)
        .await
    {
        return Err(format!("unable to find object: {e}"));
    }

    let objects = object_access
        .try_list_objects(prefix, None, false)
        .try_collect::<Vec<_>>()
        .await;

    let objects = match objects {
        Ok(objects) => objects,
        Err(e) => {
            let content = e.to_string();
            if let Ok(error) = serde_xml_rs::from_str::<Error>(&content) {
                return Err(format!("unable to list objects: {}", error.message));
            } else {
                return Err(format!("unable to list objects: {}", content));
            }
        }
    };

    if objects != vec![file.clone()] {
        return Err(format!(
            "object listing mismatch, expected {}, got {:?}",
            file, objects
        ));
    }

    object_access.delete_object(file.clone()).await;
    if object_access.object_exists(file.clone()).await {
        return Err("unable to delete objects".to_string());
    }

    Ok(())
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
            writeln_stderr!("Connectivity test failed: {}", err);
            1
        }
        Ok(_) => {
            writeln_stdout!("Connectivity test succeeded.");
            0
        }
    });
}
