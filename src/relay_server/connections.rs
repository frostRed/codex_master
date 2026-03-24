use super::{AppState, ClientAuthConfig, SessionVisibilityChange};
use crate::protocol::{
    ClientToServerMessage, DeviceStatusMessage, ServerToBrowserMessage, ServerToClientMessage,
    WorkspaceSummaryMessage,
};
use crate::relay_state::PersistedRelaySession;
use anyhow::{Context, Result, anyhow};
use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt;

use super::session_helpers::{
    current_session_snapshot, mark_device_disconnected, pending_resume_commands,
    session_summary_visible_to_user, session_visibility_change,
};

pub(super) const RELAY_CLOSE_REASON_CLIENT_HELLO_INVALID: &str =
    "relay.protocol.client_hello_invalid";
pub(super) const RELAY_CLOSE_REASON_CLIENT_AUTH_REJECTED: &str = "relay.auth.client_token_rejected";
pub(super) const RELAY_CLOSE_REASON_BROWSER_JSON_INVALID: &str =
    "relay.protocol.browser_json_invalid";
pub(super) const RELAY_CLOSE_REASON_BROWSER_BINARY_UNSUPPORTED: &str =
    "relay.protocol.browser_binary_unsupported";

pub(super) fn relay_close_message(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(axum::extract::ws::CloseFrame {
        code,
        reason: reason.into(),
    }))
}

pub(super) async fn read_client_hello(
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
        }
    }

    Err(anyhow!("client 在发送 hello 前断开连接"))
}

pub(super) async fn broadcast_session_delta(
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

pub(super) async fn broadcast_session_closed(state: &AppState, session_id: &str) -> Result<()> {
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

pub(super) async fn broadcast_session_updated(state: &AppState, session_id: &str) -> Result<()> {
    let current = current_session_snapshot(state, session_id).await?;
    broadcast_session_delta(state, None, Some(&current)).await
}

pub(super) async fn broadcast_session_visibility_transition(
    state: &AppState,
    previous: &PersistedRelaySession,
    session_id: &str,
) -> Result<()> {
    let current = current_session_snapshot(state, session_id).await?;
    broadcast_session_delta(state, Some(previous), Some(&current)).await
}

pub(super) async fn broadcast_device_status(state: &AppState, device: DeviceStatusMessage) {
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

pub(super) async fn broadcast_info(state: &AppState, message: String) {
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

pub(super) async fn route_session_message(
    state: &AppState,
    session_id: &str,
    message: ServerToBrowserMessage,
) -> Result<()> {
    let browser_id = active_browser_for_session(state, session_id)
        .await
        .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;

    send_to_browser(state, &browser_id, message).await
}

pub(super) async fn active_browser_for_session(
    state: &AppState,
    session_id: &str,
) -> Option<String> {
    let sessions = state.inner.sessions.lock().await;
    sessions
        .get(session_id)
        .and_then(|session| (!session.browser_id.is_empty()).then(|| session.browser_id.clone()))
}

pub(super) async fn send_to_browser(
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

pub(super) async fn send_to_client(
    state: &AppState,
    device_id: &str,
    message: ServerToClientMessage,
) -> Result<()> {
    let message_kind = server_message_kind(&message);
    let sender = {
        let devices = state.inner.devices.lock().await;
        devices
            .get(device_id)
            .map(|device| device.sender.clone())
            .ok_or_else(|| anyhow!("device 未连接: {device_id}"))?
    };
    sender.send(text_message(&message)).map_err(|_| {
        eprintln!(
            "[relay-send-client-error] device_id={} kind={} err=channel closed",
            device_id, message_kind
        );
        anyhow!("client channel 已关闭")
    })?;
    eprintln!(
        "[relay-send-client] device_id={} kind={}",
        device_id, message_kind
    );
    Ok(())
}

pub(super) async fn resume_device_sessions(state: &AppState, device_id: &str) -> Result<()> {
    let commands = {
        let store = state.inner.store.lock().await;
        pending_resume_commands(&store, device_id)
    };

    for command in commands {
        send_to_client(state, device_id, command).await?;
    }

    Ok(())
}

pub(super) async fn ensure_device_connected(state: &AppState, device_id: &str) -> Result<()> {
    let devices = state.inner.devices.lock().await;
    if devices.contains_key(device_id) {
        Ok(())
    } else {
        Err(anyhow!("device 未连接: {device_id}"))
    }
}

pub(super) async fn cleanup_browser(state: &AppState, browser_id: &str) {
    {
        let mut browsers = state.inner.browsers.lock().await;
        browsers.remove(browser_id);
    }

    {
        let mut permissions = state.inner.permissions.lock().await;
        permissions.retain(|_, pending| pending.browser_id != browser_id);
    }
}

pub(super) async fn cleanup_device(state: &AppState, device_id: &str) {
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

    let affected_session_ids = match mark_device_disconnected(state, device_id).await {
        Ok(session_ids) => session_ids,
        Err(err) => {
            eprintln!("[relay-persist-error] {err}");
            Vec::new()
        }
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

pub(super) fn text_message<T: serde::Serialize>(message: &T) -> Message {
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

pub(super) fn client_message_kind(message: &ClientToServerMessage) -> &'static str {
    match message {
        ClientToServerMessage::Hello { .. } => "hello",
        ClientToServerMessage::Ready { .. } => "ready",
        ClientToServerMessage::Info { .. } => "info",
        ClientToServerMessage::SessionCreated { .. } => "session_created",
        ClientToServerMessage::SessionResumed { .. } => "session_resumed",
        ClientToServerMessage::SessionResumeFailed { .. } => "session_resume_failed",
        ClientToServerMessage::OutputChunk { .. } => "output_chunk",
        ClientToServerMessage::PromptFinished { .. } => "prompt_finished",
        ClientToServerMessage::PermissionRequested { .. } => "permission_requested",
        ClientToServerMessage::Error { .. } => "error",
    }
}

pub(super) fn server_message_kind(message: &ServerToClientMessage) -> &'static str {
    match message {
        ServerToClientMessage::CreateSession { .. } => "create_session",
        ServerToClientMessage::ResumeSession { .. } => "resume_session",
        ServerToClientMessage::Prompt { .. } => "prompt",
        ServerToClientMessage::ResolvePermission { .. } => "resolve_permission",
        ServerToClientMessage::CancelSession { .. } => "cancel_session",
        ServerToClientMessage::Ping => "ping",
        ServerToClientMessage::Shutdown => "shutdown",
    }
}

pub(super) fn close_frame_summary(frame: Option<&axum::extract::ws::CloseFrame>) -> String {
    match frame {
        Some(frame) => format!("code={} reason={}", frame.code, frame.reason),
        None => "none".to_string(),
    }
}

pub(super) fn truncate_for_log(text: &str, max_chars: usize) -> String {
    let mut truncated = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max_chars {
            truncated.push_str("...");
            break;
        }
        truncated.push(ch);
    }
    truncated.replace('\n', "\\n")
}

pub(super) fn is_client_token_allowed(auth: &ClientAuthConfig, token: &str) -> bool {
    if auth.revoked_tokens.contains(token) {
        return false;
    }
    if auth.accepted_tokens.is_empty() {
        return true;
    }
    auth.accepted_tokens.contains(token)
}
