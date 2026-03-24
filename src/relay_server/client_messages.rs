use super::connections::{
    active_browser_for_session, broadcast_info, broadcast_session_updated, resume_device_sessions,
    route_session_message, send_to_browser,
};
use super::{AppState, PendingPermission, update_store};
use crate::protocol::{ClientToServerMessage, ServerToBrowserMessage, SessionEventMessage};
use crate::relay_state::unix_timestamp_secs;
use anyhow::{Result, anyhow};

use super::session_helpers::{
    SESSION_STATUS_AWAITING_PERMISSION, SESSION_STATUS_IDLE, SESSION_STATUS_READY,
    SESSION_STATUS_RESUME_FAILED, resolve_client_session_alias_in_map, session_is_closed,
};

pub(super) async fn handle_client_message(
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
                        summary.summary.status = SESSION_STATUS_READY.to_string();
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
                        summary.summary.status = SESSION_STATUS_IDLE.to_string();
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
                        summary.summary.status = SESSION_STATUS_RESUME_FAILED.to_string();
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
            let session_id = resolve_client_session_alias(state, device_id, &session_id)
                .await
                .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
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
            let session_id = resolve_client_session_alias(state, device_id, &session_id)
                .await
                .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))?;
            let now = unix_timestamp_secs();
            let seq = state.next_event_seq();
            let session_id_for_store = session_id.clone();
            let stop_reason_for_store = stop_reason.clone();
            update_store(state, |store| {
                if let Some(summary) = store.sessions.get_mut(&session_id_for_store) {
                    if !session_is_closed(&summary.summary.status) {
                        summary.summary.status = SESSION_STATUS_IDLE.to_string();
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
            let session_id = if let Some(session_id) = session_id {
                resolve_client_session_alias(state, device_id, &session_id)
                    .await
                    .or(Some(session_id))
            } else {
                None
            };
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
                        summary.summary.status = SESSION_STATUS_AWAITING_PERMISSION.to_string();
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

async fn resolve_client_session_alias(
    state: &AppState,
    device_id: &str,
    incoming_session_id: &str,
) -> Option<String> {
    let sessions = state.inner.sessions.lock().await;
    resolve_client_session_alias_in_map(&sessions, device_id, incoming_session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_server::RelaySession;
    use std::collections::HashMap;

    #[test]
    fn resolve_client_session_alias_maps_acp_id_for_same_device() {
        let mut sessions = HashMap::new();
        sessions.insert(
            "relay-session-1".to_string(),
            RelaySession {
                session_id: "relay-session-1".to_string(),
                device_id: "device-a".to_string(),
                browser_id: "browser-1".to_string(),
                owner_user_id: "alice".to_string(),
                client_session_id: Some("acp-1".to_string()),
            },
        );

        let resolved = resolve_client_session_alias_in_map(&sessions, "device-a", "acp-1");

        assert_eq!(resolved.as_deref(), Some("relay-session-1"));
    }

    #[test]
    fn resolve_client_session_alias_does_not_cross_devices() {
        let mut sessions = HashMap::new();
        sessions.insert(
            "relay-session-1".to_string(),
            RelaySession {
                session_id: "relay-session-1".to_string(),
                device_id: "device-a".to_string(),
                browser_id: "browser-1".to_string(),
                owner_user_id: "alice".to_string(),
                client_session_id: Some("acp-1".to_string()),
            },
        );

        let resolved = resolve_client_session_alias_in_map(&sessions, "device-b", "acp-1");

        assert!(resolved.is_none());
    }
}
