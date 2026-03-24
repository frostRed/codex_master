use super::connections::{
    broadcast_session_closed, broadcast_session_delta, broadcast_session_updated,
    broadcast_session_visibility_transition, ensure_device_connected, send_to_browser,
    send_to_client,
};
use super::{AppState, RelaySession, update_store};
use crate::protocol::{
    BrowserToServerMessage, RelaySessionSummaryMessage, ServerToBrowserMessage,
    ServerToClientMessage, SessionEventMessage,
};
use crate::relay_state::{PersistedRelaySession, unix_timestamp_secs};
use anyhow::{Result, anyhow};

use super::session_helpers::{
    SESSION_STATUS_CLOSED, SESSION_STATUS_REQUESTED, SESSION_STATUS_RUNNING,
    delete_session_from_store, get_browser_session, get_visible_session_summary,
    paginate_session_events, resolve_device_workspace, session_is_closed, session_owned_by_user_id,
    session_summary_visible_to_user,
};

pub(super) async fn handle_browser_message(
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
                            status: SESSION_STATUS_REQUESTED.to_string(),
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
                    summary.summary.status = SESSION_STATUS_RUNNING.to_string();
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
                    summary.summary.status = SESSION_STATUS_RUNNING.to_string();
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
                    summary.summary.status = SESSION_STATUS_CLOSED.to_string();
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
        BrowserToServerMessage::Ping => {}
    }

    Ok(())
}

pub(super) async fn send_device_list(state: &AppState, browser_id: &str) -> Result<()> {
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

pub(super) async fn send_session_list(
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
