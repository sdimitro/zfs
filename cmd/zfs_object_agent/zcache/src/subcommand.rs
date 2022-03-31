use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait ZcacheSubCommand {
    async fn invoke(&self) -> Result<()>;
}
