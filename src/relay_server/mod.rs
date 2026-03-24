mod auth;
mod browser_messages;
mod client_messages;
mod connections;
mod pages;
mod session_helpers;

use crate::protocol::{
    BrowserToServerMessage, ClientToServerMessage, DeviceStatusMessage, ServerToBrowserMessage,
};
use crate::relay_state::{RelayPersistence, RelayStore};
use anyhow::{Context, Result, anyhow};
use auth::{
    AuthenticatedBrowser, BrowserAuthConfig, LocalLoginAuthConfig, authenticate_browser_ws_request,
    login_page, login_submit, logout_submit, parse_bool_env, parse_origin_set_env,
    required_nonempty_env, validate_auth_configuration,
};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
    routing::{get, post},
};
use browser_messages::{handle_browser_message, send_device_list, send_session_list};
use client_messages::handle_client_message;
use connections::{
    RELAY_CLOSE_REASON_BROWSER_BINARY_UNSUPPORTED, RELAY_CLOSE_REASON_BROWSER_JSON_INVALID,
    RELAY_CLOSE_REASON_CLIENT_AUTH_REJECTED, RELAY_CLOSE_REASON_CLIENT_HELLO_INVALID,
    broadcast_device_status, cleanup_browser, cleanup_device, close_frame_summary,
    is_client_token_allowed, read_client_hello, relay_close_message, send_to_browser,
    truncate_for_log,
};
use futures_util::{SinkExt, StreamExt};
use pages::{app_icon, app_maskable_icon, debug_console, healthz, service_worker, web_manifest};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionVisibilityChange {
    None,
    Updated,
    Removed,
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
            let _ = tx.send(relay_close_message(
                axum::extract::ws::close_code::PROTOCOL,
                RELAY_CLOSE_REASON_CLIENT_HELLO_INVALID,
            ));
            drop(tx);
            let _ = writer.await;
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
        let _ = tx.send(relay_close_message(
            axum::extract::ws::close_code::POLICY,
            RELAY_CLOSE_REASON_CLIENT_AUTH_REJECTED,
        ));
        drop(tx);
        let _ = writer.await;
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
                            eprintln!(
                                "[relay-client-message-error] device_id={} err={}",
                                device_id, err
                            );
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
                eprintln!(
                    "[relay-client-ws-error] device_id={} err={}",
                    device_id, err
                );
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
    drop(tx);
    let _ = writer.await;
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
                        eprintln!(
                            "[relay-browser-parse-error] browser_id={} err={} payload_len={} payload_preview={}",
                            browser_id,
                            err,
                            text.len(),
                            truncate_for_log(text.as_ref(), 160)
                        );
                        let _ = tx.send(relay_close_message(
                            axum::extract::ws::close_code::INVALID,
                            RELAY_CLOSE_REASON_BROWSER_JSON_INVALID,
                        ));
                        break;
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(payload)) => {
                let _ = tx.send(Message::Pong(payload));
            }
            Ok(Message::Pong(_)) => {}
            Ok(Message::Binary(payload)) => {
                eprintln!(
                    "[relay-browser-binary-unsupported] browser_id={} bytes={}",
                    browser_id,
                    payload.len()
                );
                let _ = tx.send(relay_close_message(
                    axum::extract::ws::close_code::UNSUPPORTED,
                    RELAY_CLOSE_REASON_BROWSER_BINARY_UNSUPPORTED,
                ));
                break;
            }
            Err(err) => {
                eprintln!("[relay-browser-ws-error] {err}");
                break;
            }
        }
    }

    cleanup_browser(&state, &browser_id).await;
    drop(tx);
    let _ = writer.await;
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

async fn update_store<F>(state: &AppState, mutate: F) -> Result<()>
where
    F: FnOnce(&mut RelayStore),
{
    let (snapshot, appended_events, rewritten_log_events, persistence) = {
        let mut store = state.inner.store.lock().await;
        let prior_event_count = store.events.len();
        mutate(&mut store);
        store.normalize_checkpoint_boundary();
        let persistence = state.inner.persistence.clone();

        if store.live_event_count() >= persistence.compaction_threshold() {
            store.compact_into_checkpoint();
            let live_tail = store.live_events().to_vec();
            (store.clone(), Vec::new(), Some(live_tail), persistence)
        } else {
            let appended_events = store.events_from(prior_event_count).to_vec();
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
    use crate::protocol::{RelaySessionSummaryMessage, ServerToClientMessage, SessionEventMessage};
    use crate::relay_server::session_helpers::{
        SESSION_STATUS_CLOSED, SESSION_STATUS_IDLE, SESSION_STATUS_RESUME_FAILED,
        delete_session_from_store, pending_resume_commands, session_visibility_change,
    };
    use crate::relay_state::PersistedRelaySession;
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
                status: SESSION_STATUS_IDLE.to_string(),
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
    fn pending_resume_commands_skip_closed_and_resume_failed_sessions() {
        let mut store = RelayStore::default();

        store.sessions.insert(
            "relay-session-idle".to_string(),
            persisted_session(
                "relay-session-idle",
                "device-a",
                Some("ws-a"),
                Some("acp-a"),
                10,
            ),
        );
        store.sessions.insert(
            "relay-session-resume-failed".to_string(),
            persisted_session(
                "relay-session-resume-failed",
                "device-a",
                Some("ws-b"),
                Some("acp-b"),
                11,
            ),
        );
        store.sessions.insert(
            "relay-session-closed".to_string(),
            persisted_session(
                "relay-session-closed",
                "device-a",
                Some("ws-c"),
                Some("acp-c"),
                12,
            ),
        );

        if let Some(session) = store.sessions.get_mut("relay-session-resume-failed") {
            session.summary.status = SESSION_STATUS_RESUME_FAILED.to_string();
        }
        if let Some(session) = store.sessions.get_mut("relay-session-closed") {
            session.summary.status = SESSION_STATUS_CLOSED.to_string();
        }

        let commands = pending_resume_commands(&store, "device-a");

        assert_eq!(commands.len(), 1);
        match &commands[0] {
            ServerToClientMessage::ResumeSession {
                session_id,
                client_session_id,
                workspace_id,
            } => {
                assert_eq!(session_id, "relay-session-idle");
                assert_eq!(client_session_id, "acp-a");
                assert_eq!(workspace_id, "ws-a");
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
}
