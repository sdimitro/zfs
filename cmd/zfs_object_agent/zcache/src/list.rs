//! `zcache list` subcommand

use std::fs;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use util::message::TYPE_LIST_DEVICES;
use util::nice_p2size;
use util::write_stdout;
use util::writeln_stdout;
use util::DeviceEntry;
use util::DeviceList;

use crate::remote_channel::RemoteChannel;
use crate::subcommand::ZcacheSubCommand;

#[derive(Parser)]
#[clap(about = "Display zettacache devices")]
#[clap(alias = "list_devices")]
pub struct List {
    #[clap(short = 'f', long, hide(true))]
    full_paths: bool,

    /// Display real paths for devices resolving all symbolic links.
    #[clap(short = 'r', long)]
    real_paths: bool,

    /// Display device capacity in human readable form.
    #[clap(short = 's', long)]
    size: bool,

    /// Use JSON output format.
    #[clap(
        short = 'j',
        long,
        conflicts_with = "full-paths",
        conflicts_with = "real-paths",
        conflicts_with = "size"
    )]
    json: bool,
}

impl List {
    /// Derive the device name to display based on command input flags.
    fn derive_name(&self, path: &Path) -> String {
        let device_path = if self.real_paths {
            // Follow any symlinks to get the underlying device
            // e.g. "/dev/xvdz1" -> "/dev/nvme1n1p1"
            fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
        } else {
            path.to_owned()
        };

        device_path.to_string_lossy().into()
    }

    fn max_name_length(&self, devices: &[DeviceEntry]) -> usize {
        devices
            .iter()
            .map(|d| self.derive_name(&d.name).len())
            .max()
            .unwrap_or_default()
    }

    fn display_devices(&self, devices: &DeviceList) {
        let name_width = self.max_name_length(&devices.devices);

        for device in &devices.devices {
            write_stdout!("{:<1$}  ", self.derive_name(&device.name), name_width);
            if self.size {
                write_stdout!("{:>6}", nice_p2size(device.size));
            }
            writeln_stdout!();
        }
    }

    // Note, if the agent is running but there is no zettacache, this will return Ok(empty_list)
    pub async fn get_device_list() -> Result<DeviceList> {
        let mut remote = RemoteChannel::new(false).await?;

        let response = remote.call(TYPE_LIST_DEVICES, None).await?;
        let devices_json = response.lookup_string("devices_json")?;
        Ok(serde_json::from_str(devices_json.to_str()?)?)
    }

    async fn list_devices(&self) -> Result<()> {
        let devices = Self::get_device_list().await?;
        if self.json {
            writeln_stdout!("{}", serde_json::to_string_pretty(&devices)?)
        } else {
            self.display_devices(&devices);
        }
        Ok(())
    }
}

#[async_trait]
impl ZcacheSubCommand for List {
    async fn invoke(&self) -> Result<()> {
        self.list_devices().await
    }
}
