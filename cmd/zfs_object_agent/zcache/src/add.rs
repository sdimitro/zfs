//! `zcache add` subcommand

use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use util::message::AddDiskRequest;
use util::message::TYPE_ADD_DISK;
use util::writeln_stdout;

use crate::remote_channel::RemoteChannel;
use crate::remote_channel::RemoteError;
use crate::subcommand::ZcacheSubCommand;

#[derive(Parser)]
#[clap(about = "Add a disk to the ZettaCache.")]
pub struct Add {
    path: PathBuf,
}

#[async_trait]
impl ZcacheSubCommand for Add {
    async fn invoke(&self) -> Result<()> {
        let mut remote = RemoteChannel::new(true).await?;

        let request = AddDiskRequest {
            path: self.path.clone(),
        };

        match remote
            .call(TYPE_ADD_DISK, Some(nvpair::to_nvlist(&request).unwrap()))
            .await
        {
            Ok(_) => {
                writeln_stdout!("Disk {:?} added", self.path);
            }
            Err(RemoteError::ResultError(_)) => return Err(anyhow!("No cache found")),
            Err(RemoteError::Other(e)) => return Err(e),
        }
        Ok(())
    }
}
