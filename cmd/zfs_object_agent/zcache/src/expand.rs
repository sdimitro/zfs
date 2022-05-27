//! `zcache expand` subcommand

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use util::message::ExpandDiskRequest;
use util::message::ExpandDiskResponse;
use util::message::TYPE_EXPAND_DISK;
use util::nice_p2size;
use util::writeln_stdout;

use crate::remote_channel::RemoteChannel;
use crate::subcommand::ZcacheSubCommand;

#[derive(Parser)]
#[clap(about = "Expand a disk in the ZettaCache.")]
pub struct Expand {
    path: PathBuf,
}

#[async_trait]
impl ZcacheSubCommand for Expand {
    async fn invoke(&self) -> Result<()> {
        let mut remote = RemoteChannel::new(true).await?;

        let request = ExpandDiskRequest {
            path: self.path.clone(),
        };

        let nvlist = remote
            .call(TYPE_EXPAND_DISK, Some(nvpair::to_nvlist(&request).unwrap()))
            .await?;
        let response: ExpandDiskResponse = nvpair::from_nvlist(&nvlist)?;
        if response.additional_bytes > 0 {
            writeln_stdout!(
                "Disk {:?} expanded, new size {} (added {})",
                self.path,
                nice_p2size(response.new_size),
                nice_p2size(response.additional_bytes)
            );
        } else {
            writeln_stdout!(
                "Disk {:?} expansion not needed, size {}",
                self.path,
                nice_p2size(response.new_size)
            );
        }
        Ok(())
    }
}
