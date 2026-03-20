use crate::protocol::{
    BrowserToServerMessage, ClientToServerMessage, DeviceStatusMessage, RelaySessionSummaryMessage,
    ServerToBrowserMessage, ServerToClientMessage, SessionEventMessage,
};
use crate::relay_state::{RelayPersistence, RelayStore, SessionEventExt, unix_timestamp_secs};
use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Html,
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
};

#[derive(Debug, Clone)]
pub struct RelayServerConfig {
    pub bind_addr: SocketAddr,
    pub expected_client_token: Option<String>,
    pub state_file: PathBuf,
    pub compaction_threshold: usize,
    pub segment_event_limit: usize,
}

impl RelayServerConfig {
    pub fn from_env() -> Result<Self> {
        let bind_addr = std::env::var("RELAY_SERVER_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .context("解析 RELAY_SERVER_BIND 失败")?;
        let expected_client_token = std::env::var("RELAY_SERVER_CLIENT_TOKEN").ok();
        let state_file = std::env::var("RELAY_SERVER_STATE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("relay-state.json"));
        let compaction_threshold = std::env::var("RELAY_SERVER_COMPACTION_THRESHOLD")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(200);
        let segment_event_limit = std::env::var("RELAY_SERVER_SEGMENT_EVENT_LIMIT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100);
        Ok(Self {
            bind_addr,
            expected_client_token,
            state_file,
            compaction_threshold,
            segment_event_limit,
        })
    }
}

#[derive(Clone)]
struct AppState {
    inner: Arc<InnerState>,
}

struct InnerState {
    expected_client_token: Option<String>,
    persistence: RelayPersistence,
    next_browser_id: AtomicU64,
    next_session_id: AtomicU64,
    next_event_id: AtomicU64,
    devices: Mutex<HashMap<String, DeviceConnection>>,
    browsers: Mutex<HashMap<String, BrowserConnection>>,
    sessions: Mutex<HashMap<String, RelaySession>>,
    permissions: Mutex<HashMap<String, PendingPermission>>,
    store: Mutex<RelayStore>,
}

#[derive(Clone)]
struct DeviceConnection {
    device_id: String,
    device_name: String,
    sender: mpsc::UnboundedSender<Message>,
}

#[derive(Clone)]
struct BrowserConnection {
    sender: mpsc::UnboundedSender<Message>,
}

#[derive(Clone)]
struct RelaySession {
    session_id: String,
    device_id: String,
    browser_id: String,
}

#[derive(Clone)]
struct PendingPermission {
    session_id: Option<String>,
    device_id: String,
    browser_id: String,
}

impl AppState {
    fn new(config: RelayServerConfig) -> Result<Self> {
        let persistence = RelayPersistence::new(
            config.state_file,
            config.compaction_threshold,
            config.segment_event_limit,
        );
        let persisted = persistence.load()?;
        let next_session_counter = persisted.next_session_counter();
        let next_event_counter = persisted.next_event_seq();
        Ok(Self {
            inner: Arc::new(InnerState {
                expected_client_token: config.expected_client_token,
                persistence,
                next_browser_id: AtomicU64::new(1),
                next_session_id: AtomicU64::new(next_session_counter),
                next_event_id: AtomicU64::new(next_event_counter),
                devices: Mutex::new(HashMap::new()),
                browsers: Mutex::new(HashMap::new()),
                sessions: Mutex::new(
                    persisted
                        .sessions
                        .values()
                        .map(|session| {
                            (
                                session.session_id.clone(),
                                RelaySession {
                                    session_id: session.session_id.clone(),
                                    device_id: session.device_id.clone(),
                                    browser_id: String::new(),
                                },
                            )
                        })
                        .collect(),
                ),
                permissions: Mutex::new(HashMap::new()),
                store: Mutex::new(persisted),
            }),
        })
    }

    fn next_browser_id(&self) -> String {
        format!(
            "browser-{}",
            self.inner.next_browser_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn next_session_id(&self) -> String {
        format!(
            "relay-session-{}",
            self.inner.next_session_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn next_event_seq(&self) -> u64 {
        self.inner.next_event_id.fetch_add(1, Ordering::Relaxed)
    }
}

pub async fn run(config: RelayServerConfig) -> Result<()> {
    let state = AppState::new(config.clone())?;
    let app = Router::new()
        .route("/", get(debug_console))
        .route("/healthz", get(healthz))
        .route("/ws/client", get(client_ws_handler))
        .route("/ws/browser", get(browser_ws_handler))
        .with_state(state.clone());

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("绑定 relay server 地址失败: {}", config.bind_addr))?;

    println!(
        "relay server listening on {} using snapshot {} and event dir {} (compaction threshold {}, segment limit {})",
        config.bind_addr,
        state.inner.persistence.snapshot_path().display(),
        state.inner.persistence.event_log_dir().display(),
        state.inner.persistence.compaction_threshold(),
        state.inner.persistence.segment_event_limit()
    );
    axum::serve(listener, app)
        .await
        .context("relay server 运行失败")?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn debug_console() -> Html<&'static str> {
    Html(include_str!("relay_console.html"))
}

async fn client_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_client_socket(state, socket))
}

async fn browser_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_browser_socket(state, socket))
}

async fn handle_client_socket(state: AppState, socket: WebSocket) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();

    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if ws_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let hello = read_client_hello(&mut ws_receiver).await;
    let (device_id, device_name, auth_token) = match hello {
        Ok(values) => values,
        Err(err) => {
            let _ = tx.send(text_message(&ServerToBrowserMessage::Error {
                session_id: None,
                message: format!("client hello 失败: {err}"),
            }));
            writer.abort();
            return;
        }
    };

    if let Some(expected) = &state.inner.expected_client_token
        && &auth_token != expected
    {
        let _ = tx.send(Message::Close(None));
        writer.abort();
        return;
    }

    {
        let mut devices = state.inner.devices.lock().await;
        devices.insert(
            device_id.clone(),
            DeviceConnection {
                device_id: device_id.clone(),
                device_name: device_name.clone(),
                sender: tx.clone(),
            },
        );
    }
    let device_id_for_store = device_id.clone();
    let device_name_for_store = device_name.clone();
    if let Err(err) = update_store(&state, |store| {
        store.devices.insert(
            device_id_for_store.clone(),
            DeviceStatusMessage {
                device_id: device_id_for_store.clone(),
                device_name: device_name_for_store.clone(),
                connected: true,
            },
        );
    })
    .await
    {
        eprintln!("[relay-persist-error] {err}");
    }
    broadcast_device_status(
        &state,
        DeviceStatusMessage {
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            connected: true,
        },
    )
    .await;

    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(Message::Text(text)) => {
                if let Ok(message) = serde_json::from_str::<ClientToServerMessage>(text.as_ref()) {
                    if let Err(err) = handle_client_message(&state, &device_id, message).await {
                        eprintln!("[relay-client-message-error] {err}");
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(payload)) => {
                let _ = tx.send(Message::Pong(payload));
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Binary(_)) => {}
            Err(err) => {
                eprintln!("[relay-client-ws-error] {err}");
                break;
            }
        }
    }

    {
        let mut devices = state.inner.devices.lock().await;
        devices.remove(&device_id);
    }
    let device_id_for_store = device_id.clone();
    let device_name_for_store = device_name.clone();
    if let Err(err) = update_store(&state, |store| {
        store.devices.insert(
            device_id_for_store.clone(),
            DeviceStatusMessage {
                device_id: device_id_for_store.clone(),
                device_name: device_name_for_store.clone(),
                connected: false,
            },
        );
    })
    .await
    {
        eprintln!("[relay-persist-error] {err}");
    }
    cleanup_device(&state, &device_id).await;
    broadcast_device_status(
        &state,
        DeviceStatusMessage {
            device_id,
            device_name,
            connected: false,
        },
    )
    .await;
    writer.abort();
}

async fn handle_browser_socket(state: AppState, socket: WebSocket) {
    let browser_id = state.next_browser_id();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();

    {
        let mut browsers = state.inner.browsers.lock().await;
        browsers.insert(browser_id.clone(), BrowserConnection { sender: tx.clone() });
    }

    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if ws_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    if let Err(err) = send_device_list(&state, &browser_id).await {
        eprintln!("[relay-browser-init-error] {err}");
    }

    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<BrowserToServerMessage>(text.as_ref()) {
                    Ok(message) => {
                        if let Err(err) = handle_browser_message(&state, &browser_id, message).await
                        {
                            let _ = send_to_browser(
                                &state,
                                &browser_id,
                                ServerToBrowserMessage::Error {
                                    session_id: None,
                                    message: err.to_string(),
                                },
                            )
                            .await;
                        }
                    }
                    Err(err) => {
                        let _ = send_to_browser(
                            &state,
                            &browser_id,
                            ServerToBrowserMessage::Error {
                                session_id: None,
                                message: format!("无法解析 browser 消息: {err}"),
                            },
                        )
                        .await;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(payload)) => {
                let _ = tx.send(Message::Pong(payload));
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Binary(_)) => {}
            Err(err) => {
                eprintln!("[relay-browser-ws-error] {err}");
                break;
            }
        }
    }

    cleanup_browser(&state, &browser_id).await;
    writer.abort();
}

async fn read_client_hello(
    ws_receiver: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Result<(String, String, String)> {
    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(Message::Text(text)) => {
                let message = serde_json::from_str::<ClientToServerMessage>(text.as_ref())
                    .context("解析 client hello 失败")?;
                if let ClientToServerMessage::Hello {
                    device_id,
                    device_name,
                    auth_token,
                    ..
                } = message
                {
                    return Ok((device_id, device_name, auth_token));
                }

                return Err(anyhow!("client 第一个消息必须是 hello"));
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) => return Err(anyhow!("client 在 hello 前断开连接")),
            Ok(Message::Binary(_)) => return Err(anyhow!("client hello 不支持 binary 消息")),
            Err(err) => return Err(anyhow!("读取 client hello 失败: {err}")),
        }
    }

    Err(anyhow!("client 在发送 hello 前断开连接"))
}

async fn handle_client_message(
    state: &AppState,
    device_id: &str,
    message: ClientToServerMessage,
) -> Result<()> {
    match message {
        ClientToServerMessage::Hello { .. } => {}
        ClientToServerMessage::Ready { agent_name } => {
            broadcast_info(
                state,
                format!("device {device_id} ready with agent {agent_name}"),
            )
            .await;
        }
        ClientToServerMessage::Info { message } => {
            broadcast_info(state, format!("device {device_id}: {message}")).await;
        }
        ClientToServerMessage::SessionCreated { session_id, cwd } => {
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let cwd_for_store = cwd.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    summary.status = "ready".to_string();
                    summary.cwd = Some(cwd_for_store.clone());
                    summary.updated_at = now;
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "session_created".to_string(),
                    text: Some(cwd_for_store.clone()),
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;

            let session = {
                let sessions = state.inner.sessions.lock().await;
                sessions.get(&session_id).cloned()
            };

            if let Some(session) = session {
                send_to_browser(
                    state,
                    &session.browser_id,
                    ServerToBrowserMessage::SessionCreated {
                        session_id,
                        device_id: session.device_id,
                        cwd,
                    },
                )
                .await?;
            }
        }
        ClientToServerMessage::OutputChunk { session_id, text } => {
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let text_for_store = text.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    summary.updated_at = now;
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "output_chunk".to_string(),
                    text: Some(text_for_store.clone()),
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;
            route_session_message(
                state,
                &session_id,
                ServerToBrowserMessage::OutputChunk { session_id, text },
            )
            .await?;
        }
        ClientToServerMessage::PromptFinished {
            session_id,
            stop_reason,
        } => {
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let stop_reason_for_store = stop_reason.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    summary.status = "idle".to_string();
                    summary.updated_at = now;
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "prompt_finished".to_string(),
                    text: None,
                    stop_reason: Some(stop_reason_for_store.clone()),
                    request_id: None,
                    options: None,
                });
            })
            .await?;
            route_session_message(
                state,
                &session_id,
                ServerToBrowserMessage::PromptFinished {
                    session_id,
                    stop_reason,
                },
            )
            .await?;
        }
        ClientToServerMessage::PermissionRequested {
            request_id,
            session_id,
            options,
        } => {
            let browser_id = match session_id.as_ref() {
                Some(session_id) => {
                    let session = {
                        let sessions = state.inner.sessions.lock().await;
                        sessions.get(session_id).cloned()
                    };
                    session.map(|session| session.browser_id)
                }
                None => None,
            };

            if let Some(browser_id) = browser_id {
                let now = unix_timestamp_secs();
                let seq = state.next_event_seq();
                let session_id_for_store = session_id.clone();
                let request_id_for_store = request_id.clone();
                let options_for_store = options.clone();
                {
                    let mut permissions = state.inner.permissions.lock().await;
                    permissions.insert(
                        request_id.clone(),
                        PendingPermission {
                            session_id: session_id.clone(),
                            device_id: device_id.to_string(),
                            browser_id: browser_id.clone(),
                        },
                    );
                }
                update_store(state, |store| {
                    if let Some(session_id) = session_id_for_store.as_ref() {
                        if let Some(summary) = store.sessions.get_mut(session_id) {
                            summary.status = "awaiting_permission".to_string();
                            summary.updated_at = now;
                        }
                    }
                    store.events.push(SessionEventMessage {
                        seq,
                        seq_end: None,
                        session_id: session_id_for_store.clone().unwrap_or_default(),
                        timestamp: now,
                        kind: "permission_requested".to_string(),
                        text: None,
                        stop_reason: None,
                        request_id: Some(request_id_for_store.clone()),
                        options: Some(options_for_store.clone()),
                    });
                })
                .await?;

                send_to_browser(
                    state,
                    &browser_id,
                    ServerToBrowserMessage::PermissionRequested {
                        request_id,
                        session_id,
                        options,
                    },
                )
                .await?;
            }
        }
        ClientToServerMessage::Error { message } => {
            broadcast_info(state, format!("device {device_id} error: {message}")).await;
        }
    }

    Ok(())
}

async fn handle_browser_message(
    state: &AppState,
    browser_id: &str,
    message: BrowserToServerMessage,
) -> Result<()> {
    match message {
        BrowserToServerMessage::ListDevices => send_device_list(state, browser_id).await?,
        BrowserToServerMessage::ListSessions => send_session_list(state, browser_id).await?,
        BrowserToServerMessage::CreateSession { device_id, cwd } => {
            ensure_device_connected(state, &device_id).await?;
            let session_id = state.next_session_id();
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let device_id_for_store = device_id.clone();
            let cwd_for_store = cwd.clone();

            {
                let mut sessions = state.inner.sessions.lock().await;
                sessions.insert(
                    session_id.clone(),
                    RelaySession {
                        session_id: session_id.clone(),
                        device_id: device_id.clone(),
                        browser_id: browser_id.to_string(),
                    },
                );
            }
            update_store(state, |store| {
                store.sessions.insert(
                    session_id_for_store.clone(),
                    RelaySessionSummaryMessage {
                        session_id: session_id_for_store.clone(),
                        device_id: device_id_for_store.clone(),
                        status: "requested".to_string(),
                        cwd: cwd_for_store.clone(),
                        created_at: now,
                        updated_at: now,
                    },
                );
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "session_requested".to_string(),
                    text: None,
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;

            send_to_client(
                state,
                &device_id,
                ServerToClientMessage::CreateSession { session_id, cwd },
            )
            .await?;
        }
        BrowserToServerMessage::AdoptSession { session_id } => {
            {
                let mut sessions = state.inner.sessions.lock().await;
                let session = sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
                session.browser_id = browser_id.to_string();
            }
            send_session_list(state, browser_id).await?;
        }
        BrowserToServerMessage::Prompt { session_id, text } => {
            let session = get_browser_session(state, browser_id, &session_id).await?;
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session.session_id.clone();
            let text_for_store = text.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    summary.status = "running".to_string();
                    summary.updated_at = now;
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "prompt_submitted".to_string(),
                    text: Some(text_for_store.clone()),
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;
            send_to_client(
                state,
                &session.device_id,
                ServerToClientMessage::Prompt {
                    session_id: Some(session.session_id),
                    text,
                    create_session_if_missing: false,
                },
            )
            .await?;
        }
        BrowserToServerMessage::GetSessionHistory {
            session_id,
            before_seq,
            limit,
        } => {
            let (events, next_before_seq, has_more) = {
                let store = state.inner.store.lock().await;
                if !store.sessions.contains_key(&session_id) {
                    return Err(anyhow!("未知 relay session: {session_id}"));
                }
                paginate_session_events(&store, &session_id, before_seq, limit)
            };
            send_to_browser(
                state,
                browser_id,
                ServerToBrowserMessage::SessionHistory {
                    session_id,
                    events,
                    next_before_seq,
                    has_more,
                },
            )
            .await?;
        }
        BrowserToServerMessage::ResolvePermission {
            request_id,
            selected_index,
        } => {
            let pending = {
                let mut permissions = state.inner.permissions.lock().await;
                permissions.remove(&request_id)
            }
            .ok_or_else(|| anyhow!("未知 permission request: {request_id}"))?;

            if pending.browser_id != browser_id {
                return Err(anyhow!("当前 browser 无权响应该 permission request"));
            }

            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let pending_session_id = pending.session_id.clone();
            let request_id_for_store = request_id.clone();
            update_store(state, |store| {
                if let Some(session_id) = pending_session_id.as_ref() {
                    if let Some(summary) = store.sessions.get_mut(session_id) {
                        summary.status = "running".to_string();
                        summary.updated_at = now;
                    }
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: pending_session_id.clone().unwrap_or_default(),
                    timestamp: now,
                    kind: "permission_resolved".to_string(),
                    text: Some(format!("selected_index={selected_index}")),
                    stop_reason: None,
                    request_id: Some(request_id_for_store.clone()),
                    options: None,
                });
            })
            .await?;

            send_to_client(
                state,
                &pending.device_id,
                ServerToClientMessage::ResolvePermission {
                    request_id,
                    selected_index,
                },
            )
            .await?;
        }
        BrowserToServerMessage::Ping => {
            send_to_browser(
                state,
                browser_id,
                ServerToBrowserMessage::Info {
                    message: "pong".to_string(),
                },
            )
            .await?;
        }
    }

    Ok(())
}

async fn send_device_list(state: &AppState, browser_id: &str) -> Result<()> {
    let devices = {
        let store = state.inner.store.lock().await;
        store.devices.values().cloned().collect::<Vec<_>>()
    };

    send_to_browser(
        state,
        browser_id,
        ServerToBrowserMessage::DeviceList { devices },
    )
    .await
}

async fn send_session_list(state: &AppState, browser_id: &str) -> Result<()> {
    let sessions = {
        let store = state.inner.store.lock().await;
        let mut sessions = store.sessions.values().cloned().collect::<Vec<_>>();
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        sessions
    };

    send_to_browser(
        state,
        browser_id,
        ServerToBrowserMessage::SessionList { sessions },
    )
    .await
}

fn paginate_session_events(
    store: &RelayStore,
    session_id: &str,
    before_seq: Option<u64>,
    limit: Option<usize>,
) -> (Vec<SessionEventMessage>, Option<u64>, bool) {
    let page_size = limit.unwrap_or(100).clamp(1, 500);
    let mut page = Vec::new();

    for event in store.events.iter().rev() {
        if event.session_id != session_id {
            continue;
        }
        if let Some(before_seq) = before_seq
            && event.max_seq() >= before_seq
        {
            continue;
        }
        page.push(event.clone());
        if page.len() == page_size {
            break;
        }
    }

    page.reverse();
    let first_seq = page.first().map(|event| event.seq);
    let has_more = match first_seq {
        Some(first_seq) => store
            .events
            .iter()
            .any(|event| event.session_id == session_id && event.max_seq() < first_seq),
        None => false,
    };
    let next_before_seq = if has_more { first_seq } else { None };

    (page, next_before_seq, has_more)
}

async fn broadcast_device_status(state: &AppState, device: DeviceStatusMessage) {
    let browser_senders = {
        let browsers = state.inner.browsers.lock().await;
        browsers
            .values()
            .map(|browser| browser.sender.clone())
            .collect::<Vec<_>>()
    };

    if let Ok(text) = serde_json::to_string(&ServerToBrowserMessage::DeviceStatus { device }) {
        let message = Message::Text(text.into());
        for sender in browser_senders {
            let _ = sender.send(message.clone());
        }
    }
}

async fn broadcast_info(state: &AppState, message: String) {
    let browser_ids = {
        let browsers = state.inner.browsers.lock().await;
        browsers.keys().cloned().collect::<Vec<_>>()
    };

    for browser_id in browser_ids {
        let _ = send_to_browser(
            state,
            &browser_id,
            ServerToBrowserMessage::Info {
                message: message.clone(),
            },
        )
        .await;
    }
}

async fn route_session_message(
    state: &AppState,
    session_id: &str,
    message: ServerToBrowserMessage,
) -> Result<()> {
    let browser_id = {
        let sessions = state.inner.sessions.lock().await;
        sessions
            .get(session_id)
            .map(|session| session.browser_id.clone())
            .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?
    };

    send_to_browser(state, &browser_id, message).await
}

async fn send_to_browser(
    state: &AppState,
    browser_id: &str,
    message: ServerToBrowserMessage,
) -> Result<()> {
    let sender = {
        let browsers = state.inner.browsers.lock().await;
        browsers
            .get(browser_id)
            .map(|browser| browser.sender.clone())
            .ok_or_else(|| anyhow!("browser 已离线: {browser_id}"))?
    };
    sender
        .send(text_message(&message))
        .map_err(|_| anyhow!("browser channel 已关闭"))?;
    Ok(())
}

async fn send_to_client(
    state: &AppState,
    device_id: &str,
    message: ServerToClientMessage,
) -> Result<()> {
    let sender = {
        let devices = state.inner.devices.lock().await;
        devices
            .get(device_id)
            .map(|device| device.sender.clone())
            .ok_or_else(|| anyhow!("device 未连接: {device_id}"))?
    };
    sender
        .send(text_message(&message))
        .map_err(|_| anyhow!("client channel 已关闭"))?;
    Ok(())
}

async fn ensure_device_connected(state: &AppState, device_id: &str) -> Result<()> {
    let devices = state.inner.devices.lock().await;
    if devices.contains_key(device_id) {
        Ok(())
    } else {
        Err(anyhow!("device 未连接: {device_id}"))
    }
}

async fn get_browser_session(
    state: &AppState,
    browser_id: &str,
    session_id: &str,
) -> Result<RelaySession> {
    let sessions = state.inner.sessions.lock().await;
    let session = sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
    if session.browser_id != browser_id {
        return Err(anyhow!("当前 browser 无权访问该 session"));
    }
    Ok(session)
}

async fn cleanup_browser(state: &AppState, browser_id: &str) {
    {
        let mut browsers = state.inner.browsers.lock().await;
        browsers.remove(browser_id);
    }

    {
        let mut permissions = state.inner.permissions.lock().await;
        permissions.retain(|_, pending| pending.browser_id != browser_id);
    }
}

async fn cleanup_device(state: &AppState, device_id: &str) {
    let affected_browser_ids = {
        let mut sessions = state.inner.sessions.lock().await;
        let affected_browser_ids = sessions
            .values()
            .filter(|session| session.device_id == device_id)
            .filter(|session| !session.browser_id.is_empty())
            .map(|session| session.browser_id.clone())
            .collect::<Vec<_>>();
        affected_browser_ids
    };

    {
        let mut permissions = state.inner.permissions.lock().await;
        permissions.retain(|_, pending| pending.device_id != device_id);
    }

    let now = unix_timestamp_secs();
    let device_id_for_store = device_id.to_string();
    let seq = state.next_event_seq();
    if let Err(err) = update_store(state, |store| {
        for session in store.sessions.values_mut() {
            if session.device_id == device_id_for_store {
                session.status = "device_disconnected".to_string();
                session.updated_at = now;
            }
        }
        store.events.push(SessionEventMessage {
            seq,
            seq_end: None,
            session_id: String::new(),
            timestamp: now,
            kind: "device_disconnected".to_string(),
            text: Some(device_id_for_store.clone()),
            stop_reason: None,
            request_id: None,
            options: None,
        });
    })
    .await
    {
        eprintln!("[relay-persist-error] {err}");
    }

    for browser_id in affected_browser_ids {
        let _ = send_to_browser(
            state,
            &browser_id,
            ServerToBrowserMessage::Error {
                session_id: None,
                message: format!("device 已断开连接: {device_id}"),
            },
        )
        .await;
    }
}

fn text_message<T: serde::Serialize>(message: &T) -> Message {
    match serde_json::to_string(message) {
        Ok(text) => Message::Text(text.into()),
        Err(err) => Message::Text(
            serde_json::json!({
                "type": "error",
                "message": format!("server 序列化失败: {err}")
            })
            .to_string()
            .into(),
        ),
    }
}

async fn update_store<F>(state: &AppState, mutate: F) -> Result<()>
where
    F: FnOnce(&mut RelayStore),
{
    let (snapshot, appended_events, rewritten_log_events, persistence) = {
        let mut store = state.inner.store.lock().await;
        let prior_event_count = store.events.len();
        mutate(&mut store);
        let persistence = state.inner.persistence.clone();

        if store.live_event_count() >= persistence.compaction_threshold() {
            store.compact_into_checkpoint();
            let live_tail = store.events[store.checkpointed_event_count..].to_vec();
            (store.clone(), Vec::new(), Some(live_tail), persistence)
        } else {
            let appended_events = store.events[prior_event_count..].to_vec();
            (store.clone(), appended_events, None, persistence)
        }
    };

    if let Some(live_tail) = rewritten_log_events {
        persistence.save_snapshot(&snapshot).await?;
        persistence.rewrite_event_log(live_tail).await?;
    } else {
        for event in appended_events {
            persistence.append_event(event).await?;
        }
        persistence.save_snapshot(&snapshot).await?;
    }

    Ok(())
}
