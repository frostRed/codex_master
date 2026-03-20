use anyhow::Result;
use codex_master::relay_server::{self, RelayServerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    let config = RelayServerConfig::from_env()?;
    relay_server::run(config).await
}
