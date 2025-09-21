use crate::daemon::compressor::CompressionEngine;
use crate::daemon::coordinator::KronicalEvent;
use crate::daemon::events::adapter::EventAdapter;
use crate::daemon::events::derive_hint::StateDeriver;
use crate::daemon::events::derive_signal::LockDeriver;
use crate::daemon::events::model::{EventEnvelope, EventKind, EventPayload, SignalKind};
use crate::daemon::events::{
    KeyboardEventData, MouseEventData, MousePosition, RawEvent, WheelAxis, WindowFocusInfo,
};
use crate::daemon::records::{
    ActivityRecord, ActivityState, AggregatedActivity, aggregate_activities_since,
};
use crate::daemon::snapshot;
use crate::storage::{StorageBackend, StorageCommand, storage_metrics_watch};
use anyhow::Result;
use chrono::Utc;
use log::{debug, error, info, trace};
use std::collections::{HashMap as StdHashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uiohook_rs::hook::wheel::{WHEEL_HORIZONTAL_DIRECTION, WHEEL_VERTICAL_DIRECTION};

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub retention_minutes: u64,
    pub active_grace_secs: u64,
    pub idle_threshold_secs: u64,
    pub ephemeral_max_duration_secs: u64,
    pub ephemeral_min_distinct_ids: usize,
    pub max_windows_per_app: usize,
    pub ephemeral_app_max_duration_secs: u64,
    pub ephemeral_app_min_distinct_procs: usize,
    pub focus_interner_max_strings: usize,
}

pub struct PipelineResources {
    pub storage: Box<dyn StorageBackend>,
    pub event_rx: mpsc::Receiver<KronicalEvent>,
    pub poll_handle: Arc<AtomicU64>,
    pub snapshot_bus: Arc<snapshot::SnapshotBus>,
}

pub struct PipelineHandles {
    data_thread: JoinHandle<()>,
}

impl PipelineHandles {
    pub fn join(self) -> thread::Result<()> {
        self.data_thread.join()
    }
}

pub fn spawn_pipeline(config: PipelineConfig, resources: PipelineResources) -> PipelineHandles {
    let PipelineConfig {
        retention_minutes,
        active_grace_secs,
        idle_threshold_secs,
        ephemeral_max_duration_secs,
        ephemeral_min_distinct_ids,
        max_windows_per_app,
        ephemeral_app_max_duration_secs,
        ephemeral_app_min_distinct_procs,
        focus_interner_max_strings,
    } = config;

    let PipelineResources {
        storage,
        event_rx,
        poll_handle,
        snapshot_bus,
    } = resources;

    let data_thread = thread::spawn(move || {
        info!("Background data processing thread started");

        let mut compression_engine = CompressionEngine::with_focus_cap(focus_interner_max_strings);
        let mut adapter = EventAdapter::new();
        let mut lock_deriver = LockDeriver::new();
        let mut store = storage;
        let mut pending_raw_events: Vec<RawEvent> = Vec::new();
        let mut state_hist: VecDeque<char> = VecDeque::new();
        let mut recent_records: VecDeque<ActivityRecord> = VecDeque::new();
        let mut current_focus: Option<WindowFocusInfo> = None;
        let retention = chrono::Duration::minutes(retention_minutes as i64);
        let eph_max = ephemeral_max_duration_secs;
        let eph_min = ephemeral_min_distinct_ids;
        let max_windows = max_windows_per_app;
        let app_eph_max = ephemeral_app_max_duration_secs;
        let app_eph_min = ephemeral_app_min_distinct_procs;

        let mut counts = snapshot::Counts {
            signals_seen: 0,
            hints_seen: 0,
            records_emitted: 0,
        };
        let mut last_transition: Option<snapshot::Transition> = None;

        let sm_rx = storage_metrics_watch();
        let poll_handle_inner = Arc::clone(&poll_handle);
        let ags = active_grace_secs;
        let its = idle_threshold_secs;

        let cfg_summary = snapshot::ConfigSummary {
            active_grace_secs,
            idle_threshold_secs,
            retention_minutes,
            ephemeral_max_duration_secs,
            ephemeral_min_distinct_ids,
            ephemeral_app_max_duration_secs,
            ephemeral_app_min_distinct_procs,
        };

        let mut event_count = 0u64;
        let now0 = Utc::now();
        let since0 = now0 - retention;
        match store.fetch_records_since(since0) {
            Ok(mut recs) => {
                for r in recs.drain(..) {
                    recent_records.push_back(r);
                }
                let _ = aggregate_activities_since(
                    recent_records.make_contiguous(),
                    since0,
                    now0,
                    eph_max,
                    eph_min,
                    max_windows,
                );
                info!(
                    "Hydrated {} records from DB since {}",
                    recent_records.len(),
                    since0
                );
            }
            Err(e) => error!("Failed to hydrate records from DB: {}", e),
        }

        let (hints_tx, hints_rx) = mpsc::channel::<EventEnvelope>();
        let (rec_fb_tx, rec_fb_rx) = mpsc::channel::<ActivityRecord>();
        let (storage_tx, storage_rx) = mpsc::channel::<StorageCommand>();

        let mut store_writer = store;
        let storage_thread = thread::spawn(move || {
            info!("Storage writer thread started");
            while let Ok(cmd) = storage_rx.recv() {
                match cmd {
                    StorageCommand::RawEvent(e) => {
                        if let Err(e2) = store_writer.add_events(vec![e]) {
                            error!("store add_events error: {}", e2);
                        }
                    }
                    StorageCommand::Record(r) => {
                        if let Err(e2) = store_writer.add_records(vec![r]) {
                            error!("store add_records error: {}", e2);
                        }
                    }
                    StorageCommand::Envelope(env) => {
                        if let Err(e2) = store_writer.add_envelopes(vec![env]) {
                            error!("store add_envelopes error: {}", e2);
                        }
                    }
                    StorageCommand::CompactEvents(evts) => {
                        if let Err(e2) = store_writer.add_compact_events(evts) {
                            error!("store add_compact_events error: {}", e2);
                        }
                    }
                    StorageCommand::Shutdown => {
                        info!("Storage writer shutdown received");
                        break;
                    }
                }
            }
            info!("Storage writer thread exiting");
        });

        let hints_storage_tx = storage_tx.clone();
        let hints_fb_tx = rec_fb_tx.clone();
        let hints_thread = thread::spawn(move || {
            info!("Hints worker thread started");
            let mut record_builder =
                crate::daemon::records::RecordBuilder::new(ActivityState::Inactive);
            while let Ok(env) = hints_rx.recv() {
                if let Err(e) = hints_storage_tx.send(StorageCommand::Envelope(env.clone())) {
                    error!("Failed to enqueue hint envelope to storage: {}", e);
                }
                if let Some(rec) = record_builder.on_hint(&env) {
                    if let Err(e) = hints_storage_tx.send(StorageCommand::Record(rec.clone())) {
                        error!("Failed to enqueue record to storage: {}", e);
                    }
                    let _ = hints_fb_tx.send(rec);
                }
            }
            if let Some(final_rec) = record_builder.finalize_all() {
                let _ = hints_storage_tx.send(StorageCommand::Record(final_rec.clone()));
                let _ = hints_fb_tx.send(final_rec);
            }
            info!("Hints worker thread exiting");
        });

        let mut state_deriver = StateDeriver::new(Utc::now(), ags as i64, its as i64);
        let mut current_state = ActivityState::Inactive;

        loop {
            match event_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(KronicalEvent::Shutdown) => {
                    info!("Shutdown signal received, flushing pending events and finalizing");
                    if !pending_raw_events.is_empty() {
                        let raw_events_to_process = std::mem::take(&mut pending_raw_events);
                        let envelopes = adapter.adapt_batch(&raw_events_to_process);
                        let envelopes_with_lock = lock_deriver.derive(&envelopes);
                        for env in envelopes_with_lock.iter() {
                            match &env.kind {
                                EventKind::Hint(_) => {
                                    counts.hints_seen += 1;
                                    if let EventPayload::Focus(fi) = &env.payload {
                                        current_focus = Some(fi.clone());
                                    }
                                    if let EventPayload::State { from, to } = env.payload.clone() {
                                        let tr = snapshot::Transition {
                                            from,
                                            to,
                                            at: env.timestamp,
                                            by_signal: None,
                                        };
                                        last_transition = Some(tr.clone());
                                        snapshot_bus.push_transition(tr);
                                        current_state = to;
                                    }
                                    let _ = hints_tx.send(env.clone());
                                }
                                EventKind::Signal(_) => {
                                    counts.signals_seen += 1;
                                    if let EventPayload::Focus(fi) = &env.payload {
                                        current_focus = Some(fi.clone());
                                    }
                                    let _ = storage_tx.send(StorageCommand::Envelope(env.clone()));
                                    if let Some(h) = state_deriver.on_signal(env) {
                                        counts.hints_seen += 1;
                                        if let EventPayload::State { from, to } = h.payload.clone()
                                        {
                                            let sig = match &env.kind {
                                                EventKind::Signal(sk) => Some(format!("{:?}", sk)),
                                                _ => None,
                                            };
                                            let tr = snapshot::Transition {
                                                from,
                                                to,
                                                at: h.timestamp,
                                                by_signal: sig,
                                            };
                                            last_transition = Some(tr.clone());
                                            snapshot_bus.push_transition(tr);
                                            current_state = to;
                                        }
                                        let _ = hints_tx.send(h);
                                    }
                                }
                            }
                        }
                        while let Ok(r) = rec_fb_rx.try_recv() {
                            let ch = match r.state {
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
                            recent_records.push_back(r);
                            counts.records_emitted += 1;
                        }
                    }
                    break;
                }
                Ok(event) => {
                    event_count += 1;
                    debug!("Processing event #{}: {:?}", event_count, event);

                    if let Ok(raw_event) = convert_kronid_to_raw(event) {
                        pending_raw_events.push(raw_event);
                        trace!(
                            "Added raw event to pending batch (total: {})",
                            pending_raw_events.len()
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if pending_raw_events.is_empty() {
                        continue;
                    }

                    debug!("Processing {} pending raw events", pending_raw_events.len());

                    let raw_events_to_process = std::mem::take(&mut pending_raw_events);
                    let envelopes = adapter.adapt_batch(&raw_events_to_process);
                    let envelopes_with_lock = lock_deriver.derive(&envelopes);
                    for env in envelopes_with_lock.iter() {
                        match &env.kind {
                            EventKind::Hint(_) => {
                                counts.hints_seen += 1;
                                if let EventPayload::Focus(fi) = &env.payload {
                                    current_focus = Some(fi.clone());
                                }
                                if let EventPayload::State { from, to } = env.payload.clone() {
                                    let tr = snapshot::Transition {
                                        from,
                                        to,
                                        at: env.timestamp,
                                        by_signal: None,
                                    };
                                    last_transition = Some(tr.clone());
                                    snapshot_bus.push_transition(tr);
                                    current_state = to;
                                }
                                let _ = hints_tx.send(env.clone());
                            }
                            EventKind::Signal(_) => {
                                counts.signals_seen += 1;
                                if let EventPayload::Focus(fi) = &env.payload {
                                    current_focus = Some(fi.clone());
                                }
                                let _ = storage_tx.send(StorageCommand::Envelope(env.clone()));
                                if let Some(h) = state_deriver.on_signal(env) {
                                    counts.hints_seen += 1;
                                    if let EventPayload::State { from, to } = h.payload.clone() {
                                        let sig = match &env.kind {
                                            EventKind::Signal(sk) => Some(format!("{:?}", sk)),
                                            _ => None,
                                        };
                                        let tr = snapshot::Transition {
                                            from,
                                            to,
                                            at: h.timestamp,
                                            by_signal: sig,
                                        };
                                        last_transition = Some(tr.clone());
                                        snapshot_bus.push_transition(tr);
                                        current_state = to;
                                    }
                                    let _ = hints_tx.send(h);
                                }
                            }
                        }
                    }

                    while let Ok(r) = rec_fb_rx.try_recv() {
                        let ch = match r.state {
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
                        recent_records.push_back(r);
                        counts.records_emitted += 1;
                    }

                    match compression_engine.compress_events(raw_events_to_process.clone()) {
                        Ok((processed_events, compact_events)) => {
                            if !processed_events.is_empty() {
                                let mut suppress: HashSet<u64> = HashSet::new();
                                let mut locked = false;
                                for env in &envelopes_with_lock {
                                    match &env.kind {
                                        EventKind::Signal(SignalKind::LockStart) => locked = true,
                                        EventKind::Signal(SignalKind::LockEnd) => locked = false,
                                        EventKind::Signal(
                                            SignalKind::KeyboardInput | SignalKind::MouseInput,
                                        ) if locked => {
                                            suppress.insert(env.id);
                                        }
                                        _ => {}
                                    }
                                }
                                let to_store: Vec<_> = processed_events
                                    .into_iter()
                                    .filter(|ev| match ev {
                                        RawEvent::KeyboardInput { event_id, .. }
                                        | RawEvent::MouseInput { event_id, .. } => {
                                            !suppress.contains(event_id)
                                        }
                                        _ => true,
                                    })
                                    .collect();
                                for ev in to_store {
                                    if let Err(e) = storage_tx.send(StorageCommand::RawEvent(ev)) {
                                        error!("Failed to enqueue raw event: {}", e);
                                    }
                                }
                            }

                            if !compact_events.is_empty() {
                                if let Err(e) =
                                    storage_tx.send(StorageCommand::CompactEvents(compact_events))
                                {
                                    error!("Failed to enqueue compact events to storage: {}", e);
                                }
                            }
                        }
                        Err(e) => error!("Failed to compress events: {}", e),
                    }

                    publish_snapshot(
                        snapshot_bus.as_ref(),
                        &mut recent_records,
                        &sm_rx,
                        &mut counts,
                        &current_state,
                        &current_focus,
                        &last_transition,
                        &cfg_summary,
                        &poll_handle_inner,
                        retention,
                        eph_max,
                        eph_min,
                        max_windows,
                        app_eph_max,
                        app_eph_min,
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    info!("Channel disconnected, exiting");
                    break;
                }
            }

            if let Some(state_hint) = state_deriver.on_tick(Utc::now()) {
                counts.hints_seen += 1;
                if let EventPayload::State { from, to } = state_hint.payload.clone() {
                    let tr = snapshot::Transition {
                        from,
                        to,
                        at: state_hint.timestamp,
                        by_signal: None,
                    };
                    last_transition = Some(tr.clone());
                    snapshot_bus.push_transition(tr);
                    current_state = to;
                }
                let _ = hints_tx.send(state_hint);
                while let Ok(rec) = rec_fb_rx.try_recv() {
                    let ch = match rec.state {
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
                    recent_records.push_back(rec);
                    counts.records_emitted += 1;
                }

                publish_snapshot(
                    snapshot_bus.as_ref(),
                    &mut recent_records,
                    &sm_rx,
                    &mut counts,
                    &current_state,
                    &current_focus,
                    &last_transition,
                    &cfg_summary,
                    &poll_handle_inner,
                    retention,
                    eph_max,
                    eph_min,
                    max_windows,
                    app_eph_max,
                    app_eph_min,
                );
            }
        }

        drop(hints_tx);
        while let Ok(rec) = rec_fb_rx.recv_timeout(Duration::from_millis(200)) {
            recent_records.push_back(rec);
            if rec_fb_rx.recv_timeout(Duration::from_millis(1)).is_err() {
                break;
            }
        }
        let _ = storage_tx.send(StorageCommand::Shutdown);

        publish_snapshot(
            snapshot_bus.as_ref(),
            &mut recent_records,
            &sm_rx,
            &mut counts,
            &current_state,
            &current_focus,
            &last_transition,
            &cfg_summary,
            &poll_handle_inner,
            retention,
            eph_max,
            eph_min,
            max_windows,
            app_eph_max,
            app_eph_min,
        );

        let _ = hints_thread.join();
        let _ = storage_thread.join();
        info!("Background thread completed");
    });

    PipelineHandles { data_thread }
}

fn publish_snapshot(
    snapshot_bus: &snapshot::SnapshotBus,
    recent_records: &mut VecDeque<ActivityRecord>,
    sm_rx: &tokio::sync::watch::Receiver<crate::storage::StorageMetrics>,
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

    let agg = aggregate_activities_since(
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
        ActivityState::Locked => 30000,
    };
    poll_handle.store(ms, Ordering::Relaxed);

    let aggregated_apps = build_app_tree(&agg, app_eph_max, app_eph_min);
    let reason = match current_state {
        ActivityState::Active => "Active",
        ActivityState::Passive => "Passive",
        ActivityState::Inactive => "Inactive",
        ActivityState::Locked => "Locked",
    };
    let next_timeout = Some(Utc::now() + chrono::Duration::milliseconds(ms as i64));
    let sm = sm_rx.borrow().clone();

    snapshot_bus.publish_basic(
        *current_state,
        current_focus.clone(),
        last_transition.clone(),
        counts.clone(),
        ms as u32,
        reason.to_string(),
        next_timeout,
        snapshot::StorageInfo {
            backlog_count: sm.backlog_count,
            last_flush_at: sm.last_flush_at,
        },
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
                "{} (short-lived) ×{} avg {}",
                g.title_key,
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
        windows.append(&mut groups);
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

fn convert_kronid_to_raw(event: KronicalEvent) -> Result<RawEvent> {
    let now = Utc::now();
    let event_id = now.timestamp_millis() as u64;

    match event {
        KronicalEvent::KeyboardInput => Ok(RawEvent::KeyboardInput {
            timestamp: now,
            event_id,
            data: KeyboardEventData {
                key_code: None,
                key_char: None,
                modifiers: Vec::new(),
            },
        }),
        KronicalEvent::MouseInput {
            x,
            y,
            button,
            clicks,
            kind,
        } => Ok(RawEvent::MouseInput {
            timestamp: now,
            event_id,
            data: MouseEventData {
                position: MousePosition { x, y },
                button,
                click_count: if clicks > 0 { Some(clicks) } else { None },
                event_type: Some(kind),
                wheel_amount: None,
                wheel_rotation: None,
                wheel_axis: None,
            },
        }),
        KronicalEvent::MouseWheel {
            clicks,
            x,
            y,
            type_: _,
            amount,
            rotation,
            direction,
        } => {
            let axis = if direction == WHEEL_VERTICAL_DIRECTION {
                Some(WheelAxis::Vertical)
            } else if direction == WHEEL_HORIZONTAL_DIRECTION {
                Some(WheelAxis::Horizontal)
            } else {
                None
            };
            Ok(RawEvent::MouseInput {
                timestamp: now,
                event_id,
                data: MouseEventData {
                    position: MousePosition { x, y },
                    button: None,
                    click_count: Some(clicks as u16),
                    event_type: None,
                    wheel_amount: Some(amount as i32),
                    wheel_rotation: Some(rotation as i32),
                    wheel_axis: axis,
                },
            })
        }
        KronicalEvent::WindowFocusChange { focus_info } => Ok(RawEvent::WindowFocusChange {
            timestamp: now,
            event_id,
            focus_info,
        }),
        KronicalEvent::Shutdown => {
            Err(anyhow::anyhow!("Cannot convert Shutdown event to RawEvent"))
        }
    }
}
