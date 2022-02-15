//! zcache list_devices subcommand

use crate::remote_channel::{RemoteChannel, RemoteError};
use crate::subcommand::ZcacheSubCommand;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use clap::{Arg, SubCommand};
use std::fs;
use std::path::{Path, PathBuf};
use util::{nice_p2size, DeviceEntry, DeviceList};
use util::{write_stdout, writeln_stdout};

static NAME: &str = "list_devices";

struct DeviceDisplay {
    real_paths: bool,
    full_paths: bool,
    show_size: bool,
    json_output: bool,
}

impl DeviceDisplay {
    /// Derive the device name to display based on command input flags.
    fn derive_name(&self, path: &str) -> String {
        let path_buf: PathBuf;

        let device_path = if self.real_paths {
            // Follow any symlinks to get the underlying device
            // e.g. "/dev/xvdz1" -> "/dev/nvme1n1p1"
            path_buf = fs::canonicalize(path).unwrap();
            path_buf.as_path()
        } else {
            Path::new(path)
        };

        if self.full_paths {
            device_path.to_str().unwrap()
        } else {
            device_path.file_name().unwrap().to_str().unwrap()
        }
        .to_string()
    }

    fn max_name_length(&self, devices: &[DeviceEntry]) -> usize {
        devices
            .iter()
            .map(|d| self.derive_name(&d.name).len())
            .max()
            .unwrap()
    }

    fn display_devices(&self, devices: &DeviceList) {
        let name_width = self.max_name_length(&devices.devices);

        for device in &devices.devices {
            write_stdout!("{:<1$}  ", self.derive_name(&device.name), name_width);
            if self.show_size {
                write_stdout!("{:>6}", nice_p2size(device.size));
            }
            writeln_stdout!();
        }
    }

    async fn list_devices(&self) -> Result<()> {
        let mut remote = RemoteChannel::new(false).await?;

        match remote.call(NAME, None).await {
            Ok(response) => {
                let devices_json = response.lookup_string("devices_json")?;
                let devices: DeviceList = serde_json::from_str(devices_json.to_str()?)?;

                if self.json_output {
                    writeln_stdout!("{}", serde_json::to_string_pretty(&devices)?)
                } else {
                    self.display_devices(&devices);
                }
            }
            Err(RemoteError::ResultError(_)) => {
                return Err(anyhow!("No cache found"));
            }
            Err(RemoteError::Other(e)) => {
                return Err(e).context("remote call error");
            }
        }
        Ok(())
    }
}

pub struct ListDevices;

#[async_trait]
impl ZcacheSubCommand for ListDevices {
    fn subcommand(&self) -> clap::App<'static, 'static> {
        SubCommand::with_name(NAME)
            .about("Display zettacache devices.")
            .arg(
                Arg::with_name("full-paths")
                    .long("full-paths")
                    .short("f")
                    .help(
                        "Display full paths for device instead of only the last component of \
                        the path. This can be used in conjunction with the real-paths (-r) flag.",
                    ),
            )
            .arg(
                Arg::with_name("real-paths")
                    .long("real-paths")
                    .short("r")
                    .help("Display real paths for devices resolving all symbolic links."),
            )
            .arg(
                Arg::with_name("size")
                    .long("size")
                    .short("s")
                    .help("Display device capacity in human readable form."),
            )
            .arg(
                Arg::with_name("json")
                    .long("json")
                    .short("j")
                    .help("Use JSON output format.")
                    .conflicts_with("full-paths")
                    .conflicts_with("real-paths")
                    .conflicts_with("size"),
            )
    }

    fn name(&self) -> String {
        NAME.to_string()
    }

    async fn invoke(&mut self, args: &clap::ArgMatches) -> Result<()> {
        DeviceDisplay {
            real_paths: args.is_present("real-paths"),
            full_paths: args.is_present("full-paths"),
            show_size: args.is_present("size"),
            json_output: args.is_present("json"),
        }
        .list_devices()
        .await
    }
}
