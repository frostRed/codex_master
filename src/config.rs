use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceConfig {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
}

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
    pub reconnect_max_delay_secs: u64,
    pub reconnect_jitter_millis: u64,
    pub reconnect_reset_after_secs: u64,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub transport: TransportConfig,
    pub workspaces: Vec<WorkspaceConfig>,
    pub default_workspace_id: String,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let mode = std::env::var("HOME_CLIENT_TRANSPORT")
            .unwrap_or_else(|_| "local".to_string())
            .to_lowercase();
        let current_dir = std::env::current_dir().context("无法获取当前工作目录")?;
        let workspaces = load_workspaces(&current_dir)?;
        let default_workspace_id = resolve_default_workspace_id(&workspaces)?;

        let transport = match mode.as_str() {
            "local" => TransportConfig::Local,
            "relay" => {
                let allow_insecure_ws = std::env::var("HOME_CLIENT_ALLOW_INSECURE_WS")
                    .ok()
                    .map(|value| parse_bool_env("HOME_CLIENT_ALLOW_INSECURE_WS", &value))
                    .transpose()?
                    .unwrap_or(false);
                let relay_url =
                    validate_relay_url(&required_env("HOME_CLIENT_RELAY_URL")?, allow_insecure_ws)?;
                let reconnect_delay_secs =
                    env_u64_or_default("HOME_CLIENT_RECONNECT_DELAY_SECS", 3)?.max(1);
                let reconnect_max_delay_secs =
                    env_u64_or_default("HOME_CLIENT_RECONNECT_MAX_DELAY_SECS", 60)?
                        .max(reconnect_delay_secs);
                let reconnect_jitter_millis =
                    env_u64_or_default("HOME_CLIENT_RECONNECT_JITTER_MILLIS", 750)?;
                let reconnect_reset_after_secs =
                    env_u64_or_default("HOME_CLIENT_RECONNECT_RESET_AFTER_SECS", 30)?.max(1);

                TransportConfig::Relay(RelayTransportConfig {
                    url: relay_url,
                    device_id: std::env::var("HOME_CLIENT_DEVICE_ID")
                        .or_else(|_| std::env::var("HOSTNAME"))
                        .unwrap_or_else(|_| "home-client".to_string()),
                    device_name: std::env::var("HOME_CLIENT_DEVICE_NAME")
                        .or_else(|_| std::env::var("HOSTNAME"))
                        .unwrap_or_else(|_| "Home Client".to_string()),
                    auth_token: required_env("HOME_CLIENT_AUTH_TOKEN")?,
                    reconnect_delay_secs,
                    reconnect_max_delay_secs,
                    reconnect_jitter_millis,
                    reconnect_reset_after_secs,
                })
            }
            other => bail!("不支持的 HOME_CLIENT_TRANSPORT: {other}"),
        };

        Ok(Self {
            transport,
            workspaces,
            default_workspace_id,
        })
    }
}

fn required_env(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("缺少环境变量 {key}"))
}

fn env_u64_or_default(key: &str, default: u64) -> Result<u64> {
    match std::env::var(key) {
        Ok(value) => value
            .parse::<u64>()
            .with_context(|| format!("{key} 必须是非负整数")),
        Err(_) => Ok(default),
    }
}

fn parse_bool_env(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("{key} 必须是布尔值")),
    }
}

fn validate_relay_url(raw: &str, allow_insecure_ws: bool) -> Result<String> {
    let url = raw.trim();
    if url.is_empty() {
        bail!("HOME_CLIENT_RELAY_URL 不能为空");
    }

    let Some((scheme, rest)) = url.split_once("://") else {
        bail!("HOME_CLIENT_RELAY_URL 格式错误，必须使用 wss://...");
    };

    match scheme.to_ascii_lowercase().as_str() {
        "wss" => Ok(url.to_string()),
        "ws" => {
            if !allow_insecure_ws {
                bail!(
                    "HOME_CLIENT_RELAY_URL 必须使用 wss://；仅本地调试可通过 HOME_CLIENT_ALLOW_INSECURE_WS=true 放开 ws://"
                );
            }
            if !is_loopback_target(rest) {
                bail!(
                    "仅允许在 loopback 上使用 ws://（localhost/127.0.0.1/[::1]），当前 HOME_CLIENT_RELAY_URL 不符合要求"
                );
            }
            Ok(url.to_string())
        }
        _ => bail!(
            "HOME_CLIENT_RELAY_URL 仅支持 wss://；本地调试可在 loopback 上配合 HOME_CLIENT_ALLOW_INSECURE_WS=true 使用 ws://"
        ),
    }
}

fn is_loopback_target(url_without_scheme: &str) -> bool {
    let authority = url_without_scheme
        .split('/')
        .next()
        .unwrap_or_default()
        .rsplit('@')
        .next()
        .unwrap_or_default();
    if authority.is_empty() {
        return false;
    }

    let host = if authority.starts_with('[') {
        authority
            .strip_prefix('[')
            .and_then(|rest| rest.split(']').next())
            .unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    }
    .trim()
    .to_ascii_lowercase();

    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
}

fn load_workspaces(current_dir: &Path) -> Result<Vec<WorkspaceConfig>> {
    match std::env::var("HOME_CLIENT_WORKSPACES_JSON") {
        Ok(raw) => validate_workspaces(parse_workspace_json(&raw)?),
        Err(_) => Ok(vec![default_workspace(current_dir)]),
    }
}

fn parse_workspace_json(raw: &str) -> Result<Vec<WorkspaceConfig>> {
    let workspaces = serde_json::from_str::<Vec<WorkspaceConfig>>(raw)
        .context("解析 HOME_CLIENT_WORKSPACES_JSON 失败，应为 JSON 数组")?;
    Ok(workspaces)
}

fn validate_workspaces(workspaces: Vec<WorkspaceConfig>) -> Result<Vec<WorkspaceConfig>> {
    if workspaces.is_empty() {
        bail!("HOME_CLIENT_WORKSPACES_JSON 至少需要包含一个工作区");
    }

    let mut ids = HashSet::new();
    let mut normalized = Vec::with_capacity(workspaces.len());

    for workspace in workspaces {
        if workspace.id.trim().is_empty() {
            bail!("工作区 id 不能为空");
        }
        if workspace.name.trim().is_empty() {
            bail!("工作区 name 不能为空");
        }
        if !workspace.path.is_absolute() {
            bail!("工作区 `{}` 的 path 必须是绝对路径", workspace.id);
        }
        if !ids.insert(workspace.id.clone()) {
            bail!("工作区 id 重复: {}", workspace.id);
        }

        normalized.push(workspace);
    }

    Ok(normalized)
}

fn resolve_default_workspace_id(workspaces: &[WorkspaceConfig]) -> Result<String> {
    if let Ok(default_id) = std::env::var("HOME_CLIENT_DEFAULT_WORKSPACE_ID") {
        if workspaces
            .iter()
            .any(|workspace| workspace.id == default_id)
        {
            return Ok(default_id);
        }
        return Err(anyhow!(
            "HOME_CLIENT_DEFAULT_WORKSPACE_ID 未出现在工作区列表中: {default_id}"
        ));
    }

    workspaces
        .first()
        .map(|workspace| workspace.id.clone())
        .ok_or_else(|| anyhow!("未找到可用工作区"))
}

fn default_workspace(current_dir: &Path) -> WorkspaceConfig {
    let default_name = current_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Current Workspace")
        .to_string();

    WorkspaceConfig {
        id: "default".to_string(),
        name: default_name,
        path: current_dir.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_loopback_target, validate_relay_url};

    #[test]
    fn relay_url_accepts_secure_wss() {
        let url = validate_relay_url("wss://relay.example.com/ws/client", false)
            .expect("wss url should be accepted");
        assert_eq!(url, "wss://relay.example.com/ws/client");
    }

    #[test]
    fn relay_url_rejects_ws_without_explicit_override() {
        let err = validate_relay_url("ws://127.0.0.1:8080/ws/client", false)
            .expect_err("ws should require explicit local override");
        assert!(err.to_string().contains("HOME_CLIENT_ALLOW_INSECURE_WS"));
    }

    #[test]
    fn relay_url_only_allows_ws_for_loopback_when_overridden() {
        validate_relay_url("ws://localhost:8080/ws/client", true)
            .expect("loopback ws should be allowed with override");
        validate_relay_url("ws://127.0.0.1:8080/ws/client", true)
            .expect("loopback ws should be allowed with override");
        validate_relay_url("ws://[::1]:8080/ws/client", true)
            .expect("loopback ws should be allowed with override");
        assert!(validate_relay_url("ws://relay.example.com/ws/client", true).is_err());
    }

    #[test]
    fn loopback_target_parser_handles_authority_forms() {
        assert!(is_loopback_target("localhost:8080/ws/client"));
        assert!(is_loopback_target("user:pass@127.0.0.1:9000/ws/client"));
        assert!(is_loopback_target("[::1]:8080/ws/client"));
        assert!(!is_loopback_target("relay.example.com/ws/client"));
    }
}
