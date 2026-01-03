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
    #[serde(default)]
    pub run_id: Option<String>,
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
    run_id: Option<String>,
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
            run_id: None,
            snapshot_tx,
            snapshot_rx,
            health_tx,
            health_rx,
            transitions_tx,
            transitions_rx,
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

        bus.publish_basic(
            ActivityState::Active,
            Some(sample_focus(1)),
            Some(transition.clone()),
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
        let recent = &snap.transitions_recent[0];
        assert_eq!(recent.by_signal.as_deref(), Some("signal-1"));
        assert_eq!(recent.to, ActivityState::Active);

        // Second publish bumps the sequence counter.
        bus.publish_basic(
            ActivityState::Passive,
            None,
            None,
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
}
