use std::any::type_name;

use anyhow::Context;
use anyhow::Result;
use serde::de::DeserializeOwned;

pub fn from_json_slice<T: DeserializeOwned>(slice: &[u8]) -> Result<T> {
    let t = type_name::<T>();
    serde_json::from_slice(slice).with_context(|| match std::str::from_utf8(slice) {
        Ok(utf) => format!("could not decode JSON as {t}: {utf}"),
        Err(e) => format!("could not decode JSON as {t}: [non-UTF slice: {e}]",),
    })
}
