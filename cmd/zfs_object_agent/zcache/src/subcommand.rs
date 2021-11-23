use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ZcacheSubCommand {
    fn subcommand(&self) -> clap::App<'static, 'static>;
    fn name(&self) -> String;
    async fn invoke(&mut self, args: &clap::ArgMatches) -> Result<()>;
}
