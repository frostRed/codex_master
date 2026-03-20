use crate::config::WorkspaceConfig;
use crate::transport::ClientTransport;
use crate::types::{HomeClientCommand, HomeClientEvent, PermissionOptionView, WorkspaceView};
use agent_client_protocol::{
    Agent, Client, ClientSideConnection, ContentBlock, Implementation, InitializeRequest,
    NewSessionRequest, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionNotification, SessionUpdate,
};
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::process::Stdio;
use std::rc::Rc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStderr, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

#[derive(Debug)]
enum InternalEvent {
    OutputChunk {
        session_id: String,
        text: String,
    },
    PermissionRequested {
        request_id: String,
        session_id: Option<String>,
        options: Vec<PermissionOptionView>,
    },
    PromptFinished {
        session_id: String,
        stop_reason: String,
    },
    PromptFailed {
        session_id: String,
        message: String,
    },
    RuntimeError {
        message: String,
    },
}

struct RuntimeClientState {
    active_prompt_session: Option<String>,
    next_permission_request_id: u64,
    pending_permissions: HashMap<String, oneshot::Sender<usize>>,
    internal_tx: mpsc::UnboundedSender<InternalEvent>,
}

#[derive(Clone)]
struct RuntimeClient {
    state: Rc<RefCell<RuntimeClientState>>,
}

impl RuntimeClient {
    fn new(internal_tx: mpsc::UnboundedSender<InternalEvent>) -> Self {
        Self {
            state: Rc::new(RefCell::new(RuntimeClientState {
                active_prompt_session: None,
                next_permission_request_id: 1,
                pending_permissions: HashMap::new(),
                internal_tx,
            })),
        }
    }

    fn begin_prompt(&self, session_id: &str) {
        self.state.borrow_mut().active_prompt_session = Some(session_id.to_string());
    }

    fn end_prompt(&self) {
        self.state.borrow_mut().active_prompt_session = None;
    }

    fn resolve_permission(&self, request_id: &str, selected_index: usize) -> bool {
        let sender = self
            .state
            .borrow_mut()
            .pending_permissions
            .remove(request_id);

        if let Some(sender) = sender {
            let _ = sender.send(selected_index);
            true
        } else {
            false
        }
    }
}

#[async_trait::async_trait(?Send)]
impl Client for RuntimeClient {
    async fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> agent_client_protocol::Result<RequestPermissionResponse> {
        let (request_id, session_id, rx, internal_tx) = {
            let mut state = self.state.borrow_mut();
            let request_id = format!("perm-{}", state.next_permission_request_id);
            state.next_permission_request_id += 1;

            let (tx, rx) = oneshot::channel();
            state.pending_permissions.insert(request_id.clone(), tx);

            (
                request_id,
                state.active_prompt_session.clone(),
                rx,
                state.internal_tx.clone(),
            )
        };

        let options = args
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| PermissionOptionView {
                index,
                name: option.name.clone(),
                kind: format!("{:?}", option.kind),
            })
            .collect::<Vec<_>>();

        let _ = internal_tx.send(InternalEvent::PermissionRequested {
            request_id: request_id.clone(),
            session_id,
            options,
        });

        let selected_index = match rx.await {
            Ok(selected_index) => selected_index,
            Err(_) => {
                return Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
            }
        };

        let outcome = match args.options.get(selected_index) {
            Some(option) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
            None => RequestPermissionOutcome::Cancelled,
        };

        Ok(RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(
        &self,
        args: SessionNotification,
    ) -> agent_client_protocol::Result<()> {
        if let SessionUpdate::AgentMessageChunk(chunk) = args.update
            && let ContentBlock::Text(text) = chunk.content
        {
            let _ = self
                .state
                .borrow()
                .internal_tx
                .send(InternalEvent::OutputChunk {
                    session_id: args.session_id.to_string(),
                    text: text.text,
                });
        }

        Ok(())
    }
}

pub async fn run_home_client<T: ClientTransport>(
    mut transport: T,
    workspaces: Vec<WorkspaceConfig>,
    default_workspace_id: String,
) -> Result<()> {
    let codex_acp_bin = std::env::var("CODEX_ACP_BIN").unwrap_or_else(|_| "codex-acp".to_string());

    let mut child = Command::new(&codex_acp_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("启动 `{codex_acp_bin}` 失败，请先安装 codex-acp"))?;

    let child_stdin = child.stdin.take().context("无法获取 codex-acp stdin")?;
    let child_stdout = child.stdout.take().context("无法获取 codex-acp stdout")?;
    let child_stderr = child.stderr.take().context("无法获取 codex-acp stderr")?;

    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(stream_stderr(child_stderr, internal_tx.clone()));

    let runtime_client = RuntimeClient::new(internal_tx.clone());
    let (agent, io_task) = ClientSideConnection::new(
        runtime_client.clone(),
        child_stdin.compat_write(),
        child_stdout.compat(),
        |future| {
            tokio::task::spawn_local(future);
        },
    );
    let agent = Rc::new(agent);

    let runtime_error_tx = internal_tx.clone();
    tokio::task::spawn_local(async move {
        if let Err(err) = io_task.await {
            let _ = runtime_error_tx.send(InternalEvent::RuntimeError {
                message: format!("acp-io 错误: {err}"),
            });
        }
    });

    let init_response =
        agent
            .initialize(InitializeRequest::new(ProtocolVersion::LATEST).client_info(
                Implementation::new("codex-home-client", "0.1.0").title("Home Client"),
            ))
            .await
            .context("ACP 初始化失败")?;

    let agent_name = init_response
        .agent_info
        .as_ref()
        .and_then(|info| info.title.as_deref().or(Some(info.name.as_str())))
        .unwrap_or("codex-acp")
        .to_string();

    transport
        .publish_event(HomeClientEvent::Ready {
            agent_name,
            workspaces: workspaces
                .iter()
                .map(|workspace| WorkspaceView {
                    workspace_id: workspace.id.clone(),
                    workspace_name: workspace.name.clone(),
                })
                .collect(),
        })
        .await?;
    transport
        .publish_event(HomeClientEvent::Info {
            message: "当前为本地 debug transport；下一步会接入远程 relay transport。".to_string(),
        })
        .await?;

    let mut sessions = HashMap::new();
    let mut prompt_in_flight = false;

    loop {
        tokio::select! {
            maybe_command = transport.next_command() => {
                let Some(command) = maybe_command else {
                    break;
                };

                if !handle_command(
                    &mut transport,
                    &workspaces,
                    &default_workspace_id,
                    agent.clone(),
                    runtime_client.clone(),
                    internal_tx.clone(),
                    &mut sessions,
                    &mut prompt_in_flight,
                    command,
                ).await? {
                    break;
                }
            }
            maybe_event = internal_rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };

                handle_internal_event(&mut transport, &mut prompt_in_flight, event).await?;
            }
        }
    }

    let _ = child.start_kill();
    let _ = child.wait().await;

    Ok(())
}

async fn handle_command<T: ClientTransport>(
    transport: &mut T,
    workspaces: &[WorkspaceConfig],
    default_workspace_id: &str,
    agent: Rc<ClientSideConnection>,
    runtime_client: RuntimeClient,
    internal_tx: mpsc::UnboundedSender<InternalEvent>,
    sessions: &mut HashMap<String, SessionId>,
    prompt_in_flight: &mut bool,
    command: HomeClientCommand,
) -> Result<bool> {
    match command {
        HomeClientCommand::CreateSession {
            session_id,
            workspace_id,
        } => {
            let workspace = resolve_workspace(
                workspaces,
                workspace_id.as_deref(),
                Some(default_workspace_id),
            )?;
            let acp_session_id = create_session(agent.as_ref(), workspace).await?;
            let session_ref = session_id.unwrap_or_else(|| acp_session_id.to_string());
            sessions.insert(session_ref.clone(), acp_session_id);
            transport
                .publish_event(HomeClientEvent::SessionCreated {
                    session_id: session_ref,
                    workspace_id: workspace.id.clone(),
                    workspace_name: workspace.name.clone(),
                })
                .await?;
            Ok(true)
        }
        HomeClientCommand::Prompt {
            session_id,
            text,
            create_session_if_missing,
        } => {
            if *prompt_in_flight {
                transport
                    .publish_event(HomeClientEvent::Error {
                        message: "当前已有一个进行中的 prompt，请等待完成后再发送。".to_string(),
                    })
                    .await?;
                return Ok(true);
            }

            let (session_ref, session_id) = match resolve_session(
                transport,
                workspaces,
                default_workspace_id,
                agent.as_ref(),
                sessions,
                session_id,
                create_session_if_missing,
            )
            .await?
            {
                Some(session) => session,
                None => return Ok(true),
            };

            *prompt_in_flight = true;
            runtime_client.begin_prompt(&session_ref);

            let agent = agent.clone();
            let runtime_client = runtime_client.clone();
            tokio::task::spawn_local(async move {
                let prompt_result = agent
                    .prompt(PromptRequest::new(session_id, vec![text.into()]))
                    .await;

                runtime_client.end_prompt();

                match prompt_result {
                    Ok(response) => {
                        let _ = internal_tx.send(InternalEvent::PromptFinished {
                            session_id: session_ref,
                            stop_reason: format!("{:?}", response.stop_reason),
                        });
                    }
                    Err(err) => {
                        let _ = internal_tx.send(InternalEvent::PromptFailed {
                            session_id: session_ref,
                            message: err.to_string(),
                        });
                    }
                }
            });

            Ok(true)
        }
        HomeClientCommand::ResolvePermission {
            request_id,
            selected_index,
        } => {
            let resolved = runtime_client.resolve_permission(&request_id, selected_index);
            if !resolved {
                transport
                    .publish_event(HomeClientEvent::Error {
                        message: format!("未找到待处理的 permission request: {request_id}"),
                    })
                    .await?;
            }
            Ok(true)
        }
        HomeClientCommand::Exit => Ok(false),
    }
}

async fn handle_internal_event<T: ClientTransport>(
    transport: &mut T,
    prompt_in_flight: &mut bool,
    event: InternalEvent,
) -> Result<()> {
    match event {
        InternalEvent::OutputChunk { session_id, text } => {
            transport
                .publish_event(HomeClientEvent::OutputChunk { session_id, text })
                .await?;
        }
        InternalEvent::PermissionRequested {
            request_id,
            session_id,
            options,
        } => {
            transport
                .publish_event(HomeClientEvent::PermissionRequested {
                    request_id,
                    session_id,
                    options,
                })
                .await?;
        }
        InternalEvent::PromptFinished {
            session_id,
            stop_reason,
        } => {
            *prompt_in_flight = false;
            transport
                .publish_event(HomeClientEvent::PromptFinished {
                    session_id,
                    stop_reason,
                })
                .await?;
        }
        InternalEvent::PromptFailed {
            session_id,
            message,
        } => {
            *prompt_in_flight = false;
            transport
                .publish_event(HomeClientEvent::Error {
                    message: format!("session={session_id} prompt 失败: {message}"),
                })
                .await?;
        }
        InternalEvent::RuntimeError { message } => {
            transport
                .publish_event(HomeClientEvent::Error { message })
                .await?;
        }
    }

    Ok(())
}

async fn resolve_session<T: ClientTransport>(
    transport: &mut T,
    workspaces: &[WorkspaceConfig],
    default_workspace_id: &str,
    agent: &ClientSideConnection,
    sessions: &mut HashMap<String, SessionId>,
    requested_session: Option<String>,
    create_if_missing: bool,
) -> Result<Option<(String, SessionId)>> {
    if let Some(session_ref) = requested_session {
        if let Some(session_id) = sessions.get(&session_ref) {
            return Ok(Some((session_ref, session_id.clone())));
        }

        transport
            .publish_event(HomeClientEvent::Error {
                message: format!("未知 session: {session_ref}"),
            })
            .await?;
        return Ok(None);
    }

    if !create_if_missing {
        transport
            .publish_event(HomeClientEvent::Error {
                message: "当前命令没有指定 session，且不允许自动创建。".to_string(),
            })
            .await?;
        return Ok(None);
    }

    let workspace = resolve_workspace(workspaces, None, Some(default_workspace_id))?;
    let session_id = create_session(agent, workspace).await?;
    let session_ref = session_id.to_string();
    sessions.insert(session_ref.clone(), session_id.clone());
    transport
        .publish_event(HomeClientEvent::SessionCreated {
            session_id: session_ref.clone(),
            workspace_id: workspace.id.clone(),
            workspace_name: workspace.name.clone(),
        })
        .await?;

    Ok(Some((session_ref, session_id)))
}

async fn stream_stderr(stderr: ChildStderr, internal_tx: mpsc::UnboundedSender<InternalEvent>) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let _ = internal_tx.send(InternalEvent::RuntimeError {
            message: format!("codex-acp: {line}"),
        });
    }
}

async fn create_session(
    agent: &ClientSideConnection,
    workspace: &WorkspaceConfig,
) -> Result<SessionId> {
    let response = agent
        .new_session(NewSessionRequest::new(&workspace.path))
        .await
        .context("创建会话失败")?;
    Ok(response.session_id)
}

fn resolve_workspace<'a>(
    workspaces: &'a [WorkspaceConfig],
    requested_workspace_id: Option<&str>,
    default_workspace_id: Option<&str>,
) -> Result<&'a WorkspaceConfig> {
    if let Some(workspace_id) = requested_workspace_id {
        return workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("未知 workspace: {workspace_id}"));
    }

    if let Some(default_workspace_id) = default_workspace_id {
        return workspaces
            .iter()
            .find(|workspace| workspace.id == default_workspace_id)
            .with_context(|| format!("默认 workspace 不存在: {default_workspace_id}"));
    }

    workspaces
        .first()
        .context("当前没有可用工作区，无法创建会话")
}
