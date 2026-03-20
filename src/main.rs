use anyhow::Result;
use codex_master::config::{AppConfig, TransportConfig};
use codex_master::runtime;
use codex_master::transport::{LocalDebugTransport, RelayTransport};
use tokio::time::{Duration, sleep};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(async {
            let config = AppConfig::from_env()?;
            let workspaces = config.workspaces.clone();
            let default_workspace_id = config.default_workspace_id.clone();
            match config.transport {
                TransportConfig::Local => {
                    let transport = LocalDebugTransport::new();
                    runtime::run_home_client(transport, workspaces, default_workspace_id).await
                }
                TransportConfig::Relay(relay_config) => {
                    let reconnect_delay = Duration::from_secs(relay_config.reconnect_delay_secs);
                    loop {
                        match RelayTransport::connect(relay_config.clone(), workspaces.clone())
                            .await
                        {
                            Ok(transport) => {
                                if let Err(err) = runtime::run_home_client(
                                    transport,
                                    workspaces.clone(),
                                    default_workspace_id.clone(),
                                )
                                .await
                                {
                                    eprintln!("[relay-runtime-error] {err}");
                                } else {
                                    eprintln!("[relay-runtime] connection ended");
                                }
                            }
                            Err(err) => {
                                eprintln!("[relay-connect-error] {err}");
                            }
                        }

                        eprintln!("[relay-runtime] retrying in {}s", reconnect_delay.as_secs());
                        sleep(reconnect_delay).await;
                    }
                }
            }
        })
        .await
}
