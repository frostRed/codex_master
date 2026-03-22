use anyhow::Result;
use codex_master::config::{AppConfig, RelayTransportConfig, TransportConfig};
use codex_master::runtime;
use codex_master::tls;
use codex_master::transport::{LocalDebugTransport, RelayTransport};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, Instant, sleep};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tls::install_rustls_ring_provider();

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
                    let reset_after = Duration::from_secs(relay_config.reconnect_reset_after_secs);
                    let mut consecutive_failures: u32 = 0;
                    loop {
                        let cycle_started = Instant::now();
                        let cycle_failed =
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

                                    cycle_started.elapsed() < reset_after
                                }
                                Err(err) => {
                                    eprintln!("[relay-connect-error] {err}");
                                    true
                                }
                            };

                        if cycle_failed {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                        } else {
                            consecutive_failures = 0;
                        }

                        let delay =
                            reconnect_delay_with_jitter(&relay_config, consecutive_failures.max(1));
                        eprintln!(
                            "[relay-runtime] retrying in {:.3}s (attempt {})",
                            delay.as_secs_f64(),
                            consecutive_failures.max(1)
                        );
                        sleep(delay).await;
                    }
                }
            }
        })
        .await
}

fn reconnect_delay_with_jitter(config: &RelayTransportConfig, failure_attempt: u32) -> Duration {
    let base_secs = config.reconnect_delay_secs.max(1);
    let max_secs = config.reconnect_max_delay_secs.max(base_secs);
    let exponent = failure_attempt.saturating_sub(1).min(20);
    let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let delay_secs = base_secs.saturating_mul(multiplier).min(max_secs);
    let jitter_millis = random_jitter_millis(config.reconnect_jitter_millis);
    Duration::from_secs(delay_secs).saturating_add(Duration::from_millis(jitter_millis))
}

fn random_jitter_millis(max_millis: u64) -> u64 {
    if max_millis == 0 {
        return 0;
    }

    let upper_exclusive = max_millis.saturating_add(1);
    if upper_exclusive == 0 {
        return 0;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    nanos % upper_exclusive
}
