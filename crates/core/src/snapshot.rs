use crate::events::WindowFocusInfo;
use crate::records::ActivityState;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::watch;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub mono_ns: u64,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub from: ActivityState,
    pub to: ActivityState,
    pub at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub by_signal: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Counts {
    pub signals_seen: u64,
    pub hints_seen: u64,
    pub records_emitted: u64,
}

impl Snapshot {
    pub fn empty() -> Self {
        Self {
            seq: 0,
            mono_ns: monotonic_ns(),
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
        }
    }
}

pub struct SnapshotBus {
    seq: AtomicU64,
    snapshot_tx: watch::Sender<Arc<Snapshot>>,
    snapshot_rx: watch::Receiver<Arc<Snapshot>>,
    health_tx: watch::Sender<VecDeque<String>>,
    health_rx: watch::Receiver<VecDeque<String>>,
    transitions_tx: watch::Sender<VecDeque<Transition>>,
    transitions_rx: watch::Receiver<VecDeque<Transition>>,
}

impl SnapshotBus {
    pub fn new() -> Self {
        let (snapshot_tx, snapshot_rx) = watch::channel(Arc::new(Snapshot::empty()));
        let (health_tx, health_rx) = watch::channel(VecDeque::with_capacity(64));
        let (transitions_tx, transitions_rx) = watch::channel(VecDeque::with_capacity(64));
        Self {
            seq: AtomicU64::new(0),
            snapshot_tx,
            snapshot_rx,
            health_tx,
            health_rx,
            transitions_tx,
            transitions_rx,
        }
    }

    pub fn publish_basic(
        &self,
        state: ActivityState,
        focus: Option<WindowFocusInfo>,
        last_transition: Option<Transition>,
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
        let snap = Snapshot {
            seq,
            mono_ns: monotonic_ns(),
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
        };
        let _ = self.snapshot_tx.send(Arc::new(snap));
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
}

fn monotonic_ns() -> u64 {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    now.as_nanos() as u64
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StorageInfo {
    pub backlog_count: u64,
    pub last_flush_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConfigSummary {
    pub active_grace_secs: u64,
    pub idle_threshold_secs: u64,
    pub retention_minutes: u64,
    pub ephemeral_max_duration_secs: u64,
    pub ephemeral_min_distinct_ids: usize,
    pub ephemeral_app_max_duration_secs: u64,
    pub ephemeral_app_min_distinct_procs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotWindow {
    pub window_id: String,
    pub window_title: String,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub duration_seconds: u64,
    pub is_group: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotApp {
    pub app_name: String,
    pub pid: i32,
    pub process_start_time: u64,
    pub windows: Vec<SnapshotWindow>,
    pub total_duration_secs: u64,
    pub total_duration_pretty: String,
}
