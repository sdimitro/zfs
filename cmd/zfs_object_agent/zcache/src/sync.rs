//! `zcache sync` subcommand

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use util::message::TYPE_INITIATE_MERGE;
use util::message::TYPE_SYNC_CHECKPOINT;

use crate::remote_channel::RemoteChannel;
use crate::subcommand::ZcacheSubCommand;

#[derive(Parser)]
#[clap(about = "Wait for changes to be persisted to Zettacache.")]
pub struct Sync {
    /// Request index merge.  If a merge is already in progress, a new merge will be started as
    /// soon as this one completes.
    #[clap(long)]
    merge: bool,
}

#[async_trait]
impl ZcacheSubCommand for Sync {
    async fn invoke(&self) -> Result<()> {
        let mut remote = RemoteChannel::new(true).await?;

        if self.merge {
            remote.call(TYPE_INITIATE_MERGE, None).await?;
        } else {
            remote.call(TYPE_SYNC_CHECKPOINT, None).await?;
        };
        Ok(())
    }
}
