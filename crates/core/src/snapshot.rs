use crate::events::WindowFocusInfo;
use crate::records::{ActivityRecord, ActivityState};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{broadcast, watch};

const DELTA_HISTORY_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub seq: u64,
    pub mono_ns: u64,
    #[serde(default)]
    pub run_id: Option<String>,
    pub activity_state: ActivityState,
    pub focus: Option<WindowFocusInfo>,
    pub last_transition: Option<Transition>,
    #[serde(default)]
    pub transitions_recent: Vec<Transition>,
    pub counts: Counts,
    pub cadence_ms: u32,
    pub cadence_reason: String,
    pub next_timeout: Option<chrono::DateTime<chrono::Utc>>,
    pub storage: StorageInfo,
    pub config: ConfigSummary,
    pub health: Vec<String>,
    pub aggregated_apps: Vec<SnapshotApp>,
    #[serde(default)]
    pub title_revisions_recent: Vec<TitleRevisionRef>,
    #[serde(default)]
    pub records: Vec<ActivityRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TitleRevisionRef {
    pub event_id: u64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub window_id: u32,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Transition {
    pub from: ActivityState,
    pub to: ActivityState,
    pub at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub by_signal: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub signals_seen: u64,
    pub hints_seen: u64,
    pub records_emitted: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotDelta {
    pub seq: u64,
    pub mono_ns: u64,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_state: Option<ActivityState>,
    #[serde(default)]
    pub focus_cleared: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<WindowFocusInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition: Option<Transition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitions_added: Vec<Transition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_reason: Option<String>,
    #[serde(default)]
    pub next_timeout_cleared: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_timeout: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health_appended: Vec<String>,
    #[serde(default)]
    pub aggregated_apps_changed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aggregated_apps: Vec<SnapshotApp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_revisions_added: Vec<TitleRevisionRef>,
}

impl Snapshot {
    pub fn empty() -> Self {
        Self {
            seq: 0,
            mono_ns: monotonic_ns(),
            run_id: None,
            activity_state: ActivityState::Inactive,
            focus: None,
            last_transition: None,
            transitions_recent: Vec::new(),
            counts: Counts::default(),
            cadence_ms: 0,
            cadence_reason: String::new(),
            next_timeout: None,
            storage: StorageInfo {
                backlog_count: 0,
                last_flush_at: None,
            },
            config: ConfigSummary::default(),
            health: Vec::new(),
            aggregated_apps: Vec::new(),
            title_revisions_recent: Vec::new(),
            records: Vec::new(),
        }
    }
}

pub struct SnapshotBus {
    seq: AtomicU64,
    run_id: Option<String>,
    snapshot_tx: watch::Sender<Arc<Snapshot>>,
    snapshot_rx: watch::Receiver<Arc<Snapshot>>,
    health_tx: watch::Sender<VecDeque<String>>,
    health_rx: watch::Receiver<VecDeque<String>>,
    transitions_tx: watch::Sender<VecDeque<Transition>>,
    transitions_rx: watch::Receiver<VecDeque<Transition>>,
    title_revisions_tx: watch::Sender<VecDeque<TitleRevisionRef>>,
    title_revisions_rx: watch::Receiver<VecDeque<TitleRevisionRef>>,
    delta_history_tx: watch::Sender<VecDeque<SnapshotDelta>>,
    delta_history_rx: watch::Receiver<VecDeque<SnapshotDelta>>,
    delta_tx: broadcast::Sender<SnapshotDelta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaReplayError {
    AfterSeqTooOld,
}

impl SnapshotBus {
    pub fn new() -> Self {
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(Snapshot::empty()));
        let (health_tx, health_rx) = watch::channel(VecDeque::with_capacity(64));
        let (transitions_tx, transitions_rx) = watch::channel(VecDeque::with_capacity(64));
        let (title_revisions_tx, title_revisions_rx) = watch::channel(VecDeque::with_capacity(128));
        let (delta_history_tx, delta_history_rx) =
            watch::channel(VecDeque::with_capacity(DELTA_HISTORY_CAPACITY));
        let (delta_tx, _) = broadcast::channel(DELTA_HISTORY_CAPACITY);
        Self {
            seq: AtomicU64::new(0),
            run_id: None,
            snapshot_tx,
            snapshot_rx,
            health_tx,
            health_rx,
            transitions_tx,
            transitions_rx,
            title_revisions_tx,
            title_revisions_rx,
            delta_history_tx,
            delta_history_rx,
            delta_tx,
        }
    }

    pub fn new_with_run_id(run_id: impl Into<String>) -> Self {
        let mut bus = Self::new();
        bus.run_id = Some(run_id.into());
        bus
    }

    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    pub fn publish_basic(
        &self,
        state: ActivityState,
        focus: Option<WindowFocusInfo>,
        last_transition: Option<Transition>,
        records: Vec<ActivityRecord>,
        counts: Counts,
        cadence_ms: u32,
        cadence_reason: String,
        next_timeout: Option<chrono::DateTime<chrono::Utc>>,
        storage: StorageInfo,
        config: ConfigSummary,
        health: Vec<String>,
        aggregated_apps: Vec<SnapshotApp>,
    ) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let transitions_recent = self.recent_transitions(5);
        let title_revisions_recent = self.recent_title_revisions(64);
        let previous = self.snapshot();
        let snap = Snapshot {
            seq,
            mono_ns: monotonic_ns(),
            run_id: self.run_id.clone(),
            activity_state: state,
            focus,
            last_transition,
            transitions_recent,
            counts,
            cadence_ms,
            cadence_reason,
            next_timeout,
            storage,
            config,
            health,
            aggregated_apps,
            title_revisions_recent,
            records,
        };
        let delta = build_delta(previous.as_ref(), &snap);
        let _ = self.snapshot_tx.send(Arc::new(snap));
        if let Some(delta) = delta {
            self.record_delta(delta);
        }
    }

    pub fn push_transition(&self, t: Transition) {
        let mut buf = {
            let guard = self.transitions_rx.borrow();
            guard.clone()
        };
        if buf.len() >= 64 {
            let _ = buf.pop_front();
        }
        buf.push_back(t);
        let _ = self.transitions_tx.send(buf);
    }

    pub fn recent_transitions(&self, limit: usize) -> Vec<Transition> {
        let buf = self.transitions_rx.borrow();
        let len = buf.len();
        let n = limit.min(len);
        buf.iter().rev().take(n).cloned().collect()
    }

    pub fn push_title_revision(&self, revision: TitleRevisionRef) {
        let mut buf = {
            let guard = self.title_revisions_rx.borrow();
            guard.clone()
        };
        if buf.len() >= 128 {
            let _ = buf.pop_front();
        }
        buf.push_back(revision);
        let _ = self.title_revisions_tx.send(buf);
    }

    pub fn recent_title_revisions(&self, limit: usize) -> Vec<TitleRevisionRef> {
        let buf = self.title_revisions_rx.borrow();
        let len = buf.len();
        let n = limit.min(len);
        buf.iter().rev().take(n).cloned().collect()
    }

    pub fn push_health(&self, msg: impl Into<String>) {
        let mut buf = {
            let guard = self.health_rx.borrow();
            guard.clone()
        };
        if buf.len() >= 64 {
            let _ = buf.pop_front();
        }
        buf.push_back(msg.into());
        let _ = self.health_tx.send(buf);
    }

    pub fn current_health(&self) -> Vec<String> {
        self.health_rx.borrow().iter().cloned().collect()
    }

    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot_rx.borrow().clone()
    }

    pub fn watch_snapshot(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshot_rx.clone()
    }

    pub fn watch_deltas_after(
        &self,
        after_seq: u64,
    ) -> Result<(Vec<SnapshotDelta>, broadcast::Receiver<SnapshotDelta>), DeltaReplayError> {
        let rx = self.delta_tx.subscribe();
        let history = self.delta_history_rx.borrow().clone();
        if let Some(oldest) = history.front().map(|delta| delta.seq) {
            if after_seq < oldest.saturating_sub(1) {
                return Err(DeltaReplayError::AfterSeqTooOld);
            }
        }
        let replay = history
            .iter()
            .filter(|delta| delta.seq > after_seq)
            .cloned()
            .collect();
        Ok((replay, rx))
    }

    fn record_delta(&self, delta: SnapshotDelta) {
        let mut buf = {
            let guard = self.delta_history_rx.borrow();
            guard.clone()
        };
        if buf.len() >= DELTA_HISTORY_CAPACITY {
            let _ = buf.pop_front();
        }
        buf.push_back(delta.clone());
        let _ = self.delta_history_tx.send(buf);
        let _ = self.delta_tx.send(delta);
    }
}

fn monotonic_ns() -> u64 {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    now.as_nanos() as u64
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    pub backlog_count: u64,
    pub last_flush_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSummary {
    pub active_grace_secs: u64,
    pub idle_threshold_secs: u64,
    pub retention_minutes: u64,
    pub ephemeral_max_duration_secs: u64,
    pub ephemeral_min_distinct_ids: usize,
    pub ephemeral_app_max_duration_secs: u64,
    pub ephemeral_app_min_distinct_procs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotWindow {
    pub window_id: String,
    pub window_title: String,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub duration_seconds: u64,
    pub is_group: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotApp {
    pub app_name: String,
    pub pid: i32,
    pub process_start_time: u64,
    pub windows: Vec<SnapshotWindow>,
    pub total_duration_secs: u64,
    pub total_duration_pretty: String,
}

impl Snapshot {
    pub fn apply_delta(&mut self, delta: &SnapshotDelta) {
        self.seq = delta.seq;
        self.mono_ns = delta.mono_ns;
        self.run_id = delta.run_id.clone();
        if let Some(state) = delta.activity_state {
            self.activity_state = state;
        }
        if delta.focus_cleared {
            self.focus = None;
        }
        if let Some(focus) = &delta.focus {
            self.focus = Some(focus.clone());
        }
        if let Some(transition) = &delta.last_transition {
            self.last_transition = Some(transition.clone());
        }
        if !delta.transitions_added.is_empty() {
            self.transitions_recent
                .splice(0..0, delta.transitions_added.iter().rev().cloned());
            if self.transitions_recent.len() > 64 {
                self.transitions_recent.truncate(64);
            }
        }
        if let Some(counts) = &delta.counts {
            self.counts = counts.clone();
        }
        if let Some(cadence_ms) = delta.cadence_ms {
            self.cadence_ms = cadence_ms;
        }
        if let Some(cadence_reason) = &delta.cadence_reason {
            self.cadence_reason = cadence_reason.clone();
        }
        if delta.next_timeout_cleared {
            self.next_timeout = None;
        }
        if let Some(next_timeout) = delta.next_timeout {
            self.next_timeout = Some(next_timeout);
        }
        if let Some(storage) = &delta.storage {
            self.storage = storage.clone();
        }
        if let Some(config) = &delta.config {
            self.config = config.clone();
        }
        if !delta.health_appended.is_empty() {
            self.health.extend(delta.health_appended.iter().cloned());
        }
        if delta.aggregated_apps_changed {
            self.aggregated_apps = delta.aggregated_apps.clone();
        }
        if !delta.title_revisions_added.is_empty() {
            self.title_revisions_recent
                .splice(0..0, delta.title_revisions_added.iter().rev().cloned());
            if self.title_revisions_recent.len() > 64 {
                self.title_revisions_recent.truncate(64);
            }
        }
    }
}

fn build_delta(previous: &Snapshot, current: &Snapshot) -> Option<SnapshotDelta> {
    let mut delta = SnapshotDelta {
        seq: current.seq,
        mono_ns: current.mono_ns,
        run_id: current.run_id.clone(),
        ..SnapshotDelta::default()
    };

    if previous.activity_state != current.activity_state {
        delta.activity_state = Some(current.activity_state);
    }
    if previous.focus != current.focus {
        match &current.focus {
            Some(focus) => delta.focus = Some(focus.clone()),
            None => delta.focus_cleared = true,
        }
    }
    if previous.last_transition != current.last_transition {
        delta.last_transition = current.last_transition.clone();
    }
    delta.transitions_added =
        prepended_newest_first(&previous.transitions_recent, &current.transitions_recent);
    if previous.counts != current.counts {
        delta.counts = Some(current.counts.clone());
    }
    if previous.cadence_ms != current.cadence_ms
        || previous.cadence_reason != current.cadence_reason
    {
        delta.cadence_ms = Some(current.cadence_ms);
        delta.cadence_reason = Some(current.cadence_reason.clone());
    }
    if previous.next_timeout != current.next_timeout {
        match current.next_timeout {
            Some(next_timeout) => delta.next_timeout = Some(next_timeout),
            None => delta.next_timeout_cleared = true,
        }
    }
    if previous.storage != current.storage {
        delta.storage = Some(current.storage.clone());
    }
    if previous.config != current.config {
        delta.config = Some(current.config.clone());
    }
    delta.health_appended = appended_suffix(&previous.health, &current.health);
    if previous.aggregated_apps != current.aggregated_apps {
        delta.aggregated_apps_changed = true;
        delta.aggregated_apps = current.aggregated_apps.clone();
    }
    delta.title_revisions_added = prepended_newest_first(
        &previous.title_revisions_recent,
        &current.title_revisions_recent,
    );

    if delta
        == (SnapshotDelta {
            seq: current.seq,
            mono_ns: current.mono_ns,
            run_id: current.run_id.clone(),
            ..SnapshotDelta::default()
        })
    {
        None
    } else {
        Some(delta)
    }
}

fn appended_suffix<T: Clone + PartialEq>(previous: &[T], current: &[T]) -> Vec<T> {
    if current.starts_with(previous) {
        current[previous.len()..].to_vec()
    } else {
        current.to_vec()
    }
}

fn prepended_newest_first<T: Clone + PartialEq>(previous: &[T], current: &[T]) -> Vec<T> {
    if current.len() > previous.len() && current.ends_with(previous) {
        current[..current.len() - previous.len()]
            .iter()
            .rev()
            .cloned()
            .collect()
    } else if current != previous {
        current.iter().rev().cloned().collect()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{MousePosition, WindowFocusInfo};
    use chrono::{TimeZone, Utc};
    use std::sync::Arc;

    fn sample_focus(window_id: u32) -> WindowFocusInfo {
        WindowFocusInfo {
            pid: 42,
            process_start_time: 1337,
            app_name: Arc::new("Terminal".to_string()),
            window_title: Arc::new(format!("tab-{window_id}")),
            window_id,
            window_instance_start: Utc
                .with_ymd_and_hms(2024, 4, 22, 10, 0, window_id as u32 % 60)
                .unwrap(),
            window_position: Some(MousePosition { x: 10, y: 20 }),
            window_size: Some((1440, 900)),
        }
    }

    fn sample_transition(seq: usize) -> Transition {
        Transition {
            from: ActivityState::Inactive,
            to: ActivityState::Active,
            at: Utc
                .with_ymd_and_hms(2024, 4, 22, 10, 1, seq as u32 % 60)
                .unwrap(),
            by_signal: Some(format!("signal-{seq}")),
            run_id: None,
        }
    }

    fn sample_title_revision(seq: usize) -> TitleRevisionRef {
        TitleRevisionRef {
            event_id: 1_000 + seq as u64,
            at: Utc
                .with_ymd_and_hms(2024, 4, 22, 10, 4, seq as u32 % 60)
                .unwrap(),
            window_id: 50 + seq as u32,
            title: format!("tab-{seq}"),
        }
    }

    #[test]
    fn publish_basic_updates_snapshot_and_sequence() {
        let bus = SnapshotBus::new();
        let counts = Counts {
            signals_seen: 5,
            hints_seen: 3,
            records_emitted: 2,
        };
        let storage = StorageInfo {
            backlog_count: 7,
            last_flush_at: Some(Utc.with_ymd_and_hms(2024, 4, 22, 10, 2, 0).unwrap()),
        };
        let config = ConfigSummary {
            active_grace_secs: 15,
            idle_threshold_secs: 120,
            retention_minutes: 60,
            ephemeral_max_duration_secs: 45,
            ephemeral_min_distinct_ids: 2,
            ephemeral_app_max_duration_secs: 30,
            ephemeral_app_min_distinct_procs: 1,
        };

        let transition = sample_transition(1);
        bus.push_transition(transition.clone());
        let title_revision = sample_title_revision(1);
        bus.push_title_revision(title_revision.clone());

        bus.publish_basic(
            ActivityState::Active,
            Some(sample_focus(1)),
            Some(transition.clone()),
            Vec::new(),
            counts,
            1_000,
            "timer".to_string(),
            Some(Utc.with_ymd_and_hms(2024, 4, 22, 10, 3, 0).unwrap()),
            storage,
            config,
            vec!["healthy".to_string()],
            vec![],
        );

        let snap = bus.snapshot();
        assert_eq!(snap.seq, 1);
        assert_eq!(snap.activity_state, ActivityState::Active);
        assert_eq!(snap.counts.signals_seen, 5);
        assert_eq!(snap.cadence_ms, 1_000);
        assert_eq!(snap.cadence_reason, "timer");
        assert_eq!(snap.health, vec!["healthy".to_string()]);
        assert_eq!(snap.transitions_recent.len(), 1);
        assert_eq!(snap.title_revisions_recent.len(), 1);
        let recent = &snap.transitions_recent[0];
        assert_eq!(recent.by_signal.as_deref(), Some("signal-1"));
        assert_eq!(recent.to, ActivityState::Active);
        let recent_title = &snap.title_revisions_recent[0];
        assert_eq!(recent_title.event_id, title_revision.event_id);
        assert_eq!(recent_title.title, title_revision.title);

        // Second publish bumps the sequence counter.
        bus.publish_basic(
            ActivityState::Passive,
            None,
            None,
            Vec::new(),
            Counts::default(),
            500,
            "idle".to_string(),
            None,
            StorageInfo::default(),
            ConfigSummary::default(),
            vec![],
            vec![],
        );
        let snap_two = bus.snapshot();
        assert_eq!(snap_two.seq, 2);
        assert_eq!(snap_two.activity_state, ActivityState::Passive);
    }

    #[test]
    fn transition_and_health_buffers_trim_to_capacity() {
        let bus = SnapshotBus::new();

        for i in 0..70 {
            bus.push_transition(sample_transition(i));
            bus.push_health(format!("health-{i}"));
        }

        let recent = bus.recent_transitions(10);
        assert_eq!(recent.len(), 10);
        assert_eq!(recent[0].by_signal.as_deref(), Some("signal-69"));
        assert_eq!(
            recent.last().unwrap().by_signal.as_deref(),
            Some("signal-60")
        );

        let health = bus.current_health();
        assert_eq!(health.len(), 64);
        assert_eq!(health.first().unwrap(), "health-6");
        assert_eq!(health.last().unwrap(), "health-69");
    }

    #[test]
    fn title_revision_buffer_trims_to_capacity() {
        let bus = SnapshotBus::new();

        for i in 0..140 {
            bus.push_title_revision(sample_title_revision(i));
        }

        let recent = bus.recent_title_revisions(10);
        assert_eq!(recent.len(), 10);
        assert_eq!(recent[0].event_id, 1_139);
        assert_eq!(recent.last().unwrap().event_id, 1_130);
    }
}
