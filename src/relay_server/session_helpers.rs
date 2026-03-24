use super::{AppState, RelaySession, SessionVisibilityChange};
use crate::protocol::{ServerToClientMessage, SessionEventMessage, WorkspaceSummaryMessage};
use crate::relay_state::{PersistedRelaySession, RelayStore, SessionEventExt, unix_timestamp_secs};
use anyhow::{Result, anyhow};
use std::collections::HashMap;

pub(super) const SESSION_STATUS_REQUESTED: &str = "requested";
pub(super) const SESSION_STATUS_READY: &str = "ready";
pub(super) const SESSION_STATUS_IDLE: &str = "idle";
pub(super) const SESSION_STATUS_RUNNING: &str = "running";
pub(super) const SESSION_STATUS_AWAITING_PERMISSION: &str = "awaiting_permission";
pub(super) const SESSION_STATUS_DEVICE_DISCONNECTED: &str = "device_disconnected";
pub(super) const SESSION_STATUS_RESUME_FAILED: &str = "resume_failed";
pub(super) const SESSION_STATUS_CLOSED: &str = "closed";

pub(super) fn session_is_closed(status: &str) -> bool {
    status == SESSION_STATUS_CLOSED
}

pub(super) fn session_should_auto_resume(status: &str) -> bool {
    !session_is_closed(status) && status != SESSION_STATUS_RESUME_FAILED
}

pub(super) fn delete_session_from_store(
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

pub(super) fn resolve_client_session_alias_in_map(
    sessions: &HashMap<String, RelaySession>,
    device_id: &str,
    incoming_session_id: &str,
) -> Option<String> {
    if sessions.contains_key(incoming_session_id) {
        return Some(incoming_session_id.to_string());
    }

    sessions.iter().find_map(|(relay_session_id, session)| {
        (session.device_id == device_id
            && session.client_session_id.as_deref() == Some(incoming_session_id))
        .then(|| relay_session_id.clone())
    })
}

pub(super) fn session_visibility_change(
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

pub(super) async fn current_session_snapshot(
    state: &AppState,
    session_id: &str,
) -> Result<PersistedRelaySession> {
    {
        let store = state.inner.store.lock().await;
        store.sessions.get(session_id).cloned()
    }
    .ok_or_else(|| anyhow!("未知 relay session: {session_id}"))
}

pub(super) fn paginate_session_events(
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

pub(super) fn pending_resume_commands(
    store: &RelayStore,
    device_id: &str,
) -> Vec<ServerToClientMessage> {
    let mut candidates = store
        .sessions
        .values()
        .filter(|session| session.summary.device_id == device_id)
        .filter(|session| session_should_auto_resume(&session.summary.status))
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

pub(super) fn session_summary_visible_to_user(
    session: &PersistedRelaySession,
    browser_user_id: &str,
) -> bool {
    session_owned_by_user_id(
        session.summary.owner_user_id.as_deref().unwrap_or_default(),
        browser_user_id,
    )
}

pub(super) fn session_owned_by_user_id(owner_user_id: &str, browser_user_id: &str) -> bool {
    owner_user_id.is_empty() || owner_user_id == browser_user_id
}

pub(super) async fn get_visible_session_summary(
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

pub(super) async fn resolve_device_workspace(
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

pub(super) async fn get_browser_session(
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

pub(super) async fn mark_device_disconnected(
    state: &AppState,
    device_id: &str,
) -> Result<Vec<String>> {
    let now = unix_timestamp_secs();
    let device_id_for_store = device_id.to_string();
    let seq = state.next_event_seq();
    let affected_session_ids = {
        let store = state.inner.store.lock().await;
        store
            .sessions
            .values()
            .filter(|session| session.summary.device_id == device_id)
            .map(|session| session.summary.session_id.clone())
            .collect::<Vec<_>>()
    };

    super::update_store(state, |store| {
        for session in store.sessions.values_mut() {
            if session.summary.device_id == device_id_for_store {
                session.summary.status = SESSION_STATUS_DEVICE_DISCONNECTED.to_string();
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
    .await?;

    Ok(affected_session_ids)
}
