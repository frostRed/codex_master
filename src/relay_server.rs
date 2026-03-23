use crate::protocol::{
    BrowserToServerMessage, ClientToServerMessage, DeviceStatusMessage, RelaySessionSummaryMessage,
    ServerToBrowserMessage, ServerToClientMessage, SessionEventMessage, WorkspaceSummaryMessage,
};
use crate::relay_state::{
    PersistedRelaySession, RelayPersistence, RelayStore, SessionEventExt, unix_timestamp_secs,
};
use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    extract::{
        Json, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Html,
    response::Redirect,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
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
struct LocalLoginAuthConfig {
    username: String,
    password: String,
    session_ttl_secs: u64,
    cookie_name: String,
    cookie_secure: bool,
}

#[derive(Debug, Clone)]
struct BrowserAuthConfig {
    login: LocalLoginAuthConfig,
    allowed_origins: HashSet<String>,
}

impl BrowserAuthConfig {
    fn mode_name(&self) -> &'static str {
        "local_login"
    }
}

#[derive(Debug, Clone)]
pub struct RelayServerConfig {
    bind_addr: SocketAddr,
    accepted_client_tokens: HashSet<String>,
    revoked_client_tokens: HashSet<String>,
    browser_auth: BrowserAuthConfig,
    allow_insecure_dev: bool,
    state_file: PathBuf,
    compaction_threshold: usize,
    segment_event_limit: usize,
}

impl RelayServerConfig {
    pub fn from_env() -> Result<Self> {
        let bind_addr = std::env::var("RELAY_SERVER_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .context("解析 RELAY_SERVER_BIND 失败")?;
        let allow_insecure_dev = std::env::var("RELAY_SERVER_ALLOW_INSECURE_DEV")
            .ok()
            .map(|value| parse_bool_env("RELAY_SERVER_ALLOW_INSECURE_DEV", &value))
            .transpose()?
            .unwrap_or(false);
        let accepted_client_tokens = parse_client_token_set()?;
        let revoked_client_tokens = parse_string_set_env("RELAY_SERVER_CLIENT_TOKENS_REVOKED")?;
        let browser_allowed_origins = parse_origin_set_env("RELAY_SERVER_BROWSER_ALLOWED_ORIGINS")?;
        let session_ttl_secs = std::env::var("RELAY_SERVER_LOGIN_SESSION_TTL_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(60 * 60 * 24)
            .max(60);
        let cookie_name = std::env::var("RELAY_SERVER_LOGIN_COOKIE_NAME")
            .unwrap_or_else(|_| "relay_session".to_string());
        let cookie_secure = std::env::var("RELAY_SERVER_LOGIN_COOKIE_SECURE")
            .ok()
            .map(|value| parse_bool_env("RELAY_SERVER_LOGIN_COOKIE_SECURE", &value))
            .transpose()?
            .unwrap_or(!allow_insecure_dev);
        let browser_auth = BrowserAuthConfig {
            login: LocalLoginAuthConfig {
                username: required_nonempty_env("RELAY_SERVER_LOGIN_USERNAME")?,
                password: required_nonempty_env("RELAY_SERVER_LOGIN_PASSWORD")?,
                session_ttl_secs,
                cookie_name,
                cookie_secure,
            },
            allowed_origins: browser_allowed_origins.clone(),
        };

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

        validate_auth_configuration(
            bind_addr,
            allow_insecure_dev,
            &accepted_client_tokens,
            &browser_auth,
        )?;

        Ok(Self {
            bind_addr,
            accepted_client_tokens,
            revoked_client_tokens,
            browser_auth,
            allow_insecure_dev,
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

#[derive(Debug, Clone)]
struct ClientAuthConfig {
    accepted_tokens: HashSet<String>,
    revoked_tokens: HashSet<String>,
}

struct InnerState {
    client_auth: ClientAuthConfig,
    browser_auth: BrowserAuthConfig,
    login_sessions: Mutex<HashMap<String, BrowserLoginSession>>,
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
    sender: mpsc::UnboundedSender<Message>,
}

#[derive(Clone)]
struct BrowserConnection {
    sender: mpsc::UnboundedSender<Message>,
    user_id: String,
}

#[derive(Clone)]
struct RelaySession {
    session_id: String,
    device_id: String,
    browser_id: String,
    owner_user_id: String,
    client_session_id: Option<String>,
}

#[derive(Clone)]
struct PendingPermission {
    session_id: Option<String>,
    device_id: String,
    browser_id: String,
}

#[derive(Debug, Clone)]
struct BrowserLoginSession {
    user_id: String,
    expires_at: u64,
}

#[derive(Debug, Clone)]
struct AuthenticatedBrowser {
    user_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionVisibilityChange {
    None,
    Updated,
    Removed,
}

fn session_is_closed(status: &str) -> bool {
    status == "closed"
}

fn delete_session_from_store(
    store: &mut RelayStore,
    session_id: &str,
    deleted_by_user_id: &str,
    timestamp: u64,
    seq: u64,
) {
    store.sessions.remove(session_id);
    store.events.retain(|event| event.session_id != session_id);
    store.events.push(SessionEventMessage {
        seq,
        seq_end: None,
        session_id: String::new(),
        timestamp,
        kind: "session_deleted".to_string(),
        text: Some(format!(
            "session_id={session_id} deleted_by={deleted_by_user_id}"
        )),
        stop_reason: None,
        request_id: None,
        options: None,
    });
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
                client_auth: ClientAuthConfig {
                    accepted_tokens: config.accepted_client_tokens,
                    revoked_tokens: config.revoked_client_tokens,
                },
                browser_auth: config.browser_auth,
                login_sessions: Mutex::new(HashMap::new()),
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
                                session.summary.session_id.clone(),
                                RelaySession {
                                    session_id: session.summary.session_id.clone(),
                                    device_id: session.summary.device_id.clone(),
                                    browser_id: String::new(),
                                    owner_user_id: session
                                        .summary
                                        .owner_user_id
                                        .clone()
                                        .unwrap_or_default(),
                                    client_session_id: session.client_session_id.clone(),
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
        .route("/login", get(login_page))
        .route("/auth/login", post(login_submit))
        .route("/auth/logout", post(logout_submit))
        .route("/", get(debug_console))
        .route("/manifest.webmanifest", get(web_manifest))
        .route("/sw.js", get(service_worker))
        .route("/favicon.svg", get(app_icon))
        .route("/icons/app.svg", get(app_icon))
        .route("/icons/app-maskable.svg", get(app_maskable_icon))
        .route("/healthz", get(healthz))
        .route("/ws/client", get(client_ws_handler))
        .route("/ws/browser", get(browser_ws_handler))
        .with_state(state.clone());

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("绑定 relay server 地址失败: {}", config.bind_addr))?;

    println!(
        "relay server listening on {} using snapshot {} and event dir {} (compaction threshold {}, segment limit {}, security mode {}, browser auth {})",
        config.bind_addr,
        state.inner.persistence.snapshot_path().display(),
        state.inner.persistence.event_log_dir().display(),
        state.inner.persistence.compaction_threshold(),
        state.inner.persistence.segment_event_limit(),
        if config.allow_insecure_dev {
            "insecure-dev"
        } else {
            "strict"
        },
        config.browser_auth.mode_name()
    );
    axum::serve(listener, app)
        .await
        .context("relay server 运行失败")?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login_page(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if authenticate_browser_http_request(&state, &headers)
        .await
        .is_ok()
    {
        return Redirect::to("/").into_response();
    }

    (
        [("cache-control", "no-cache")],
        Html(include_str!("relay_login.html")),
    )
        .into_response()
}

async fn login_submit(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> Response {
    let config = &state.inner.browser_auth.login;

    if let Err(response) = validate_browser_origin(&state.inner.browser_auth, &headers) {
        return *response;
    }

    if payload.username != config.username || payload.password != config.password {
        return (StatusCode::UNAUTHORIZED, "invalid credentials").into_response();
    }

    let token = uuid::Uuid::new_v4().to_string();
    let expires_at = unix_timestamp_secs().saturating_add(config.session_ttl_secs);
    {
        let mut sessions = state.inner.login_sessions.lock().await;
        sessions.insert(
            token.clone(),
            BrowserLoginSession {
                user_id: config.username.clone(),
                expires_at,
            },
        );
    }

    let cookie = match build_auth_cookie(
        &config.cookie_name,
        &token,
        config.session_ttl_secs,
        config.cookie_secure,
    ) {
        Ok(cookie) => cookie,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build auth cookie: {err}"),
            )
                .into_response();
        }
    };

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(axum::http::header::SET_COOKIE, cookie);
    response
}

async fn logout_submit(headers: HeaderMap, State(state): State<AppState>) -> Response {
    let config = &state.inner.browser_auth.login;
    if let Err(response) = validate_browser_origin(&state.inner.browser_auth, &headers) {
        return *response;
    }

    if let Some(session_token) = cookie_value(&headers, &config.cookie_name) {
        let mut sessions = state.inner.login_sessions.lock().await;
        sessions.remove(&session_token);
    }

    let clear_cookie = match build_expired_auth_cookie(&config.cookie_name, config.cookie_secure) {
        Ok(cookie) => cookie,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to clear auth cookie: {err}"),
            )
                .into_response();
        }
    };

    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(axum::http::header::SET_COOKIE, clear_cookie);
    response
}

async fn debug_console(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [("cache-control", "no-cache")],
        Html(include_str!("relay_console.html")),
    )
        .into_response()
}

async fn web_manifest(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "application/manifest+json; charset=utf-8"),
            ("cache-control", "no-cache"),
        ],
        include_str!("pwa_manifest.webmanifest"),
    )
        .into_response()
}

async fn service_worker(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "application/javascript; charset=utf-8"),
            ("cache-control", "no-cache"),
            ("service-worker-allowed", "/"),
        ],
        include_str!("pwa_service_worker.js"),
    )
        .into_response()
}

async fn app_icon(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "image/svg+xml"),
            ("cache-control", "no-cache"),
        ],
        include_str!("pwa_icon.svg"),
    )
        .into_response()
}

async fn app_maskable_icon(headers: HeaderMap, State(state): State<AppState>) -> Response {
    if let Err(response) = ensure_browser_http_auth(&state, &headers).await {
        return *response;
    }

    (
        [
            ("content-type", "image/svg+xml"),
            ("cache-control", "no-cache"),
        ],
        include_str!("pwa_icon_maskable.svg"),
    )
        .into_response()
}

async fn client_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_client_socket(state, socket))
}

async fn browser_ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let browser = match authenticate_browser_ws_request(&state, &headers).await {
        Ok(browser) => browser,
        Err(response) => return *response,
    };

    ws.on_upgrade(move |socket| handle_browser_socket(state, socket, browser))
}

async fn handle_client_socket(state: AppState, socket: WebSocket) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();

    eprintln!("[relay-client] websocket upgraded; awaiting hello");
    let writer = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if ws_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let hello = read_client_hello(&mut ws_receiver).await;
    let (device_id, device_name, workspaces, auth_token) = match hello {
        Ok(values) => values,
        Err(err) => {
            eprintln!("[relay-client-hello-error] {err}");
            let _ = tx.send(text_message(&ServerToBrowserMessage::Error {
                session_id: None,
                message: format!("client hello 失败: {err}"),
            }));
            writer.abort();
            return;
        }
    };
    let auth_token_state = if auth_token.trim().is_empty() {
        "empty"
    } else {
        "set"
    };
    let auth_token_len = auth_token.chars().count();

    if !is_client_token_allowed(&state.inner.client_auth, &auth_token) {
        eprintln!(
            "[relay-auth] reject client `{}` token_state={} token_len={} accepted_token_count={} revoked_token_count={}",
            device_id,
            auth_token_state,
            auth_token_len,
            state.inner.client_auth.accepted_tokens.len(),
            state.inner.client_auth.revoked_tokens.len()
        );
        let _ = tx.send(Message::Close(None));
        writer.abort();
        return;
    }
    eprintln!(
        "[relay-client] authenticated device_id={} device_name={} workspace_count={} auth_token={} token_len={}",
        device_id,
        device_name,
        workspaces.len(),
        auth_token_state,
        auth_token_len
    );

    {
        let mut devices = state.inner.devices.lock().await;
        devices.insert(device_id.clone(), DeviceConnection { sender: tx.clone() });
    }
    let device_id_for_store = device_id.clone();
    let device_name_for_store = device_name.clone();
    let workspaces_for_store = workspaces.clone();
    if let Err(err) = update_store(&state, |store| {
        store.devices.insert(
            device_id_for_store.clone(),
            DeviceStatusMessage {
                device_id: device_id_for_store.clone(),
                device_name: device_name_for_store.clone(),
                connected: true,
                workspaces: workspaces_for_store.clone(),
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
            workspaces: workspaces.clone(),
        },
    )
    .await;

    let mut disconnect_reason = "stream_ended".to_string();
    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<ClientToServerMessage>(text.as_ref()) {
                    Ok(message) => {
                        if let Err(err) = handle_client_message(&state, &device_id, message).await {
                            eprintln!("[relay-client-message-error] device_id={} err={}", device_id, err);
                        }
                    }
                    Err(err) => {
                        eprintln!(
                            "[relay-client-parse-error] device_id={} err={} payload_len={} payload_preview={}",
                            device_id,
                            err,
                            text.len(),
                            truncate_for_log(text.as_ref(), 160)
                        );
                    }
                }
            }
            Ok(Message::Close(frame)) => {
                disconnect_reason = format!("close_frame: {}", close_frame_summary(frame.as_ref()));
                break;
            }
            Ok(Message::Ping(payload)) => {
                let _ = tx.send(Message::Pong(payload));
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Binary(_)) => {}
            Err(err) => {
                disconnect_reason = format!("ws_error: {err}");
                eprintln!("[relay-client-ws-error] device_id={} err={}", device_id, err);
                break;
            }
        }
    }
    eprintln!(
        "[relay-client] disconnect device_id={} reason={}",
        device_id, disconnect_reason
    );

    {
        let mut devices = state.inner.devices.lock().await;
        devices.remove(&device_id);
    }
    let device_id_for_store = device_id.clone();
    let device_name_for_store = device_name.clone();
    let workspaces_for_store = workspaces.clone();
    if let Err(err) = update_store(&state, |store| {
        store.devices.insert(
            device_id_for_store.clone(),
            DeviceStatusMessage {
                device_id: device_id_for_store.clone(),
                device_name: device_name_for_store.clone(),
                connected: false,
                workspaces: workspaces_for_store.clone(),
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
            workspaces,
        },
    )
    .await;
    writer.abort();
}

async fn handle_browser_socket(state: AppState, socket: WebSocket, browser: AuthenticatedBrowser) {
    let browser_id = state.next_browser_id();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();

    {
        let mut browsers = state.inner.browsers.lock().await;
        browsers.insert(
            browser_id.clone(),
            BrowserConnection {
                sender: tx.clone(),
                user_id: browser.user_id.clone(),
            },
        );
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
    if let Err(err) = send_session_list(&state, &browser_id, &browser.user_id).await {
        eprintln!("[relay-browser-init-error] {err}");
    }

    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<BrowserToServerMessage>(text.as_ref()) {
                    Ok(message) => {
                        if let Err(err) =
                            handle_browser_message(&state, &browser_id, &browser.user_id, message)
                                .await
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
) -> Result<(String, String, Vec<WorkspaceSummaryMessage>, String)> {
    while let Some(result) = ws_receiver.next().await {
        match result {
            Ok(Message::Text(text)) => {
                let message = serde_json::from_str::<ClientToServerMessage>(text.as_ref())
                    .context("解析 client hello 失败")?;
                if let ClientToServerMessage::Hello {
                    device_id,
                    device_name,
                    workspaces,
                    auth_token,
                    ..
                } = message
                {
                    return Ok((device_id, device_name, workspaces, auth_token));
                }

                return Err(anyhow!(
                    "client 第一个消息必须是 hello，收到 {}",
                    client_message_kind(&message)
                ));
            }
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(frame)) => {
                return Err(anyhow!(
                    "client 在 hello 前断开连接 ({})",
                    close_frame_summary(frame.as_ref())
                ));
            }
            Ok(Message::Binary(payload)) => {
                return Err(anyhow!(
                    "client hello 不支持 binary 消息 (bytes={})",
                    payload.len()
                ));
            }
            Err(err) => return Err(anyhow!("读取 client hello 失败: {err}")),
            Ok(Message::Frame(_)) => {}
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
            resume_device_sessions(state, device_id).await?;
        }
        ClientToServerMessage::Info { message } => {
            broadcast_info(state, format!("device {device_id}: {message}")).await;
        }
        ClientToServerMessage::SessionCreated {
            session_id,
            client_session_id,
            workspace_id,
            workspace_name,
        } => {
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let client_session_id_for_store = client_session_id.clone();
            let workspace_id_for_store = workspace_id.clone();
            let workspace_name_for_store = workspace_name.clone();
            {
                let mut sessions = state.inner.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.client_session_id = Some(client_session_id.clone());
                }
            }
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    if !session_is_closed(&summary.summary.status) {
                        summary.summary.status = "ready".to_string();
                    }
                    summary.summary.workspace_id = Some(workspace_id_for_store.clone());
                    summary.summary.workspace_name = Some(workspace_name_for_store.clone());
                    summary.summary.updated_at = now;
                    summary.client_session_id = Some(client_session_id_for_store.clone());
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "session_created".to_string(),
                    text: Some(format!(
                        "{} ({})",
                        workspace_name_for_store, workspace_id_for_store
                    )),
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;
            broadcast_session_updated(state, &session_id).await?;

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
                        workspace_id,
                        workspace_name,
                    },
                )
                .await?;
            }
        }
        ClientToServerMessage::SessionResumed {
            session_id,
            client_session_id,
        } => {
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let client_session_id_for_store = client_session_id.clone();
            {
                let mut sessions = state.inner.sessions.lock().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.client_session_id = Some(client_session_id.clone());
                }
            }
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    if !session_is_closed(&summary.summary.status) {
                        summary.summary.status = "idle".to_string();
                    }
                    summary.summary.updated_at = now;
                    summary.client_session_id = Some(client_session_id_for_store.clone());
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "session_resumed".to_string(),
                    text: Some(client_session_id_for_store.clone()),
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;
            broadcast_session_updated(state, &session_id).await?;
        }
        ClientToServerMessage::SessionResumeFailed {
            session_id,
            client_session_id,
            message,
        } => {
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let client_session_id_for_store = client_session_id.clone();
            let message_for_store = message.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    if !session_is_closed(&summary.summary.status) {
                        summary.summary.status = "resume_failed".to_string();
                    }
                    summary.summary.updated_at = now;
                    summary.client_session_id = Some(client_session_id_for_store.clone());
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "session_resume_failed".to_string(),
                    text: Some(message_for_store.clone()),
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;
            broadcast_session_updated(state, &session_id).await?;

            if let Some(browser_id) = active_browser_for_session(state, &session_id).await {
                send_to_browser(
                    state,
                    &browser_id,
                    ServerToBrowserMessage::Error {
                        session_id: Some(session_id),
                        message: format!("session 恢复失败，acp={client_session_id}: {message}"),
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
                    summary.summary.updated_at = now;
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
            broadcast_session_updated(state, &session_id).await?;
            route_session_message(
                state,
                &session_id,
                ServerToBrowserMessage::OutputChunk {
                    session_id: session_id.clone(),
                    text,
                },
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
                    if !session_is_closed(&summary.summary.status) {
                        summary.summary.status = "idle".to_string();
                    }
                    summary.summary.updated_at = now;
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
                    session_id: session_id.clone(),
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
                    if let Some(session_id) = session_id_for_store.as_ref()
                        && let Some(summary) = store.sessions.get_mut(session_id)
                        && !session_is_closed(&summary.summary.status)
                    {
                        summary.summary.status = "awaiting_permission".to_string();
                        summary.summary.updated_at = now;
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
                if let Some(session_id) = session_id.as_deref() {
                    broadcast_session_updated(state, session_id).await?;
                }

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
    browser_user_id: &str,
    message: BrowserToServerMessage,
) -> Result<()> {
    match message {
        BrowserToServerMessage::ListDevices => send_device_list(state, browser_id).await?,
        BrowserToServerMessage::ListSessions => {
            send_session_list(state, browser_id, browser_user_id).await?
        }
        BrowserToServerMessage::CreateSession {
            device_id,
            workspace_id,
        } => {
            ensure_device_connected(state, &device_id).await?;
            let workspace = resolve_device_workspace(state, &device_id, &workspace_id).await?;
            let session_id = state.next_session_id();
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let device_id_for_store = device_id.clone();
            let workspace_id_for_store = workspace.workspace_id.clone();
            let workspace_name_for_store = workspace.workspace_name.clone();

            {
                let mut sessions = state.inner.sessions.lock().await;
                sessions.insert(
                    session_id.clone(),
                    RelaySession {
                        session_id: session_id.clone(),
                        device_id: device_id.clone(),
                        browser_id: browser_id.to_string(),
                        owner_user_id: browser_user_id.to_string(),
                        client_session_id: None,
                    },
                );
            }
            update_store(state, |store| {
                store.sessions.insert(
                    session_id_for_store.clone(),
                    PersistedRelaySession {
                        summary: RelaySessionSummaryMessage {
                            session_id: session_id_for_store.clone(),
                            device_id: device_id_for_store.clone(),
                            status: "requested".to_string(),
                            owner_user_id: Some(browser_user_id.to_string()),
                            workspace_id: Some(workspace_id_for_store.clone()),
                            workspace_name: Some(workspace_name_for_store.clone()),
                            created_at: now,
                            updated_at: now,
                        },
                        client_session_id: None,
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
            broadcast_session_updated(state, &session_id).await?;

            send_to_client(
                state,
                &device_id,
                ServerToClientMessage::CreateSession {
                    session_id,
                    workspace_id,
                },
            )
            .await?;
        }
        BrowserToServerMessage::AdoptSession { session_id } => {
            let mut claimed_legacy_owner = false;
            let previous_session = {
                let store = state.inner.store.lock().await;
                store.sessions.get(&session_id).cloned()
            };
            {
                let mut sessions = state.inner.sessions.lock().await;
                let session = sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
                if !session_owned_by_user_id(&session.owner_user_id, browser_user_id) {
                    return Err(anyhow!("当前用户无权访问该 session"));
                }
                session.browser_id = browser_id.to_string();
                if session.owner_user_id.is_empty() {
                    session.owner_user_id = browser_user_id.to_string();
                    claimed_legacy_owner = true;
                }
            }

            if claimed_legacy_owner {
                let session_id_for_store = session_id.clone();
                let browser_user_id_for_store = browser_user_id.to_string();
                update_store(state, |store| {
                    if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                        summary.summary.owner_user_id = Some(browser_user_id_for_store.clone());
                    }
                })
                .await?;
                if let Some(previous_session) = previous_session.as_ref() {
                    broadcast_session_visibility_transition(state, previous_session, &session_id)
                        .await?;
                } else {
                    broadcast_session_updated(state, &session_id).await?;
                }
            }

            send_session_list(state, browser_id, browser_user_id).await?;
        }
        BrowserToServerMessage::Prompt { session_id, text } => {
            let summary = get_visible_session_summary(state, browser_user_id, &session_id).await?;
            if session_is_closed(&summary.summary.status) {
                return Err(anyhow!("session 已关闭，不能再发送 prompt"));
            }
            let session =
                get_browser_session(state, browser_id, browser_user_id, &session_id).await?;
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session.session_id.clone();
            let text_for_store = text.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    summary.summary.status = "running".to_string();
                    summary.summary.updated_at = now;
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
            broadcast_session_updated(state, &session_id).await?;
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
                let summary = store
                    .sessions
                    .get(&session_id)
                    .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
                if !session_summary_visible_to_user(summary, browser_user_id) {
                    return Err(anyhow!("当前用户无权访问该 session"));
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
                if let Some(session_id) = pending_session_id.as_ref()
                    && let Some(summary) = store.sessions.get_mut(session_id)
                {
                    summary.summary.status = "running".to_string();
                    summary.summary.updated_at = now;
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
            if let Some(session_id) = pending.session_id.as_deref() {
                broadcast_session_updated(state, session_id).await?;
            }

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
        BrowserToServerMessage::CloseSession { session_id } => {
            let summary = get_visible_session_summary(state, browser_user_id, &session_id).await?;
            if session_is_closed(&summary.summary.status) {
                send_to_browser(
                    state,
                    browser_id,
                    ServerToBrowserMessage::SessionClosed {
                        session_id: session_id.clone(),
                    },
                )
                .await?;
                return Ok(());
            }

            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    summary.summary.status = "closed".to_string();
                    summary.summary.updated_at = now;
                }
                store.events.push(SessionEventMessage {
                    seq,
                    seq_end: None,
                    session_id: session_id_for_store.clone(),
                    timestamp: now,
                    kind: "session_closed".to_string(),
                    text: None,
                    stop_reason: None,
                    request_id: None,
                    options: None,
                });
            })
            .await?;

            {
                let mut permissions = state.inner.permissions.lock().await;
                permissions.retain(|_, pending| pending.session_id.as_deref() != Some(&session_id));
            }

            if let Some(relay_session) = {
                let sessions = state.inner.sessions.lock().await;
                sessions.get(&session_id).cloned()
            } && relay_session.client_session_id.is_some()
            {
                let _ = send_to_client(
                    state,
                    &relay_session.device_id,
                    ServerToClientMessage::CancelSession {
                        session_id: session_id.clone(),
                    },
                )
                .await;
            }

            broadcast_session_updated(state, &session_id).await?;
            broadcast_session_closed(state, &session_id).await?;
        }
        BrowserToServerMessage::DeleteSession { session_id } => {
            let previous = get_visible_session_summary(state, browser_user_id, &session_id).await?;
            if !session_is_closed(&previous.summary.status) {
                return Err(anyhow!("只能删除已关闭的 session"));
            }
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();

            {
                let mut sessions = state.inner.sessions.lock().await;
                sessions.remove(&session_id);
            }
            {
                let mut permissions = state.inner.permissions.lock().await;
                permissions.retain(|_, pending| pending.session_id.as_deref() != Some(&session_id));
            }

            update_store(state, |store| {
                delete_session_from_store(store, &session_id, browser_user_id, now, seq);
            })
            .await?;

            broadcast_session_delta(state, Some(&previous), None).await?;
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

async fn send_session_list(
    state: &AppState,
    browser_id: &str,
    browser_user_id: &str,
) -> Result<()> {
    let sessions = {
        let store = state.inner.store.lock().await;
        let mut sessions = store
            .sessions
            .values()
            .filter(|session| session_summary_visible_to_user(session, browser_user_id))
            .map(|session| session.summary.clone())
            .collect::<Vec<_>>();
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

fn session_visibility_change(
    previous: Option<&PersistedRelaySession>,
    current: Option<&PersistedRelaySession>,
    browser_user_id: &str,
) -> SessionVisibilityChange {
    let was_visible =
        previous.is_some_and(|session| session_summary_visible_to_user(session, browser_user_id));
    let is_visible =
        current.is_some_and(|session| session_summary_visible_to_user(session, browser_user_id));

    match (was_visible, is_visible) {
        (_, true) => SessionVisibilityChange::Updated,
        (true, false) => SessionVisibilityChange::Removed,
        (false, false) => SessionVisibilityChange::None,
    }
}

async fn current_session_snapshot(
    state: &AppState,
    session_id: &str,
) -> Result<PersistedRelaySession> {
    {
        let store = state.inner.store.lock().await;
        store.sessions.get(session_id).cloned()
    }
    .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))
}

async fn broadcast_session_delta(
    state: &AppState,
    previous: Option<&PersistedRelaySession>,
    current: Option<&PersistedRelaySession>,
) -> Result<()> {
    let session_id = current
        .map(|session| session.summary.session_id.clone())
        .or_else(|| previous.map(|session| session.summary.session_id.clone()))
        .ok_or_else(|| anyhow!("session delta 缺少 session"))?;

    let browsers = {
        let browsers = state.inner.browsers.lock().await;
        browsers
            .iter()
            .map(|(browser_id, browser)| (browser_id.clone(), browser.clone()))
            .collect::<Vec<_>>()
    };

    for (_browser_id, browser) in browsers {
        match session_visibility_change(previous, current, &browser.user_id) {
            SessionVisibilityChange::Updated => {
                if let Some(session) = current {
                    let _ = browser.sender.send(text_message(
                        &ServerToBrowserMessage::SessionUpdated {
                            session: session.summary.clone(),
                        },
                    ));
                }
            }
            SessionVisibilityChange::Removed => {
                let _ =
                    browser
                        .sender
                        .send(text_message(&ServerToBrowserMessage::SessionRemoved {
                            session_id: session_id.clone(),
                        }));
            }
            SessionVisibilityChange::None => {}
        }
    }

    Ok(())
}

async fn broadcast_session_closed(state: &AppState, session_id: &str) -> Result<()> {
    let session = current_session_snapshot(state, session_id).await?;
    let browsers = {
        let browsers = state.inner.browsers.lock().await;
        browsers.values().cloned().collect::<Vec<_>>()
    };

    for browser in browsers {
        if !session_summary_visible_to_user(&session, &browser.user_id) {
            continue;
        }

        let _ = browser
            .sender
            .send(text_message(&ServerToBrowserMessage::SessionClosed {
                session_id: session_id.to_string(),
            }));
    }

    Ok(())
}

async fn broadcast_session_updated(state: &AppState, session_id: &str) -> Result<()> {
    let current = current_session_snapshot(state, session_id).await?;
    broadcast_session_delta(state, None, Some(&current)).await
}

async fn broadcast_session_visibility_transition(
    state: &AppState,
    previous: &PersistedRelaySession,
    session_id: &str,
) -> Result<()> {
    let current = current_session_snapshot(state, session_id).await?;
    broadcast_session_delta(state, Some(previous), Some(&current)).await
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
        let message = Message::Text(text);
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
    let browser_id = active_browser_for_session(state, session_id)
        .await
        .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;

    send_to_browser(state, &browser_id, message).await
}

async fn active_browser_for_session(state: &AppState, session_id: &str) -> Option<String> {
    let sessions = state.inner.sessions.lock().await;
    sessions
        .get(session_id)
        .and_then(|session| (!session.browser_id.is_empty()).then(|| session.browser_id.clone()))
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

fn pending_resume_commands(store: &RelayStore, device_id: &str) -> Vec<ServerToClientMessage> {
    let mut candidates = store
        .sessions
        .values()
        .filter(|session| session.summary.device_id == device_id)
        .filter_map(|session| {
            let client_session_id = session.client_session_id.clone()?;
            let workspace_id = session.summary.workspace_id.clone()?;
            Some((
                session.summary.updated_at,
                ServerToClientMessage::ResumeSession {
                    session_id: session.summary.session_id.clone(),
                    client_session_id,
                    workspace_id,
                },
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(updated_at, _)| *updated_at);
    candidates.into_iter().map(|(_, command)| command).collect()
}

async fn resume_device_sessions(state: &AppState, device_id: &str) -> Result<()> {
    let commands = {
        let store = state.inner.store.lock().await;
        pending_resume_commands(&store, device_id)
    };

    for command in commands {
        send_to_client(state, device_id, command).await?;
    }

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

async fn get_visible_session_summary(
    state: &AppState,
    browser_user_id: &str,
    session_id: &str,
) -> Result<PersistedRelaySession> {
    let store = state.inner.store.lock().await;
    let session = store
        .sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
    if !session_summary_visible_to_user(&session, browser_user_id) {
        return Err(anyhow!("当前用户无权访问该 session"));
    }
    Ok(session)
}

async fn resolve_device_workspace(
    state: &AppState,
    device_id: &str,
    workspace_id: &str,
) -> Result<WorkspaceSummaryMessage> {
    let store = state.inner.store.lock().await;
    let device = store
        .devices
        .get(device_id)
        .ok_or_else(|| anyhow!("未知 device: {device_id}"))?;
    device
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .cloned()
        .ok_or_else(|| anyhow!("device `{device_id}` 不存在 workspace `{workspace_id}`"))
}

async fn get_browser_session(
    state: &AppState,
    browser_id: &str,
    browser_user_id: &str,
    session_id: &str,
) -> Result<RelaySession> {
    let sessions = state.inner.sessions.lock().await;
    let session = sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
    if !session_owned_by_user_id(&session.owner_user_id, browser_user_id) {
        return Err(anyhow!("当前用户无权访问该 session"));
    }
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
        let sessions = state.inner.sessions.lock().await;
        sessions
            .values()
            .filter(|session| session.device_id == device_id)
            .filter(|session| !session.browser_id.is_empty())
            .map(|session| session.browser_id.clone())
            .collect::<Vec<_>>()
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
            if session.summary.device_id == device_id_for_store {
                session.summary.status = "device_disconnected".to_string();
                session.summary.updated_at = now;
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

    let affected_session_ids = {
        let store = state.inner.store.lock().await;
        store
            .sessions
            .values()
            .filter(|session| session.summary.device_id == device_id)
            .map(|session| session.summary.session_id.clone())
            .collect::<Vec<_>>()
    };

    for session_id in affected_session_ids {
        let _ = broadcast_session_updated(state, &session_id).await;
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
        Ok(text) => Message::Text(text),
        Err(err) => Message::Text(
            serde_json::json!({
                "type": "error",
                "message": format!("server 序列化失败: {err}")
            })
            .to_string(),
        ),
    }
}

fn session_summary_visible_to_user(session: &PersistedRelaySession, browser_user_id: &str) -> bool {
    session_owned_by_user_id(
        session.summary.owner_user_id.as_deref().unwrap_or_default(),
        browser_user_id,
    )
}

fn session_owned_by_user_id(owner_user_id: &str, browser_user_id: &str) -> bool {
    owner_user_id.is_empty() || owner_user_id == browser_user_id
}

fn optional_nonempty_env(key: &str) -> Result<Option<String>> {
    match std::env::var(key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(anyhow!("{key} 不能为空字符串"))
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        Err(_) => Ok(None),
    }
}

fn required_nonempty_env(key: &str) -> Result<String> {
    optional_nonempty_env(key)?.ok_or_else(|| anyhow!("缺少环境变量 {key}"))
}

fn parse_string_set_env(key: &str) -> Result<HashSet<String>> {
    let Ok(raw) = std::env::var(key) else {
        return Ok(HashSet::new());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HashSet::new());
    }

    let values = trimmed
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();

    if values.is_empty() {
        return Err(anyhow!("{key} 至少需要包含一个非空值"));
    }

    Ok(values)
}

fn parse_client_token_set() -> Result<HashSet<String>> {
    parse_string_set_env("RELAY_SERVER_CLIENT_TOKENS")
}

fn parse_origin_set_env(key: &str) -> Result<HashSet<String>> {
    let origins = parse_string_set_env(key)?;
    let mut normalized = HashSet::with_capacity(origins.len());
    for origin in origins {
        let normalized_origin =
            normalize_origin(&origin).ok_or_else(|| anyhow!("{key} 包含非法 origin: {origin}"))?;
        normalized.insert(normalized_origin);
    }
    Ok(normalized)
}

fn normalize_origin(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_end_matches('/');
    let (scheme, rest) = trimmed.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    if rest.is_empty() || rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return None;
    }
    Some(format!("{scheme}://{}", rest.to_ascii_lowercase()))
}

fn cookie_value(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let cookie_header = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for segment in cookie_header.split(';') {
        let part = segment.trim();
        if part.is_empty() {
            continue;
        }
        let (name, value) = part.split_once('=')?;
        if name.trim() == cookie_name {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn validate_browser_origin(
    auth: &BrowserAuthConfig,
    headers: &HeaderMap,
) -> std::result::Result<(), Box<Response>> {
    if auth.allowed_origins.is_empty() {
        return Ok(());
    }

    let Some(origin_header) = header_value(headers, "Origin") else {
        return Err(unauthorized_response("browser origin header missing"));
    };
    let Some(origin) = normalize_origin(&origin_header) else {
        return Err(unauthorized_response("browser origin header invalid"));
    };
    if !auth.allowed_origins.contains(&origin) {
        return Err(unauthorized_response("browser origin not allowed"));
    }

    Ok(())
}

fn build_auth_cookie(
    cookie_name: &str,
    token: &str,
    max_age_secs: u64,
    secure: bool,
) -> Result<HeaderValue> {
    let secure_flag = if secure { "; Secure" } else { "" };
    let value = format!(
        "{cookie_name}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure_flag}"
    );
    HeaderValue::from_str(&value).context("构建登录 cookie 失败")
}

fn build_expired_auth_cookie(cookie_name: &str, secure: bool) -> Result<HeaderValue> {
    let secure_flag = if secure { "; Secure" } else { "" };
    let value = format!("{cookie_name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure_flag}");
    HeaderValue::from_str(&value).context("构建退出登录 cookie 失败")
}

fn is_client_token_allowed(auth: &ClientAuthConfig, token: &str) -> bool {
    if auth.revoked_tokens.contains(token) {
        return false;
    }
    if auth.accepted_tokens.is_empty() {
        return true;
    }
    auth.accepted_tokens.contains(token)
}

fn validate_auth_configuration(
    bind_addr: SocketAddr,
    allow_insecure_dev: bool,
    accepted_client_tokens: &HashSet<String>,
    browser_auth: &BrowserAuthConfig,
) -> Result<()> {
    if allow_insecure_dev {
        if !bind_addr.ip().is_loopback() {
            return Err(anyhow!(
                "RELAY_SERVER_ALLOW_INSECURE_DEV=true 仅允许在 loopback 监听地址上使用"
            ));
        }
        return Ok(());
    }

    if accepted_client_tokens.is_empty() {
        return Err(anyhow!(
            "缺少 RELAY_SERVER_CLIENT_TOKENS；生产模式必须启用 home client token 认证"
        ));
    }
    if browser_auth.allowed_origins.is_empty() {
        return Err(anyhow!(
            "生产模式必须设置 RELAY_SERVER_BROWSER_ALLOWED_ORIGINS（逗号分隔）用于浏览器 Origin 校验"
        ));
    }

    let config = &browser_auth.login;
    if config.username.trim().is_empty() {
        return Err(anyhow!("RELAY_SERVER_LOGIN_USERNAME 不能为空"));
    }
    if config.password.trim().is_empty() {
        return Err(anyhow!("RELAY_SERVER_LOGIN_PASSWORD 不能为空"));
    }

    Ok(())
}

fn parse_bool_env(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!("{key} 必须是布尔值")),
    }
}

async fn authenticate_browser_ws_request(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    authenticate_browser_request(state, headers, true).await
}

async fn authenticate_browser_http_request(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    authenticate_browser_request(state, headers, false).await
}

async fn authenticate_browser_request(
    state: &AppState,
    headers: &HeaderMap,
    enforce_origin: bool,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    let auth = &state.inner.browser_auth;
    if enforce_origin {
        validate_browser_origin(auth, headers)?;
    }

    let config = &auth.login;
    let Some(session_token) = cookie_value(headers, &config.cookie_name) else {
        return Err(unauthorized_response("browser login session missing"));
    };

    let now = unix_timestamp_secs();
    let mut login_sessions = state.inner.login_sessions.lock().await;
    let Some(session) = login_sessions.get(&session_token).cloned() else {
        return Err(unauthorized_response("browser login session invalid"));
    };
    if session.expires_at <= now {
        login_sessions.remove(&session_token);
        return Err(unauthorized_response("browser login session expired"));
    }

    Ok(AuthenticatedBrowser {
        user_id: session.user_id,
    })
}

async fn ensure_browser_http_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<AuthenticatedBrowser, Box<Response>> {
    match authenticate_browser_http_request(state, headers).await {
        Ok(browser) => Ok(browser),
        Err(_) => Err(Box::new(Redirect::to("/login").into_response())),
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn unauthorized_response(message: &str) -> Box<Response> {
    Box::new((StatusCode::UNAUTHORIZED, message.to_string()).into_response())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn persisted_session(
        session_id: &str,
        device_id: &str,
        workspace_id: Option<&str>,
        client_session_id: Option<&str>,
        updated_at: u64,
    ) -> PersistedRelaySession {
        PersistedRelaySession {
            summary: RelaySessionSummaryMessage {
                session_id: session_id.to_string(),
                device_id: device_id.to_string(),
                status: "idle".to_string(),
                owner_user_id: None,
                workspace_id: workspace_id.map(str::to_string),
                workspace_name: Some("Workspace".to_string()),
                created_at: updated_at,
                updated_at,
            },
            client_session_id: client_session_id.map(str::to_string),
        }
    }

    #[test]
    fn session_visibility_change_removes_users_who_lose_access() {
        let previous = persisted_session(
            "relay-session-1",
            "device-a",
            Some("ws-a"),
            Some("acp-1"),
            10,
        );
        let mut current = previous.clone();
        current.summary.owner_user_id = Some("alice".to_string());

        assert_eq!(
            session_visibility_change(Some(&previous), Some(&current), "alice"),
            SessionVisibilityChange::Updated
        );
        assert_eq!(
            session_visibility_change(Some(&previous), Some(&current), "bob"),
            SessionVisibilityChange::Removed
        );
    }

    #[test]
    fn pending_resume_commands_only_include_bound_sessions_for_device() {
        let mut store = RelayStore::default();
        store.sessions.insert(
            "relay-session-2".to_string(),
            persisted_session(
                "relay-session-2",
                "device-a",
                Some("ws-b"),
                Some("acp-2"),
                20,
            ),
        );
        store.sessions.insert(
            "relay-session-1".to_string(),
            persisted_session(
                "relay-session-1",
                "device-a",
                Some("ws-a"),
                Some("acp-1"),
                10,
            ),
        );
        store.sessions.insert(
            "relay-session-missing-binding".to_string(),
            persisted_session(
                "relay-session-missing-binding",
                "device-a",
                Some("ws-c"),
                None,
                30,
            ),
        );
        store.sessions.insert(
            "relay-session-other-device".to_string(),
            persisted_session(
                "relay-session-other-device",
                "device-b",
                Some("ws-z"),
                Some("acp-z"),
                5,
            ),
        );

        let commands = pending_resume_commands(&store, "device-a");

        assert_eq!(commands.len(), 2);
        match &commands[0] {
            ServerToClientMessage::ResumeSession {
                session_id,
                client_session_id,
                workspace_id,
            } => {
                assert_eq!(session_id, "relay-session-1");
                assert_eq!(client_session_id, "acp-1");
                assert_eq!(workspace_id, "ws-a");
            }
            other => panic!("unexpected command: {other:?}"),
        }
        match &commands[1] {
            ServerToClientMessage::ResumeSession {
                session_id,
                client_session_id,
                workspace_id,
            } => {
                assert_eq!(session_id, "relay-session-2");
                assert_eq!(client_session_id, "acp-2");
                assert_eq!(workspace_id, "ws-b");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn delete_session_from_store_removes_session_history_but_keeps_audit_event() {
        let mut store = RelayStore::default();
        store.sessions.insert(
            "relay-session-1".to_string(),
            persisted_session(
                "relay-session-1",
                "device-a",
                Some("ws-a"),
                Some("acp-1"),
                10,
            ),
        );
        store.events.push(SessionEventMessage {
            seq: 1,
            seq_end: None,
            session_id: "relay-session-1".to_string(),
            timestamp: 10,
            kind: "prompt_submitted".to_string(),
            text: Some("hello".to_string()),
            stop_reason: None,
            request_id: None,
            options: None,
        });
        store.events.push(SessionEventMessage {
            seq: 2,
            seq_end: None,
            session_id: "other-session".to_string(),
            timestamp: 11,
            kind: "prompt_submitted".to_string(),
            text: Some("keep me".to_string()),
            stop_reason: None,
            request_id: None,
            options: None,
        });

        delete_session_from_store(&mut store, "relay-session-1", "alice", 12, 3);

        assert!(!store.sessions.contains_key("relay-session-1"));
        assert_eq!(store.events.len(), 2);
        assert_eq!(store.events[0].session_id, "other-session");
        assert_eq!(store.events[1].kind, "session_deleted");
        assert_eq!(store.events[1].session_id, "");
        assert_eq!(
            store.events[1].text.as_deref(),
            Some("session_id=relay-session-1 deleted_by=alice")
        );
    }

    #[test]
    fn strict_auth_configuration_requires_tokens_and_browser_guardrails() {
        let bind = "127.0.0.1:8080".parse().expect("valid addr");
        let mut tokens = HashSet::new();
        tokens.insert("token".to_string());
        let mut origins = HashSet::new();
        origins.insert("https://relay.example.com".to_string());
        let local_auth = BrowserAuthConfig {
            login: LocalLoginAuthConfig {
                username: "admin".to_string(),
                password: "password".to_string(),
                session_ttl_secs: 3600,
                cookie_name: "relay_session".to_string(),
                cookie_secure: true,
            },
            allowed_origins: origins.clone(),
        };

        let missing_client_token =
            validate_auth_configuration(bind, false, &HashSet::new(), &local_auth);
        assert!(missing_client_token.is_err());

        let mut no_origin_auth = local_auth.clone();
        no_origin_auth.allowed_origins = HashSet::new();
        let missing_origin = validate_auth_configuration(bind, false, &tokens, &no_origin_auth);
        assert!(missing_origin.is_err());

        validate_auth_configuration(bind, false, &tokens, &local_auth)
            .expect("strict mode with local login config should pass");
    }

    #[test]
    fn insecure_dev_mode_only_allows_loopback_bind() {
        let loopback = "127.0.0.1:8080".parse().expect("valid addr");
        let local_auth = BrowserAuthConfig {
            login: LocalLoginAuthConfig {
                username: "admin".to_string(),
                password: "password".to_string(),
                session_ttl_secs: 3600,
                cookie_name: "relay_session".to_string(),
                cookie_secure: false,
            },
            allowed_origins: HashSet::new(),
        };
        validate_auth_configuration(loopback, true, &HashSet::new(), &local_auth)
            .expect("insecure dev mode should allow loopback");

        let non_loopback = "0.0.0.0:8080".parse().expect("valid addr");
        let result = validate_auth_configuration(non_loopback, true, &HashSet::new(), &local_auth);
        assert!(result.is_err());
    }

    #[test]
    fn revoked_client_token_is_rejected_even_if_in_allowlist() {
        let mut accepted = HashSet::new();
        accepted.insert("token-a".to_string());
        accepted.insert("token-b".to_string());
        let mut revoked = HashSet::new();
        revoked.insert("token-b".to_string());
        let auth = ClientAuthConfig {
            accepted_tokens: accepted,
            revoked_tokens: revoked,
        };

        assert!(is_client_token_allowed(&auth, "token-a"));
        assert!(!is_client_token_allowed(&auth, "token-b"));
        assert!(!is_client_token_allowed(&auth, "unknown"));
    }

    #[test]
    fn normalize_origin_requires_scheme_and_host_only() {
        assert_eq!(
            normalize_origin("https://Relay.Example.com"),
            Some("https://relay.example.com".to_string())
        );
        assert_eq!(
            normalize_origin("https://relay.example.com/"),
            Some("https://relay.example.com".to_string())
        );
        assert!(normalize_origin("https://relay.example.com/path").is_none());
        assert!(normalize_origin("javascript:alert(1)").is_none());
    }
}
