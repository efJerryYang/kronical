use crate::daemon::events::WindowFocusInfo;
use crate::daemon::records::ActivityState;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub mono_ns: u64,
    pub activity_state: ActivityState,
    pub focus: Option<WindowFocusInfo>,
    pub last_transition: Option<Transition>,
    pub counts: Counts,
    pub cadence_ms: u32,
    pub cadence_reason: String,
    pub next_timeout: Option<chrono::DateTime<chrono::Utc>>,
    pub storage: StorageInfo,
    pub config: ConfigSummary,
    pub replay: ReplayInfo,
    pub health: Vec<String>,
    pub aggregated_apps: Vec<SnapshotApp>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub from: ActivityState,
    pub to: ActivityState,
    pub at: chrono::DateTime<chrono::Utc>,
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
            counts: Counts::default(),
            cadence_ms: 0,
            cadence_reason: String::new(),
            next_timeout: None,
            storage: StorageInfo {
                backlog_count: 0,
                last_flush_at: None,
            },
            config: ConfigSummary::default(),
            replay: ReplayInfo {
                mode: "live".into(),
                position: None,
            },
            health: Vec::new(),
            aggregated_apps: Vec::new(),
        }
    }
}

pub static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);
pub static SNAPSHOT: Lazy<RwLock<Arc<Snapshot>>> =
    Lazy::new(|| RwLock::new(Arc::new(Snapshot::empty())));
static HEALTH_BUF: Lazy<RwLock<VecDeque<String>>> =
    Lazy::new(|| RwLock::new(VecDeque::with_capacity(64)));

fn monotonic_ns() -> u64 {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    now.as_nanos() as u64
}

pub fn publish_basic(
    state: ActivityState,
    focus: Option<WindowFocusInfo>,
    last_transition: Option<Transition>,
    counts: Counts,
    cadence_ms: u32,
    cadence_reason: String,
    next_timeout: Option<chrono::DateTime<chrono::Utc>>,
    storage: StorageInfo,
    config: ConfigSummary,
    replay: ReplayInfo,
    health: Vec<String>,
    aggregated_apps: Vec<SnapshotApp>,
) {
    let seq = SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let snap = Snapshot {
        seq,
        mono_ns: monotonic_ns(),
        activity_state: state,
        focus,
        last_transition,
        counts,
        cadence_ms,
        cadence_reason,
        next_timeout,
        storage,
        config,
        replay,
        health,
        aggregated_apps,
    };
    if let Ok(mut guard) = SNAPSHOT.write() {
        *guard = Arc::new(snap);
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayInfo {
    pub mode: String, // "live" | "replay"
    pub position: Option<u64>,
}

pub fn get_current() -> Arc<Snapshot> {
    if let Ok(guard) = SNAPSHOT.read() {
        guard.clone()
    } else {
        Arc::new(Snapshot::empty())
    }
}

pub fn push_health(msg: impl Into<String>) {
    let s = msg.into();
    if let Ok(mut q) = HEALTH_BUF.write() {
        if q.len() >= 64 {
            q.pop_front();
        }
        q.push_back(s);
    }
}

pub fn current_health() -> Vec<String> {
    if let Ok(q) = HEALTH_BUF.read() {
        q.iter().cloned().collect()
    } else {
        Vec::new()
    }
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
