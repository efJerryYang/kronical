use crate::daemon::events::WindowFocusInfo;
use crate::daemon::records::{
    ActivityRecord, ActivityState, AggregatedActivity, aggregate_activities_since,
};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::daemon::snapshot;
use crate::storage::{StorageCommand, StorageMetrics};
use anyhow::Result;
use chrono::Utc;
use crossbeam_channel::{Receiver, Sender};
use log::info;
use std::collections::{HashMap as StdHashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::types::{SnapshotMessage, SnapshotUpdate};

pub const POLL_SUSPEND_MS: u64 = 0;

pub struct SnapshotStageConfig {
    pub receiver: Receiver<SnapshotMessage>,
    pub snapshot_bus: Arc<snapshot::SnapshotBus>,
    pub storage_tx: Sender<StorageCommand>,
    pub poll_handle: Arc<AtomicU64>,
    pub cfg_summary: snapshot::ConfigSummary,
    pub retention_minutes: u64,
    pub initial_records: Vec<ActivityRecord>,
    pub initial_transitions: Vec<snapshot::Transition>,
    pub ephemeral_max_duration_secs: u64,
    pub ephemeral_min_distinct_ids: usize,
    pub max_windows_per_app: usize,
    pub ephemeral_app_max_duration_secs: u64,
    pub ephemeral_app_min_distinct_procs: usize,
    pub storage_metrics_rx: tokio::sync::watch::Receiver<StorageMetrics>,
}

pub fn spawn_snapshot_stage(
    threads: &ThreadRegistry,
    config: SnapshotStageConfig,
) -> Result<ThreadHandle> {
    let SnapshotStageConfig {
        receiver,
        snapshot_bus,
        storage_tx,
        poll_handle,
        cfg_summary,
        retention_minutes,
        mut initial_records,
        mut initial_transitions,
        ephemeral_max_duration_secs,
        ephemeral_min_distinct_ids,
        max_windows_per_app,
        ephemeral_app_max_duration_secs,
        ephemeral_app_min_distinct_procs,
        storage_metrics_rx,
    } = config;

    let threads = threads.clone();
    threads.spawn("pipeline-snapshot", move || {
        info!("Pipeline snapshot stage started");
        let mut counts = snapshot::Counts {
            signals_seen: 0,
            hints_seen: 0,
            records_emitted: 0,
        };
        let mut recent_records: VecDeque<ActivityRecord> = initial_records.drain(..).collect();
        let mut current_state = ActivityState::Inactive;
        let mut current_focus: Option<WindowFocusInfo> = None;
        let mut last_transition: Option<snapshot::Transition> = None;
        let mut state_hist: VecDeque<char> = VecDeque::new();
        let retention = chrono::Duration::minutes(retention_minutes as i64);

        let mut shutdown_requested = false;
        let mut hints_complete = false;
        let mut storage_shutdown_sent = false;

        if !initial_transitions.is_empty() {
            for mut transition in initial_transitions.drain(..) {
                attach_run_id(snapshot_bus.as_ref(), &mut transition);
                current_state = transition.to;
                last_transition = Some(transition.clone());
                snapshot_bus.push_transition(transition);
            }
        }

        publish_snapshot(
            snapshot_bus.as_ref(),
            &mut recent_records,
            &storage_metrics_rx,
            &mut counts,
            &current_state,
            &current_focus,
            &last_transition,
            &cfg_summary,
            &poll_handle,
            retention,
            ephemeral_max_duration_secs,
            ephemeral_min_distinct_ids,
            max_windows_per_app,
            ephemeral_app_max_duration_secs,
            ephemeral_app_min_distinct_procs,
        );

        while let Ok(msg) = receiver.recv() {
            match msg {
                SnapshotMessage::Update(update) => {
                    apply_update(
                        update,
                        &snapshot_bus,
                        &storage_tx,
                        &mut counts,
                        &mut current_state,
                        &mut current_focus,
                        &mut last_transition,
                    );
                }
                SnapshotMessage::Record(record) => {
                    update_records(&mut recent_records, &mut counts, &mut state_hist, record);
                }
                SnapshotMessage::Publish => {
                    publish_snapshot(
                        snapshot_bus.as_ref(),
                        &mut recent_records,
                        &storage_metrics_rx,
                        &mut counts,
                        &current_state,
                        &current_focus,
                        &last_transition,
                        &cfg_summary,
                        &poll_handle,
                        retention,
                        ephemeral_max_duration_secs,
                        ephemeral_min_distinct_ids,
                        max_windows_per_app,
                        ephemeral_app_max_duration_secs,
                        ephemeral_app_min_distinct_procs,
                    );
                }
                SnapshotMessage::HintsComplete => {
                    hints_complete = true;
                    if maybe_finalize(
                        shutdown_requested,
                        hints_complete,
                        &mut storage_shutdown_sent,
                        snapshot_bus.as_ref(),
                        &mut recent_records,
                        &storage_metrics_rx,
                        &mut counts,
                        &current_state,
                        &current_focus,
                        &last_transition,
                        &cfg_summary,
                        &poll_handle,
                        &storage_tx,
                        retention,
                        ephemeral_max_duration_secs,
                        ephemeral_min_distinct_ids,
                        max_windows_per_app,
                        ephemeral_app_max_duration_secs,
                        ephemeral_app_min_distinct_procs,
                    ) {
                        break;
                    }
                }
                SnapshotMessage::Shutdown => {
                    shutdown_requested = true;
                    if maybe_finalize(
                        shutdown_requested,
                        hints_complete,
                        &mut storage_shutdown_sent,
                        snapshot_bus.as_ref(),
                        &mut recent_records,
                        &storage_metrics_rx,
                        &mut counts,
                        &current_state,
                        &current_focus,
                        &last_transition,
                        &cfg_summary,
                        &poll_handle,
                        &storage_tx,
                        retention,
                        ephemeral_max_duration_secs,
                        ephemeral_min_distinct_ids,
                        max_windows_per_app,
                        ephemeral_app_max_duration_secs,
                        ephemeral_app_min_distinct_procs,
                    ) {
                        break;
                    }
                }
            }
        }

        if !storage_shutdown_sent {
            maybe_finalize(
                true,
                true,
                &mut storage_shutdown_sent,
                snapshot_bus.as_ref(),
                &mut recent_records,
                &storage_metrics_rx,
                &mut counts,
                &current_state,
                &current_focus,
                &last_transition,
                &cfg_summary,
                &poll_handle,
                &storage_tx,
                retention,
                ephemeral_max_duration_secs,
                ephemeral_min_distinct_ids,
                max_windows_per_app,
                ephemeral_app_max_duration_secs,
                ephemeral_app_min_distinct_procs,
            );
        }

        info!("Pipeline snapshot stage exiting");
    })
}

fn apply_update(
    update: SnapshotUpdate,
    snapshot_bus: &Arc<snapshot::SnapshotBus>,
    storage_tx: &Sender<StorageCommand>,
    counts: &mut snapshot::Counts,
    current_state: &mut ActivityState,
    current_focus: &mut Option<WindowFocusInfo>,
    last_transition: &mut Option<snapshot::Transition>,
) {
    counts.hints_seen = counts.hints_seen.saturating_add(update.hints_delta);
    counts.signals_seen = counts.signals_seen.saturating_add(update.signals_delta);

    if let Some(focus) = update.focus {
        *current_focus = Some(focus);
    }
    if let Some(state) = update.state {
        *current_state = state;
    }
    if let Some((window_id, title)) = update.focus_title {
        if let Some(focus) = current_focus.as_mut() {
            if focus.window_id == window_id {
                focus.window_title = Arc::new(title);
            }
        }
    }
    if let Some(mut transition) = update.transition {
        attach_run_id(snapshot_bus.as_ref(), &mut transition);
        snapshot_bus.push_transition(transition.clone());
        *last_transition = Some(transition.clone());
        let _ = storage_tx.send(StorageCommand::Transition(transition));
    }
}

fn attach_run_id(snapshot_bus: &snapshot::SnapshotBus, transition: &mut snapshot::Transition) {
    if transition.run_id.is_none() {
        transition.run_id = snapshot_bus.run_id().map(|id| id.to_string());
    }
}

fn update_records(
    recent_records: &mut VecDeque<ActivityRecord>,
    counts: &mut snapshot::Counts,
    state_hist: &mut VecDeque<char>,
    record: ActivityRecord,
) {
    let ch = match record.state {
        ActivityState::Active => 'A',
        ActivityState::Passive => 'P',
        ActivityState::Inactive => 'I',
        ActivityState::Locked => 'L',
    };
    if state_hist.back().copied() != Some(ch) {
        state_hist.push_back(ch);
        if state_hist.len() > 10 {
            state_hist.pop_front();
        }
    }
    recent_records.push_back(record);
    counts.records_emitted = counts.records_emitted.saturating_add(1);
}

fn maybe_finalize(
    shutdown_requested: bool,
    hints_complete: bool,
    storage_shutdown_sent: &mut bool,
    snapshot_bus: &snapshot::SnapshotBus,
    recent_records: &mut VecDeque<ActivityRecord>,
    storage_metrics_rx: &tokio::sync::watch::Receiver<StorageMetrics>,
    counts: &mut snapshot::Counts,
    current_state: &ActivityState,
    current_focus: &Option<WindowFocusInfo>,
    last_transition: &Option<snapshot::Transition>,
    cfg_summary: &snapshot::ConfigSummary,
    poll_handle: &Arc<AtomicU64>,
    storage_tx: &Sender<StorageCommand>,
    retention: chrono::Duration,
    eph_max: u64,
    eph_min: usize,
    max_windows: usize,
    app_eph_max: u64,
    app_eph_min: usize,
) -> bool {
    if !shutdown_requested || !hints_complete || *storage_shutdown_sent {
        return false;
    }

    publish_snapshot(
        snapshot_bus,
        recent_records,
        storage_metrics_rx,
        counts,
        current_state,
        current_focus,
        last_transition,
        cfg_summary,
        poll_handle,
        retention,
        eph_max,
        eph_min,
        max_windows,
        app_eph_max,
        app_eph_min,
    );
    let _ = storage_tx.send(StorageCommand::Shutdown);
    *storage_shutdown_sent = true;
    true
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn publish_snapshot(
    snapshot_bus: &snapshot::SnapshotBus,
    recent_records: &mut VecDeque<ActivityRecord>,
    sm_rx: &tokio::sync::watch::Receiver<StorageMetrics>,
    counts: &mut snapshot::Counts,
    current_state: &ActivityState,
    current_focus: &Option<WindowFocusInfo>,
    last_transition: &Option<snapshot::Transition>,
    cfg_summary: &snapshot::ConfigSummary,
    poll_handle: &Arc<AtomicU64>,
    retention: chrono::Duration,
    eph_max: u64,
    eph_min: usize,
    max_windows: usize,
    app_eph_max: u64,
    app_eph_min: usize,
) {
    let now = Utc::now();
    let since = now - retention;
    while let Some(front) = recent_records.front() {
        let end = front.end_time.unwrap_or(now);
        if end < since {
            recent_records.pop_front();
        } else {
            break;
        }
    }

    let aggregated = aggregate_activities_since(
        recent_records.make_contiguous(),
        since,
        now,
        eph_max,
        eph_min,
        max_windows,
    );

    let ms = match current_state {
        ActivityState::Active => 2000,
        ActivityState::Passive => 10000,
        ActivityState::Inactive => 20000,
        ActivityState::Locked => POLL_SUSPEND_MS,
    };
    poll_handle.store(ms, Ordering::Relaxed);

    let aggregated_apps = build_app_tree(&aggregated, app_eph_max, app_eph_min);
    let reason = match current_state {
        ActivityState::Active => "Active",
        ActivityState::Passive => "Passive",
        ActivityState::Inactive => "Inactive",
        ActivityState::Locked => "Locked",
    };
    let next_timeout = Some(Utc::now() + chrono::Duration::milliseconds(ms as i64));
    let storage_metrics = sm_rx.borrow().clone();
    let storage_info = snapshot::StorageInfo {
        backlog_count: storage_metrics.backlog_count,
        last_flush_at: storage_metrics.last_flush_at,
    };

    snapshot_bus.publish_basic(
        *current_state,
        current_focus.clone(),
        last_transition.clone(),
        counts.clone(),
        ms as u32,
        reason.to_string(),
        next_timeout,
        storage_info,
        cfg_summary.clone(),
        snapshot_bus.current_health(),
        aggregated_apps,
    );
}

fn build_app_tree(
    aggregated_activities: &[AggregatedActivity],
    app_short_max_duration_secs: u64,
    app_short_min_distinct: usize,
) -> Vec<snapshot::SnapshotApp> {
    use crate::daemon::snapshot::{SnapshotApp, SnapshotWindow};

    let mut short_per_name: StdHashMap<&str, Vec<&AggregatedActivity>> =
        StdHashMap::with_capacity(16);
    let mut normal: Vec<&AggregatedActivity> = Vec::with_capacity(aggregated_activities.len());
    for agg in aggregated_activities.iter() {
        if agg.total_duration_seconds <= app_short_max_duration_secs {
            short_per_name.entry(&agg.app_name).or_default().push(agg);
        } else {
            normal.push(agg);
        }
    }

    let mut items: Vec<SnapshotApp> = Vec::with_capacity(normal.len() + short_per_name.len());
    for agg in normal {
        let mut windows: Vec<SnapshotWindow> = Vec::with_capacity(agg.windows.len());
        for w in agg.windows.values() {
            windows.push(SnapshotWindow {
                window_id: w.window_id.to_string(),
                window_title: (*w.window_title).clone(),
                first_seen: w.first_seen,
                last_seen: w.last_seen,
                duration_seconds: w.duration_seconds,
                is_group: false,
            });
        }
        let mut groups: Vec<SnapshotWindow> = Vec::with_capacity(agg.ephemeral_groups.len());
        for g in agg.ephemeral_groups.values() {
            let avg = if g.occurrence_count > 0 {
                g.total_duration_seconds / g.occurrence_count as u64
            } else {
                0
            };
            let title = format!(
                "(short-lived) ×{} avg {}",
                g.distinct_ids.len(),
                pretty_format_duration(avg)
            );
            groups.push(SnapshotWindow {
                window_id: format!("group:{}", g.title_key),
                window_title: title,
                first_seen: g.first_seen,
                last_seen: g.last_seen,
                duration_seconds: g.total_duration_seconds,
                is_group: true,
            });
        }
        let mut temporal: Vec<SnapshotWindow> = Vec::with_capacity(agg.temporal_groups.len());
        for (idx, g) in agg.temporal_groups.iter().enumerate() {
            let avg = if g.occurrence_count > 0 {
                g.total_duration_seconds / g.occurrence_count as u64
            } else {
                0
            };
            let title = format!(
                "(temporal locality) ×{} avg {} max {}",
                g.occurrence_count,
                pretty_format_duration(avg),
                pretty_format_duration(g.max_duration_seconds),
            );
            let window_id = format!(
                "group-temporal:{}:{}:{}",
                g.title_key,
                g.anchor_last_seen.timestamp(),
                idx
            );
            temporal.push(SnapshotWindow {
                window_id,
                window_title: title,
                first_seen: g.first_seen,
                last_seen: g.last_seen,
                duration_seconds: g.total_duration_seconds,
                is_group: true,
            });
        }
        windows.append(&mut groups);
        windows.append(&mut temporal);
        windows.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        items.push(SnapshotApp {
            app_name: (*agg.app_name).clone(),
            pid: agg.pid,
            process_start_time: agg.process_start_time,
            windows,
            total_duration_secs: agg.total_duration_seconds,
            total_duration_pretty: pretty_format_duration(agg.total_duration_seconds),
        });
    }

    for (name, v) in short_per_name.into_iter() {
        if v.len() >= app_short_min_distinct {
            let mut total = 0u64;
            let mut first_seen = chrono::Utc::now();
            let mut last_seen = chrono::Utc::now();
            let mut rep_pid = 0;
            let mut rep_start = 0u64;
            let mut max_dur = 0u64;
            for agg in v.iter() {
                total = total.saturating_add(agg.total_duration_seconds);
                if agg.first_seen < first_seen {
                    first_seen = agg.first_seen;
                }
                if agg.last_seen > last_seen {
                    last_seen = agg.last_seen;
                }
                if agg.total_duration_seconds > max_dur {
                    max_dur = agg.total_duration_seconds;
                    rep_pid = agg.pid;
                    rep_start = agg.process_start_time;
                }
            }
            let count = v.len();
            let avg = if count > 0 { total / count as u64 } else { 0 };
            let windows = vec![SnapshotWindow {
                window_id: format!("app-group:{}", name),
                window_title: format!("×{} avg {}", count, pretty_format_duration(avg)),
                first_seen,
                last_seen,
                duration_seconds: total,
                is_group: true,
            }];
            items.push(SnapshotApp {
                app_name: name.to_string(),
                pid: rep_pid,
                process_start_time: rep_start,
                windows,
                total_duration_secs: total,
                total_duration_pretty: pretty_format_duration(total),
            });
        }
    }

    items.sort_by(|a, b| {
        let a_last = a.windows.first().map(|w| w.last_seen).unwrap_or_else(|| {
            chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::UNIX_EPOCH)
        });
        let b_last = b.windows.first().map(|w| w.last_seen).unwrap_or_else(|| {
            chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::UNIX_EPOCH)
        });
        b_last.cmp(&a_last)
    });
    items
}

fn pretty_format_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "0s".to_string();
    }
    let days = seconds / (24 * 3600);
    let hours = (seconds % (24 * 3600)) / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    let mut result = String::new();
    if days > 0 {
        result.push_str(&format!("{}d", days));
    }
    if hours > 0 {
        result.push_str(&format!("{}h", hours));
    }
    if minutes > 0 {
        result.push_str(&format!("{}m", minutes));
    }
    if secs > 0 || result.is_empty() {
        result.push_str(&format!("{}s", secs));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pipeline::types::SnapshotMessage;
    use crate::daemon::runtime::ThreadRegistry;
    use crate::daemon::snapshot;
    use crate::storage::StorageCommand;
    use crate::storage::storage_metrics_watch;
    use crossbeam_channel as channel;
    use std::time::Duration;

    #[test]
    fn snapshot_stage_sends_storage_shutdown_after_completion() {
        let registry = ThreadRegistry::new();
        let (snapshot_tx, snapshot_rx) = channel::unbounded();
        let (storage_tx, storage_rx) = channel::unbounded();
        let poll_handle = Arc::new(AtomicU64::new(0));
        let snapshot_bus = Arc::new(snapshot::SnapshotBus::new());
        let cfg_summary = snapshot::ConfigSummary {
            active_grace_secs: 5,
            idle_threshold_secs: 30,
            retention_minutes: 5,
            ephemeral_max_duration_secs: 60,
            ephemeral_min_distinct_ids: 2,
            ephemeral_app_max_duration_secs: 90,
            ephemeral_app_min_distinct_procs: 2,
        };

        let handle = spawn_snapshot_stage(
            &registry,
            SnapshotStageConfig {
                receiver: snapshot_rx,
                snapshot_bus: Arc::clone(&snapshot_bus),
                storage_tx: storage_tx.clone(),
                poll_handle: Arc::clone(&poll_handle),
                cfg_summary: cfg_summary.clone(),
                retention_minutes: 5,
                initial_records: Vec::new(),
                initial_transitions: Vec::new(),
                ephemeral_max_duration_secs: 60,
                ephemeral_min_distinct_ids: 2,
                max_windows_per_app: 5,
                ephemeral_app_max_duration_secs: 90,
                ephemeral_app_min_distinct_procs: 2,
                storage_metrics_rx: storage_metrics_watch(),
            },
        )
        .unwrap();

        let mut update = SnapshotUpdate::default();
        update.hints_delta = 1;
        update.signals_delta = 1;
        update.state = Some(ActivityState::Active);
        update.transition = Some(snapshot::Transition {
            from: ActivityState::Inactive,
            to: ActivityState::Active,
            at: Utc::now(),
            by_signal: Some("KeyboardInput".into()),
            run_id: None,
        });
        snapshot_tx.send(SnapshotMessage::Update(update)).unwrap();

        let record = ActivityRecord {
            record_id: 1,
            start_time: Utc::now(),
            end_time: Some(Utc::now()),
            state: ActivityState::Active,
            focus_info: None,
            event_count: 1,
            triggering_events: vec![42],
        };
        snapshot_tx.send(SnapshotMessage::Record(record)).unwrap();
        snapshot_tx.send(SnapshotMessage::Publish).unwrap();
        snapshot_tx.send(SnapshotMessage::HintsComplete).unwrap();
        snapshot_tx.send(SnapshotMessage::Shutdown).unwrap();

        loop {
            match storage_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(StorageCommand::Shutdown) => break,
                Ok(_) => continue,
                Err(_) => panic!("storage shutdown command missing"),
            }
        }

        handle.join().unwrap();
        assert_eq!(poll_handle.load(Ordering::Relaxed), 2000);
        let snapshot = snapshot_bus.snapshot();
        assert_eq!(snapshot.counts.hints_seen, 1);
        assert_eq!(snapshot.counts.signals_seen, 1);
        assert_eq!(snapshot.counts.records_emitted, 1);
    }
}
