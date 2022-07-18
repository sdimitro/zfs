//! `zcache status` subcommand

use std::thread::sleep;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Local;
use clap::Parser;
use util::flush_stdout;
use util::message::TYPE_ZCACHE_STATUS;
use util::nice_p2size;
use util::write_stdout;
use util::writeln_stdout;
use util::DeviceStatus;
use util::ZcacheStatus;

use crate::remote_channel::RemoteChannel;
use crate::subcommand::ZcacheSubCommand;

#[derive(Parser)]
#[clap(about = "Display zettacache status.")]
pub struct Status {
    /// Display a timestamp
    #[clap(short = 't', long)]
    timestamp: bool,

    /// Display real paths for devices resolving all symbolic links.
    #[clap(short = 'r', long)]
    real_paths: bool,

    /// Use JSON output format.
    #[clap(
        short = 'j',
        long,
        conflicts_with = "timestamp",
        conflicts_with = "real-paths"
    )]
    json: bool,

    /// Status is printed every <interval> seconds
    #[clap()]
    interval: Option<f64>,

    /// Stop after <count> status reports have been displayed
    #[clap()]
    count: Option<u64>,
}

impl Status {
    // Maximum width from the set of possible keys ("devices", "index", etc.)
    const MAXIMUM_KEY_WIDTH: usize = 7;

    /// Derive the device path to display
    fn derive_path(&self, device_status: &DeviceStatus) -> String {
        let device_path = if self.real_paths {
            &device_status.info.canonical_path
        } else {
            &device_status.info.path
        };
        device_path.to_string_lossy().into()
    }

    fn max_path_length(&self, devices: &[DeviceStatus]) -> usize {
        devices
            .iter()
            .map(|d| self.derive_path(d).len())
            .max()
            .unwrap_or_default()
    }

    fn cli_status(&self, status: &ZcacheStatus) {
        let now = Local::now();
        if self.timestamp {
            writeln_stdout!("{}", now.to_rfc2822());
        }

        // Display index status
        writeln_stdout!(
            "{:>3$}: current size {} with {} pending changes",
            "index",
            nice_p2size(status.index.bytes),
            status.index.pending_changes,
            Status::MAXIMUM_KEY_WIDTH
        );

        // Display devices status
        let path_width = self.max_path_length(&status.devices);
        writeln_stdout!("{:>1$}:", "devices", Status::MAXIMUM_KEY_WIDTH);
        for device_status in status.devices.iter() {
            // example: "         /dev/nvme6n1p2  56.0GB  <status>"
            write_stdout!(
                "{:>3$}{:<4$}  {:>6}",
                "",
                self.derive_path(device_status),
                nice_p2size(device_status.info.size),
                Status::MAXIMUM_KEY_WIDTH + 2,
                path_width,
            );

            let status = match &device_status.removal {
                Some(removal) => {
                    let removal_status = if removal.currently_removing_device {
                        if let Some(start_time) = removal.start_time {
                            let done_pct = (1.0
                                - (removal.space_left_to_evacuate as f64
                                    / removal.total_space_to_evacuate as f64))
                                * 100.0;
                            let throughput = ((removal.total_space_to_evacuate
                                - removal.space_left_to_evacuate)
                                / u64::try_from((now - start_time).num_seconds()).unwrap())
                                / (1024 * 1024);
                            format!(
                                "evacuating: moving {} of data - {done_pct:.0}% done, {throughput}MB/s",
                                nice_p2size(removal.total_space_to_evacuate),
                            )
                        } else {
                            let done_pct = (1.0
                                - (removal.space_left_to_evict as f64
                                    / removal.total_space_to_evacuate as f64))
                                * 100.0;
                            format!("evicting: index merge {done_pct:.0}% done")
                        }
                    } else {
                        "waiting for other removals".to_string()
                    };
                    format!("REMOVING ({removal_status})")
                }
                None => "ONLINE".to_string(),
            };
            write_stdout!("  {status}");

            writeln_stdout!();
        }
    }

    fn json_status(&self, status: &ZcacheStatus) {
        writeln_stdout!("{}", serde_json::to_string_pretty(&status).unwrap());
    }

    async fn display_status(&self) -> Result<()> {
        let interval = self.interval.map(Duration::from_secs_f64);
        let mut iteration = 0;
        let mut remote = RemoteChannel::new(false).await?;

        loop {
            let response = remote.call(TYPE_ZCACHE_STATUS, None).await?;
            let status_json = response.lookup_string("status_json")?;
            let status: ZcacheStatus = serde_json::from_str(status_json.to_str()?)?;
            if self.json {
                self.json_status(&status);
            } else {
                self.cli_status(&status);
                flush_stdout()?;
            }

            let interval: Duration = match interval {
                None => return Ok(()),
                Some(interval) => interval,
            };

            iteration += 1;
            if let Some(count) = self.count {
                if iteration >= count {
                    return Ok(());
                }
            }
            sleep(interval);
        }
    }
}

#[async_trait]
impl ZcacheSubCommand for Status {
    async fn invoke(&self) -> Result<()> {
        self.display_status().await
    }
}
