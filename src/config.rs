use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub enum TransportConfig {
    Local,
    Relay(RelayTransportConfig),
}

#[derive(Debug, Clone)]
pub struct RelayTransportConfig {
    pub url: String,
    pub device_id: String,
    pub device_name: String,
    pub auth_token: String,
    pub reconnect_delay_secs: u64,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub transport: TransportConfig,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let mode = std::env::var("HOME_CLIENT_TRANSPORT")
            .unwrap_or_else(|_| "local".to_string())
            .to_lowercase();

        let transport = match mode.as_str() {
            "local" => TransportConfig::Local,
            "relay" => TransportConfig::Relay(RelayTransportConfig {
                url: required_env("HOME_CLIENT_RELAY_URL")?,
                device_id: std::env::var("HOME_CLIENT_DEVICE_ID")
                    .or_else(|_| std::env::var("HOSTNAME"))
                    .unwrap_or_else(|_| "home-client".to_string()),
                device_name: std::env::var("HOME_CLIENT_DEVICE_NAME")
                    .or_else(|_| std::env::var("HOSTNAME"))
                    .unwrap_or_else(|_| "Home Client".to_string()),
                auth_token: required_env("HOME_CLIENT_AUTH_TOKEN")?,
                reconnect_delay_secs: std::env::var("HOME_CLIENT_RECONNECT_DELAY_SECS")
                    .ok()
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(3),
            }),
            other => bail!("不支持的 HOME_CLIENT_TRANSPORT: {other}"),
        };

        Ok(Self { transport })
    }
}

fn required_env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("缺少环境变量 {key}"))
}
