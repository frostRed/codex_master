use crate::config::{RelayTransportConfig, WorkspaceConfig};
use crate::protocol::{
    ClientToServerMessage, PermissionOptionMessage, ServerToClientMessage, WorkspaceSummaryMessage,
};
use crate::types::{HomeClientCommand, HomeClientEvent, WorkspaceView};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write as _;
use std::rc::Rc;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[async_trait(?Send)]
pub trait ClientTransport {
    async fn next_command(&mut self) -> Option<HomeClientCommand>;
    async fn publish_event(&mut self, event: HomeClientEvent) -> Result<()>;
}

#[derive(Default)]
struct LocalDebugState {
    active_session: Option<String>,
    pending_permission_requests: HashMap<String, usize>,
    active_stream_session: Option<String>,
    available_workspaces: Vec<WorkspaceView>,
}

pub struct LocalDebugTransport {
    state: Rc<RefCell<LocalDebugState>>,
    command_rx: mpsc::UnboundedReceiver<HomeClientCommand>,
}

impl LocalDebugTransport {
    pub fn new() -> Self {
        let state = Rc::new(RefCell::new(LocalDebugState::default()));
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let state_for_input = state.clone();

        tokio::task::spawn_local(async move {
            print_help();

            let mut lines = BufReader::new(io::stdin()).lines();
            loop {
                print!("you> ");
                let _ = std::io::stdout().flush();

                let next_line = lines.next_line().await;
                let line = match next_line {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        let _ = command_tx.send(HomeClientCommand::Exit);
                        break;
                    }
                    Err(err) => {
                        eprintln!("[input-error] {err}");
                        let _ = command_tx.send(HomeClientCommand::Exit);
                        break;
                    }
                };

                match parse_command(&line, &state_for_input) {
                    Ok(Some(command)) => {
                        if command_tx.send(command).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(err) => eprintln!("[command-error] {err}"),
                }
            }
        });

        Self { state, command_rx }
    }
}

#[async_trait(?Send)]
impl ClientTransport for LocalDebugTransport {
    async fn next_command(&mut self) -> Option<HomeClientCommand> {
        self.command_rx.recv().await
    }

    async fn publish_event(&mut self, event: HomeClientEvent) -> Result<()> {
        match event {
            HomeClientEvent::Ready {
                agent_name,
                workspaces,
            } => {
                self.state.borrow_mut().available_workspaces = workspaces.clone();
                println!("已连接 {agent_name}。输入 /help 查看命令。");
                print_workspaces(&workspaces);
            }
            HomeClientEvent::Info { message } => {
                println!("[info] {message}");
            }
            HomeClientEvent::SessionCreated {
                session_id,
                workspace_id,
                workspace_name,
            } => {
                let mut state = self.state.borrow_mut();
                state.active_session = Some(session_id.clone());
                println!("[session] {session_id} workspace={workspace_name} ({workspace_id})");
            }
            HomeClientEvent::OutputChunk { session_id, text } => {
                let mut state = self.state.borrow_mut();
                if state.active_stream_session.as_ref() != Some(&session_id) {
                    if state.active_stream_session.is_some() {
                        println!();
                    }
                    state.active_stream_session = Some(session_id);
                }

                print!("{text}");
                std::io::stdout().flush()?;
            }
            HomeClientEvent::PromptFinished {
                session_id,
                stop_reason,
            } => {
                let mut state = self.state.borrow_mut();
                if state.active_stream_session.is_some() {
                    println!();
                }
                state.active_stream_session = None;

                if stop_reason != "EndTurn" {
                    println!("[done] session={session_id} stop_reason={stop_reason}");
                }
            }
            HomeClientEvent::PermissionRequested {
                request_id,
                session_id,
                options,
            } => {
                let mut state = self.state.borrow_mut();
                state
                    .pending_permission_requests
                    .insert(request_id.clone(), options.len());

                println!(
                    "\n[permission] request_id={request_id} session={}",
                    session_id.as_deref().unwrap_or("unknown")
                );
                for option in &options {
                    println!("  [{}] {} ({})", option.index, option.name, option.kind);
                }
                println!("  使用 /perm {request_id} <index> 来选择。");
            }
            HomeClientEvent::Error { message } => {
                eprintln!("[error] {message}");
            }
        }

        Ok(())
    }
}

fn parse_command(
    line: &str,
    state: &Rc<RefCell<LocalDebugState>>,
) -> Result<Option<HomeClientCommand>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if matches!(trimmed, "/exit" | "/quit") {
        return Ok(Some(HomeClientCommand::Exit));
    }

    if trimmed == "/help" {
        print_help();
        return Ok(None);
    }

    if trimmed == "/workspaces" {
        let workspaces = state.borrow().available_workspaces.clone();
        print_workspaces(&workspaces);
        return Ok(None);
    }

    if let Some(rest) = trimmed.strip_prefix("/new") {
        let workspace_id = parse_optional_arg(rest).map(str::to_string);
        return Ok(Some(HomeClientCommand::CreateSession {
            session_id: None,
            workspace_id,
        }));
    }

    if let Some(rest) = trimmed.strip_prefix("/use") {
        let Some(session_id) = parse_optional_arg(rest) else {
            bail!("用法: /use <session_id>");
        };
        state.borrow_mut().active_session = Some(session_id.to_string());
        println!("[info] active session 切换为 {session_id}");
        return Ok(None);
    }

    if let Some(rest) = trimmed.strip_prefix("/perm") {
        let mut parts = rest.split_whitespace();
        let Some(request_id) = parts.next() else {
            bail!("用法: /perm <request_id> <index>");
        };
        let Some(raw_index) = parts.next() else {
            bail!("用法: /perm <request_id> <index>");
        };
        let selected_index = raw_index
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("permission index 必须是数字"))?;

        let mut state = state.borrow_mut();
        if let Some(option_count) = state.pending_permission_requests.get(request_id).copied() {
            if selected_index >= option_count {
                bail!("index 超出范围，当前可选上限为 {}", option_count - 1);
            }
            state.pending_permission_requests.remove(request_id);
        } else {
            bail!("未找到待处理的 permission request: {request_id}");
        }

        return Ok(Some(HomeClientCommand::ResolvePermission {
            request_id: request_id.to_string(),
            selected_index,
        }));
    }

    if trimmed.starts_with('/') {
        bail!("未知命令: {trimmed}");
    }

    let active_session = state.borrow().active_session.clone();
    Ok(Some(HomeClientCommand::Prompt {
        session_id: active_session,
        text: line.to_string(),
        create_session_if_missing: true,
    }))
}

fn parse_optional_arg(rest: &str) -> Option<&str> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn print_help() {
    println!("命令:");
    println!("  /new [workspace_id]     创建新会话，可选指定工作区");
    println!("  /workspaces             查看当前允许的工作区");
    println!("  /use <session_id>       切换当前活跃会话");
    println!("  /perm <id> <index>      响应权限请求");
    println!("  /exit                   退出");
    println!("  其他任意文本会发送到当前会话；若没有会话则自动创建。");
}

fn print_workspaces(workspaces: &[WorkspaceView]) {
    if workspaces.is_empty() {
        println!("[info] 当前没有可用工作区。");
        return;
    }

    println!("工作区:");
    for workspace in workspaces {
        println!(
            "  - {} ({})",
            workspace.workspace_name, workspace.workspace_id
        );
    }
}

enum RelayOutbound {
    Protocol(ClientToServerMessage),
    Raw(Message),
}

pub struct RelayTransport {
    command_rx: mpsc::UnboundedReceiver<HomeClientCommand>,
    outbound_tx: mpsc::UnboundedSender<RelayOutbound>,
}

impl RelayTransport {
    pub async fn connect(
        config: RelayTransportConfig,
        workspaces: Vec<WorkspaceConfig>,
    ) -> Result<Self> {
        let (ws_stream, _) = connect_async(&config.url)
            .await
            .with_context(|| format!("连接 relay 失败: {}", config.url))?;
        let (mut ws_write, mut ws_read) = ws_stream.split();

        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel();

        let outbound_for_reader = outbound_tx.clone();
        let command_tx_for_reader = command_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(message_result) = ws_read.next().await {
                match message_result {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<ServerToClientMessage>(text.as_ref()) {
                            Ok(message) => {
                                if let Some(command) = map_server_message(message) {
                                    if command_tx_for_reader.send(command).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(err) => {
                                let _ = outbound_for_reader.send(RelayOutbound::Protocol(
                                    ClientToServerMessage::Error {
                                        message: format!("无法解析 server 消息: {err}"),
                                    },
                                ));
                            }
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        let _ =
                            outbound_for_reader.send(RelayOutbound::Raw(Message::Pong(payload)));
                    }
                    Ok(Message::Close(_)) => {
                        let _ = command_tx_for_reader.send(HomeClientCommand::Exit);
                        break;
                    }
                    Ok(Message::Binary(_)) => {
                        let _ = outbound_for_reader.send(RelayOutbound::Protocol(
                            ClientToServerMessage::Error {
                                message: "当前 relay transport 不支持 binary websocket 消息"
                                    .to_string(),
                            },
                        ));
                    }
                    Ok(Message::Pong(_)) => {}
                    Ok(Message::Frame(_)) => {}
                    Err(err) => {
                        let _ = outbound_for_reader.send(RelayOutbound::Protocol(
                            ClientToServerMessage::Error {
                                message: format!("relay websocket 读取失败: {err}"),
                            },
                        ));
                        let _ = command_tx_for_reader.send(HomeClientCommand::Exit);
                        break;
                    }
                }
            }
        });

        tokio::task::spawn_local(async move {
            while let Some(outbound) = outbound_rx.recv().await {
                let message = match outbound {
                    RelayOutbound::Protocol(payload) => match serde_json::to_string(&payload) {
                        Ok(text) => Message::Text(text.into()),
                        Err(err) => {
                            eprintln!("[relay-serialize-error] {err}");
                            continue;
                        }
                    },
                    RelayOutbound::Raw(message) => message,
                };

                if let Err(err) = ws_write.send(message).await {
                    eprintln!("[relay-write-error] {err}");
                    break;
                }
            }
        });

        outbound_tx
            .send(RelayOutbound::Protocol(ClientToServerMessage::Hello {
                device_id: config.device_id,
                device_name: config.device_name,
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                capabilities: vec![
                    "acp".to_string(),
                    "session-create".to_string(),
                    "prompt-stream".to_string(),
                    "permission-roundtrip".to_string(),
                ],
                workspaces: workspaces
                    .into_iter()
                    .map(|workspace| WorkspaceSummaryMessage {
                        workspace_id: workspace.id,
                        workspace_name: workspace.name,
                    })
                    .collect(),
                auth_token: config.auth_token,
            }))
            .map_err(|_| anyhow!("relay outbound channel 已关闭"))?;

        Ok(Self {
            command_rx,
            outbound_tx,
        })
    }
}

#[async_trait(?Send)]
impl ClientTransport for RelayTransport {
    async fn next_command(&mut self) -> Option<HomeClientCommand> {
        self.command_rx.recv().await
    }

    async fn publish_event(&mut self, event: HomeClientEvent) -> Result<()> {
        let message = map_home_event(event);
        self.outbound_tx
            .send(RelayOutbound::Protocol(message))
            .map_err(|_| anyhow!("relay outbound channel 已关闭"))?;
        Ok(())
    }
}

fn map_server_message(message: ServerToClientMessage) -> Option<HomeClientCommand> {
    match message {
        ServerToClientMessage::CreateSession {
            session_id,
            workspace_id,
        } => Some(HomeClientCommand::CreateSession {
            session_id: Some(session_id),
            workspace_id: Some(workspace_id),
        }),
        ServerToClientMessage::Prompt {
            session_id,
            text,
            create_session_if_missing,
        } => Some(HomeClientCommand::Prompt {
            session_id,
            text,
            create_session_if_missing,
        }),
        ServerToClientMessage::ResolvePermission {
            request_id,
            selected_index,
        } => Some(HomeClientCommand::ResolvePermission {
            request_id,
            selected_index,
        }),
        ServerToClientMessage::Shutdown => Some(HomeClientCommand::Exit),
        ServerToClientMessage::Ping => None,
    }
}

fn map_home_event(event: HomeClientEvent) -> ClientToServerMessage {
    match event {
        HomeClientEvent::Ready { agent_name, .. } => ClientToServerMessage::Ready { agent_name },
        HomeClientEvent::Info { message } => ClientToServerMessage::Info { message },
        HomeClientEvent::SessionCreated {
            session_id,
            workspace_id,
            workspace_name,
        } => ClientToServerMessage::SessionCreated {
            session_id,
            workspace_id,
            workspace_name,
        },
        HomeClientEvent::OutputChunk { session_id, text } => {
            ClientToServerMessage::OutputChunk { session_id, text }
        }
        HomeClientEvent::PromptFinished {
            session_id,
            stop_reason,
        } => ClientToServerMessage::PromptFinished {
            session_id,
            stop_reason,
        },
        HomeClientEvent::PermissionRequested {
            request_id,
            session_id,
            options,
        } => ClientToServerMessage::PermissionRequested {
            request_id,
            session_id,
            options: options
                .into_iter()
                .map(|option| PermissionOptionMessage {
                    index: option.index,
                    name: option.name,
                    kind: option.kind,
                })
                .collect(),
        },
        HomeClientEvent::Error { message } => ClientToServerMessage::Error { message },
    }
}
