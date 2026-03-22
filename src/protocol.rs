use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionOptionMessage {
    pub index: usize,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummaryMessage {
    pub workspace_id: String,
    pub workspace_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientToServerMessage {
    Hello {
        device_id: String,
        device_name: String,
        client_version: String,
        capabilities: Vec<String>,
        workspaces: Vec<WorkspaceSummaryMessage>,
        auth_token: String,
    },
    Ready {
        agent_name: String,
    },
    Info {
        message: String,
    },
    SessionCreated {
        session_id: String,
        client_session_id: String,
        workspace_id: String,
        workspace_name: String,
    },
    SessionResumed {
        session_id: String,
        client_session_id: String,
    },
    SessionResumeFailed {
        session_id: String,
        client_session_id: String,
        message: String,
    },
    OutputChunk {
        session_id: String,
        text: String,
    },
    PromptFinished {
        session_id: String,
        stop_reason: String,
    },
    PermissionRequested {
        request_id: String,
        session_id: Option<String>,
        options: Vec<PermissionOptionMessage>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToClientMessage {
    CreateSession {
        session_id: String,
        workspace_id: String,
    },
    ResumeSession {
        session_id: String,
        client_session_id: String,
        workspace_id: String,
    },
    Prompt {
        session_id: Option<String>,
        text: String,
        create_session_if_missing: bool,
    },
    ResolvePermission {
        request_id: String,
        selected_index: usize,
    },
    CancelSession {
        session_id: String,
    },
    Ping,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStatusMessage {
    pub device_id: String,
    pub device_name: String,
    pub connected: bool,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceSummaryMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaySessionSummaryMessage {
    pub session_id: String,
    pub device_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventMessage {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub seq_end: Option<u64>,
    pub session_id: String,
    pub timestamp: u64,
    pub kind: String,
    pub text: Option<String>,
    pub stop_reason: Option<String>,
    pub request_id: Option<String>,
    pub options: Option<Vec<PermissionOptionMessage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserToServerMessage {
    ListDevices,
    ListSessions,
    CreateSession {
        device_id: String,
        workspace_id: String,
    },
    AdoptSession {
        session_id: String,
    },
    CloseSession {
        session_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    Prompt {
        session_id: String,
        text: String,
    },
    GetSessionHistory {
        session_id: String,
        before_seq: Option<u64>,
        limit: Option<usize>,
    },
    ResolvePermission {
        request_id: String,
        selected_index: usize,
    },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToBrowserMessage {
    DeviceList {
        devices: Vec<DeviceStatusMessage>,
    },
    SessionList {
        sessions: Vec<RelaySessionSummaryMessage>,
    },
    SessionUpdated {
        session: RelaySessionSummaryMessage,
    },
    SessionRemoved {
        session_id: String,
    },
    SessionHistory {
        session_id: String,
        events: Vec<SessionEventMessage>,
        next_before_seq: Option<u64>,
        has_more: bool,
    },
    DeviceStatus {
        device: DeviceStatusMessage,
    },
    SessionCreated {
        session_id: String,
        device_id: String,
        workspace_id: String,
        workspace_name: String,
    },
    SessionClosed {
        session_id: String,
    },
    OutputChunk {
        session_id: String,
        text: String,
    },
    PromptFinished {
        session_id: String,
        stop_reason: String,
    },
    PermissionRequested {
        request_id: String,
        session_id: Option<String>,
        options: Vec<PermissionOptionMessage>,
    },
    Info {
        message: String,
    },
    Error {
        session_id: Option<String>,
        message: String,
    },
}
