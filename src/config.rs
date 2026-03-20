use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

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

fn load_workspaces(current_dir: &PathBuf) -> Result<Vec<WorkspaceConfig>> {
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

fn default_workspace(current_dir: &PathBuf) -> WorkspaceConfig {
    let default_name = current_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Current Workspace")
        .to_string();

    WorkspaceConfig {
        id: "default".to_string(),
        name: default_name,
        path: current_dir.clone(),
    }
}
