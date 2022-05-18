//! `zcache labelclear` subcommand

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use util::message::SUPERBLOCK_SIZE;
use util::writeln_stdout;
use util::DeviceList;

use crate::list::List;
use crate::subcommand::ZcacheSubCommand;

#[derive(Parser)]
#[clap(about = "Clear zettacache labels")]
pub struct Labelclear {
    /// Force labels to be cleared even if the agent can't be contacted to determine if the device
    /// is in use.
    #[clap(short = 'f', long)]
    force: bool,

    /// Device/file whose label is to be cleared.
    path: PathBuf,
}

#[async_trait]
impl ZcacheSubCommand for Labelclear {
    async fn invoke(&self) -> Result<()> {
        let canonical_path = fs::canonicalize(&self.path)
            .with_context(|| format!("failed to canonicalize path {:?}", self.path))?;

        let devices = match List::get_device_list().await {
            Ok(devices) => devices,
            Err(e) => {
                if self.force {
                    DeviceList::default()
                } else {
                    return Err(e).context("failed to get device list");
                }
            }
        };
        let in_use_paths = devices
            .devices
            .iter()
            .map(|d| fs::canonicalize(&d.name).unwrap_or_else(|_| d.name.clone()))
            .collect::<HashSet<_>>();

        if in_use_paths.contains(&canonical_path) {
            return Err(anyhow!(
                "{:?} ({canonical_path:?}) is in use by running agent",
                self.path
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&canonical_path)
            .with_context(|| format!("opening disk '{}'", canonical_path.display()))?;

        file.write_all(&vec![0u8; SUPERBLOCK_SIZE])
            .with_context(|| format!("writing to disk '{}'", canonical_path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing disk '{}'", canonical_path.display()))?;

        writeln_stdout!("cleared label of {:?} ({canonical_path:?})", self.path);

        Ok(())
    }
}
