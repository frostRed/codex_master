use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum HomeClientCommand {
    CreateSession {
        session_id: Option<String>,
        cwd: Option<PathBuf>,
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
    },
    Info {
        message: String,
    },
    SessionCreated {
        session_id: String,
        cwd: PathBuf,
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
