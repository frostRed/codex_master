#[derive(Debug, Clone)]
pub struct WorkspaceView {
    pub workspace_id: String,
    pub workspace_name: String,
}

#[derive(Debug, Clone)]
pub enum HomeClientCommand {
    CreateSession {
        session_id: Option<String>,
        workspace_id: Option<String>,
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
    Exit,
}

#[derive(Debug, Clone)]
pub struct PermissionOptionView {
    pub index: usize,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub enum HomeClientEvent {
    Ready {
        agent_name: String,
        workspaces: Vec<WorkspaceView>,
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
        options: Vec<PermissionOptionView>,
    },
    Error {
        message: String,
    },
}
