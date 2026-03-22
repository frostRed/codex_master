use anyhow::Result;
use codex_master::relay_server::{self, RelayServerConfig};
use codex_master::tls;

#[tokio::main]
async fn main() -> Result<()> {
    tls::install_rustls_ring_provider();
    let config = RelayServerConfig::from_env()?;
    relay_server::run(config).await
}
