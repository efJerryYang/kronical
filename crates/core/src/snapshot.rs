use crate::events::WindowFocusInfo;
use crate::records::ActivityState;
use once_cell::sync::OnceCell;
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
            health: Vec::new(),
            aggregated_apps: Vec::new(),
        }
    }
}

pub static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);
// Channel-first health buffer: maintain the last 64 messages via a watch channel.
// Avoids RwLock by cloning/modifying the current buffer and re-sending.
static HEALTH_WATCH: OnceCell<(
    watch::Sender<VecDeque<String>>,
    watch::Receiver<VecDeque<String>>,
)> = OnceCell::new();

fn init_health_watch() -> &'static (
    watch::Sender<VecDeque<String>>,
    watch::Receiver<VecDeque<String>>,
) {
    HEALTH_WATCH.get_or_init(|| {
        let initial: VecDeque<String> = VecDeque::with_capacity(64);
        watch::channel(initial)
    })
}

// Optional live snapshot watch channel
static SNAP_WATCH: OnceCell<(watch::Sender<Arc<Snapshot>>, watch::Receiver<Arc<Snapshot>>)> =
    OnceCell::new();

fn init_snapshot_watch() -> &'static (watch::Sender<Arc<Snapshot>>, watch::Receiver<Arc<Snapshot>>)
{
    SNAP_WATCH.get_or_init(|| {
        // Initialize with an empty snapshot value and update over time
        let initial = Arc::new(Snapshot::empty());
        watch::channel(initial)
    })
}

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
        health,
        aggregated_apps,
    };
    let arc = Arc::new(snap);
    let (tx, _rx) = init_snapshot_watch();
    let _ = tx.send(arc);
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

pub fn get_current() -> Arc<Snapshot> {
    let (_tx, rx) = init_snapshot_watch();
    rx.borrow().clone()
}

pub fn watch_snapshot() -> watch::Receiver<Arc<Snapshot>> {
    let (_tx, rx) = init_snapshot_watch();
    rx.clone()
}

pub fn push_health(msg: impl Into<String>) {
    let s = msg.into();
    let (tx, rx) = init_health_watch();
    let mut buf = rx.borrow().clone();
    if buf.len() >= 64 {
        let _ = buf.pop_front();
    }
    buf.push_back(s);
    let _ = tx.send(buf);
}

pub fn current_health() -> Vec<String> {
    let (_tx, rx) = init_health_watch();
    rx.borrow().iter().cloned().collect()
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
