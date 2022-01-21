use crate::remote_channel::{RemoteChannel, RemoteError};
use crate::subcommand::ZcacheSubCommand;
use anyhow::Result;
use async_trait::async_trait;
use clap::SubCommand;
use util::writeln_stdout;

static NAME: &str = "clear_hit_data";
pub struct ClearHitData;

#[async_trait]
impl ZcacheSubCommand for ClearHitData {
    fn subcommand(&self) -> clap::App<'static, 'static> {
        SubCommand::with_name(NAME).about("Clear the current hit-by-size histogram")
    }

    fn name(&self) -> String {
        NAME.to_string()
    }

    async fn invoke(&mut self, _args: &clap::ArgMatches) -> Result<()> {
        let mut remote = RemoteChannel::new(true).await?;
        let response = remote.call(NAME, None).await;
        match response {
            Ok(_) => {
                writeln_stdout!("Hits-by-size data cleared");
            }
            Err(RemoteError::ResultError(_)) => {
                writeln_stdout!("No cache found, so no hits-by-size data present");
            }
            Err(RemoteError::Other(e)) => return Err(e),
        }
        Ok(())
    }
}
