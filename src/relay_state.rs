use crate::protocol::{DeviceStatusMessage, RelaySessionSummaryMessage, SessionEventMessage};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedRelaySession {
    #[serde(flatten)]
    pub summary: RelaySessionSummaryMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RelaySnapshot {
    pub devices: HashMap<String, DeviceStatusMessage>,
    pub sessions: HashMap<String, PersistedRelaySession>,
    #[serde(default)]
    pub checkpointed_through_seq: u64,
    #[serde(default, alias = "events")]
    pub checkpointed_events: Vec<SessionEventMessage>,
}

#[derive(Debug, Clone, Default)]
pub struct RelayStore {
    pub devices: HashMap<String, DeviceStatusMessage>,
    pub sessions: HashMap<String, PersistedRelaySession>,
    pub events: Vec<SessionEventMessage>,
    pub checkpointed_event_count: usize,
    pub checkpointed_through_seq: u64,
}

impl RelayStore {
    pub fn next_session_counter(&self) -> u64 {
        self.sessions
            .keys()
            .filter_map(|session_id| session_id.rsplit('-').next())
            .filter_map(|suffix| suffix.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn next_event_seq(&self) -> u64 {
        self.events
            .iter()
            .map(SessionEventExt::max_seq)
            .max()
            .unwrap_or(0)
            + 1
    }

    pub fn snapshot(&self) -> RelaySnapshot {
        RelaySnapshot {
            devices: self.devices.clone(),
            sessions: self.sessions.clone(),
            checkpointed_through_seq: self.checkpointed_through_seq,
            checkpointed_events: self.events[..self.checkpointed_event_count].to_vec(),
        }
    }

    pub fn live_event_count(&self) -> usize {
        self.events
            .len()
            .saturating_sub(self.checkpointed_event_count)
    }

    pub fn compact_into_checkpoint(&mut self) {
        let compacted = compact_events(&self.events);
        self.checkpointed_through_seq = compacted
            .iter()
            .map(SessionEventExt::max_seq)
            .max()
            .unwrap_or(self.checkpointed_through_seq);
        self.checkpointed_event_count = compacted.len();
        self.events = compacted;
    }
}

pub trait SessionEventExt {
    fn max_seq(&self) -> u64;
}

impl SessionEventExt for SessionEventMessage {
    fn max_seq(&self) -> u64 {
        self.seq_end.unwrap_or(self.seq)
    }
}

fn compact_events(events: &[SessionEventMessage]) -> Vec<SessionEventMessage> {
    let mut compacted: Vec<SessionEventMessage> = Vec::new();

    for event in events {
        if let Some(previous) = compacted.last_mut()
            && previous.kind == "output_chunk"
            && event.kind == "output_chunk"
            && previous.session_id == event.session_id
        {
            previous.timestamp = event.timestamp;
            previous.seq_end = Some(previous.max_seq().max(event.max_seq()));
            previous.text = Some(format!(
                "{}{}",
                previous.text.clone().unwrap_or_default(),
                event.text.clone().unwrap_or_default()
            ));
            continue;
        }

        compacted.push(event.clone());
    }

    compacted
}

#[derive(Debug, Clone)]
pub struct RelayPersistence {
    snapshot_path: PathBuf,
    event_log_dir: PathBuf,
    legacy_event_log_path: PathBuf,
    compaction_threshold: usize,
    segment_event_limit: usize,
}

impl RelayPersistence {
    pub fn new(
        snapshot_path: PathBuf,
        compaction_threshold: usize,
        segment_event_limit: usize,
    ) -> Self {
        let event_log_dir = snapshot_path.with_extension("events.d");
        let legacy_event_log_path = snapshot_path.with_extension("events.jsonl");
        Self {
            snapshot_path,
            event_log_dir,
            legacy_event_log_path,
            compaction_threshold,
            segment_event_limit: segment_event_limit.max(1),
        }
    }

    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }

    pub fn event_log_dir(&self) -> &Path {
        &self.event_log_dir
    }

    pub fn compaction_threshold(&self) -> usize {
        self.compaction_threshold
    }

    pub fn segment_event_limit(&self) -> usize {
        self.segment_event_limit
    }

    pub fn load(&self) -> Result<RelayStore> {
        let snapshot = load_snapshot(&self.snapshot_path)?;
        let checkpointed_event_count = snapshot.checkpointed_events.len();
        let checkpointed_through_seq = snapshot.checkpointed_through_seq.max(
            snapshot
                .checkpointed_events
                .iter()
                .map(SessionEventExt::max_seq)
                .max()
                .unwrap_or(0),
        );
        let mut events = snapshot.checkpointed_events;

        let mut tail_events = if self.event_log_dir.exists() {
            load_segmented_event_log(&self.event_log_dir)?
        } else if self.legacy_event_log_path.exists() {
            load_legacy_event_log(&self.legacy_event_log_path)?
        } else {
            Vec::new()
        };
        tail_events.retain(|event| event.max_seq() > checkpointed_through_seq);
        events.append(&mut tail_events);

        let mut store = RelayStore {
            devices: snapshot.devices,
            sessions: snapshot.sessions,
            events,
            checkpointed_event_count,
            checkpointed_through_seq,
        };

        for device in store.devices.values_mut() {
            device.connected = false;
        }

        if !self.event_log_dir.exists() && self.legacy_event_log_path.exists() {
            let live_tail = store.events[store.checkpointed_event_count..].to_vec();
            rewrite_segmented_event_log(&self.event_log_dir, &live_tail, self.segment_event_limit)?;
        }

        Ok(store)
    }

    pub async fn save_snapshot(&self, store: &RelayStore) -> Result<()> {
        let snapshot_path = self.snapshot_path.clone();
        let snapshot = store.snapshot();
        tokio::task::spawn_blocking(move || save_snapshot_blocking(&snapshot_path, &snapshot))
            .await
            .context("等待 relay snapshot 持久化任务失败")?
    }

    pub async fn append_event(&self, event: SessionEventMessage) -> Result<()> {
        let event_log_dir = self.event_log_dir.clone();
        let segment_event_limit = self.segment_event_limit;
        tokio::task::spawn_blocking(move || {
            append_event_to_segment(&event_log_dir, segment_event_limit, &event)
        })
        .await
        .context("等待 relay event segment 持久化任务失败")?
    }

    pub async fn rewrite_event_log(&self, events: Vec<SessionEventMessage>) -> Result<()> {
        let event_log_dir = self.event_log_dir.clone();
        let segment_event_limit = self.segment_event_limit;
        tokio::task::spawn_blocking(move || {
            rewrite_segmented_event_log(&event_log_dir, &events, segment_event_limit)
        })
        .await
        .context("等待 relay segmented event log 重写任务失败")?
    }
}

fn load_snapshot(path: &Path) -> Result<RelaySnapshot> {
    if !path.exists() {
        return Ok(RelaySnapshot::default());
    }

    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取 relay snapshot 文件失败: {}", path.display()))?;
    let state = serde_json::from_str(&text)
        .with_context(|| format!("解析 relay snapshot 文件失败: {}", path.display()))?;
    Ok(state)
}

fn load_segmented_event_log(dir: &Path) -> Result<Vec<SessionEventMessage>> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("读取 relay event log 目录失败: {}", dir.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("遍历 relay event log 目录失败: {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut events = Vec::new();
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let mut segment_events = load_legacy_event_log(&path)?;
        events.append(&mut segment_events);
    }

    Ok(events)
}

fn load_legacy_event_log(path: &Path) -> Result<Vec<SessionEventMessage>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("打开 relay event log 失败: {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut events = Vec::new();

    for (index, line_result) in reader.lines().enumerate() {
        let line = line_result.with_context(|| {
            format!(
                "读取 relay event log 第 {} 行失败: {}",
                index + 1,
                path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }

        let event = serde_json::from_str::<SessionEventMessage>(&line).with_context(|| {
            format!(
                "解析 relay event log 第 {} 行失败: {}",
                index + 1,
                path.display()
            )
        })?;
        events.push(event);
    }

    Ok(events)
}

fn save_snapshot_blocking(path: &Path, snapshot: &RelaySnapshot) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("创建 relay snapshot 目录失败: {}", parent.display()))?;

    let tmp_path = path.with_extension("tmp");
    let text = serde_json::to_string_pretty(snapshot).context("序列化 relay snapshot 失败")?;
    std::fs::write(&tmp_path, text)
        .with_context(|| format!("写入 relay snapshot 临时文件失败: {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("替换 relay snapshot 文件失败: {}", path.display()))?;
    Ok(())
}

fn append_event_to_segment(
    dir: &Path,
    segment_event_limit: usize,
    event: &SessionEventMessage,
) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建 relay segmented event log 目录失败: {}", dir.display()))?;

    let segment_path = segment_path_for_seq(dir, segment_event_limit, event.seq);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&segment_path)
        .with_context(|| format!("打开 relay event segment 失败: {}", segment_path.display()))?;
    let line = serde_json::to_string(event).context("序列化 relay event 失败")?;
    writeln!(file, "{line}")
        .with_context(|| format!("写入 relay event segment 失败: {}", segment_path.display()))?;
    file.flush()
        .with_context(|| format!("刷新 relay event segment 失败: {}", segment_path.display()))?;
    Ok(())
}

fn rewrite_segmented_event_log(
    dir: &Path,
    events: &[SessionEventMessage],
    segment_event_limit: usize,
) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("创建 relay segmented event log 目录失败: {}", dir.display()))?;

    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("读取 relay segmented event log 目录失败: {}", dir.display()))?
    {
        let entry = entry.with_context(|| {
            format!("遍历 relay segmented event log 目录失败: {}", dir.display())
        })?;
        let path = entry.path();
        if path.is_file() {
            std::fs::remove_file(&path)
                .with_context(|| format!("删除旧 relay event segment 失败: {}", path.display()))?;
        }
    }

    let mut grouped: HashMap<PathBuf, Vec<&SessionEventMessage>> = HashMap::new();
    for event in events {
        grouped
            .entry(segment_path_for_seq(dir, segment_event_limit, event.seq))
            .or_default()
            .push(event);
    }

    let mut paths = grouped.keys().cloned().collect::<Vec<_>>();
    paths.sort();

    for path in paths {
        let mut file = std::fs::File::create(&path)
            .with_context(|| format!("创建 relay event segment 失败: {}", path.display()))?;
        if let Some(segment_events) = grouped.get(&path) {
            for event in segment_events {
                let line = serde_json::to_string(event).context("序列化 relay event 失败")?;
                writeln!(file, "{line}").with_context(|| {
                    format!("写入 relay event segment 失败: {}", path.display())
                })?;
            }
        }
        file.flush()
            .with_context(|| format!("刷新 relay event segment 失败: {}", path.display()))?;
    }

    Ok(())
}

fn segment_path_for_seq(dir: &Path, segment_event_limit: usize, seq: u64) -> PathBuf {
    let start_seq =
        (((seq.saturating_sub(1)) / segment_event_limit as u64) * segment_event_limit as u64) + 1;
    dir.join(format!("segment-{start_seq:020}.jsonl"))
}

pub fn unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_snapshot_deserializes_without_client_session_id() {
        let snapshot = serde_json::from_str::<RelaySnapshot>(
            r#"{
                "devices": {},
                "sessions": {
                    "relay-session-1": {
                        "session_id": "relay-session-1",
                        "device_id": "device-1",
                        "status": "idle",
                        "created_at": 1,
                        "updated_at": 2
                    }
                }
            }"#,
        )
        .expect("legacy snapshot should deserialize");

        let session = snapshot
            .sessions
            .get("relay-session-1")
            .expect("session should exist");
        assert_eq!(session.summary.session_id, "relay-session-1");
        assert_eq!(session.summary.device_id, "device-1");
        assert_eq!(session.client_session_id, None);
    }
}
