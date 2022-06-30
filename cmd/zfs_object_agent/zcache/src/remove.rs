//! `zcache remove` subcommand

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use util::message::RemoveDiskRequest;
use util::message::TYPE_CANCEL_DISK_REMOVAL;
use util::message::TYPE_PAUSE_DISK_REMOVALS;
use util::message::TYPE_REMOVE_DISK;
use util::message::TYPE_RESUME_DISK_REMOVALS;
use util::writeln_stdout;

use crate::remote_channel::RemoteChannel;
use crate::subcommand::ZcacheSubCommand;

#[derive(Parser)]
#[clap(about = "Remove disks from the ZettaCache.")]
pub struct Remove {
    /// Pause ongoing removals
    #[clap(
        long,
        conflicts_with_all = &["resume", "cancel", "path"]
    )]
    pause: bool,

    /// Resume paused removals
    #[clap(
        long,
        conflicts_with_all = &["cancel", "path"]
    )]
    resume: bool,

    /// Stop and cancel in-progress removal
    #[clap(short = 's', long)]
    cancel: bool,

    /// Supplied disk for the operation issued
    #[clap(required_unless_present_any = &["pause", "resume"])]
    path: Option<PathBuf>,
}

#[async_trait]
impl ZcacheSubCommand for Remove {
    async fn invoke(&self) -> Result<()> {
        let mut remote = RemoteChannel::new(true).await?;
        if self.pause {
            remote.call(TYPE_PAUSE_DISK_REMOVALS, None).await?;
            writeln_stdout!("Removals paused");
        } else if self.resume {
            remote.call(TYPE_RESUME_DISK_REMOVALS, None).await?;
            writeln_stdout!("Resuming removals");
        } else if self.cancel {
            let path = self.path.clone().unwrap();
            let request = RemoveDiskRequest { path: path.clone() };
            remote
                .call(
                    TYPE_CANCEL_DISK_REMOVAL,
                    Some(nvpair::to_nvlist(&request).unwrap()),
                )
                .await?;
            writeln_stdout!("Cancelled the removal of {path:?}");
        } else {
            let path = self.path.clone().unwrap();
            let request = RemoveDiskRequest { path: path.clone() };
            remote
                .call(TYPE_REMOVE_DISK, Some(nvpair::to_nvlist(&request).unwrap()))
                .await?;
            writeln_stdout!("Removing {path:?}");
        }
        Ok(())
    }
}
