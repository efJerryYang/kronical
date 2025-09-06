use crate::daemon::compressor::CompressionEngine;
use crate::daemon::events::adapter::EventAdapter;
use crate::daemon::events::derive_hint::StateDeriver;
use crate::daemon::events::derive_signal::LockDeriver;
use crate::daemon::events::model::{EventKind, EventPayload, SignalKind};
use crate::daemon::events::{
    KeyboardEventData, MouseEventData, MouseEventKind, MousePosition, WindowFocusInfo,
};
use crate::daemon::records::{ActivityRecord, aggregate_activities_since};
use crate::daemon::tracker::DuckDbSystemTracker;
use crate::daemon::tracker::{FocusCacheCaps, FocusChangeCallback, FocusEventWrapper};
use crate::storage::{StorageBackend, StorageCommand};
use anyhow::Result;
use log::{debug, error, info, trace};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;
use uiohook_rs::{EventHandler, Uiohook, UiohookEvent};

use winshift::ActiveWindowInfo;
use winshift::{FocusChangeHandler, MonitoringMode, WindowFocusHook, WindowHookConfig};

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

#[derive(Debug, Clone)]
pub enum KronicalEvent {
    KeyboardInput,
    MouseInput {
        x: i32,
        y: i32,
        button: Option<String>,
        clicks: u16,
        kind: MouseEventKind,
    },
    MouseWheel {
        clicks: u16,
        x: i32,
        y: i32,
        type_: u8,
        amount: u16,
        rotation: i16,
        direction: u8,
    },
    WindowFocusChange {
        focus_info: WindowFocusInfo,
    },
    Shutdown,
}

pub struct KronicalEventHandler {
    sender: mpsc::Sender<KronicalEvent>,
    focus_wrapper: FocusEventWrapper,
}

impl KronicalEventHandler {
    fn new(
        sender: mpsc::Sender<KronicalEvent>,
        caps: FocusCacheCaps,
        poll_handle: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let callback = Arc::new(FocusCallback {
            sender: sender.clone(),
        });
        let initial_ms = poll_handle.load(std::sync::atomic::Ordering::Relaxed);
        let focus_wrapper = FocusEventWrapper::new(
            callback,
            Duration::from_millis(initial_ms),
            caps,
            poll_handle,
        );

        Self {
            sender,
            focus_wrapper,
        }
    }
}

struct FocusCallback {
    sender: mpsc::Sender<KronicalEvent>,
}

impl FocusChangeCallback for FocusCallback {
    fn on_focus_change(&self, focus_info: WindowFocusInfo) {
        if let Err(e) = self
            .sender
            .send(KronicalEvent::WindowFocusChange { focus_info })
        {
            error!("Failed to send unified focus change event: {}", e);
        } else {
            info!("Unified focus change event sent successfully");
        }
    }
}

impl EventHandler for KronicalEventHandler {
    fn handle_event(&self, event: &UiohookEvent) {
        trace!("UIohook event received: {:?}", event);
        let kronid_event = match event {
            UiohookEvent::Keyboard(_) => {
                trace!("Keyboard event detected");
                KronicalEvent::KeyboardInput
            }
            UiohookEvent::Mouse(m) => {
                use uiohook_rs::hook::mouse::MouseEventType as VMouseEventType;
                trace!("Mouse event detected");
                let kind = match m.event_type {
                    VMouseEventType::Moved => MouseEventKind::Moved,
                    VMouseEventType::Pressed => MouseEventKind::Pressed,
                    VMouseEventType::Released => MouseEventKind::Released,
                    VMouseEventType::Clicked => MouseEventKind::Clicked,
                    VMouseEventType::Dragged => MouseEventKind::Dragged,
                };
                // Map button; treat NoButton as None
                let button_str = match format!("{:?}", m.button).as_str() {
                    "NoButton" => None,
                    other => Some(other.to_string()),
                };
                KronicalEvent::MouseInput {
                    x: m.x as i32,
                    y: m.y as i32,
                    button: button_str,
                    clicks: m.clicks,
                    kind,
                }
            }
            UiohookEvent::Wheel(w) => {
                trace!("Wheel event detected");
                KronicalEvent::MouseWheel {
                    clicks: w.clicks,
                    x: w.x as i32,
                    y: w.y as i32,
                    type_: w.type_,
                    amount: w.amount,
                    rotation: w.rotation,
                    direction: w.direction,
                }
            }
            UiohookEvent::HookEnabled => {
                debug!("UIohook enabled");
                return;
            }
            UiohookEvent::HookDisabled => {
                debug!("UIohook disabled");
                return;
            }
        };

        if let Err(e) = self.sender.send(kronid_event) {
            error!("Failed to send input event: {}", e);
        } else {
            trace!("Event sent to background thread successfully");
        }
    }
}

impl FocusChangeHandler for KronicalEventHandler {
    fn on_app_change(&self, pid: i32, app_name: String) {
        debug!("App changed: {} (PID: {})", app_name, pid);
        self.focus_wrapper.handle_app_change(pid, app_name);
    }

    fn on_window_change(&self, window_title: String) {
        debug!("Window changed: {}", window_title);
        self.focus_wrapper.handle_window_change(window_title);
    }

    fn on_app_change_info(&self, info: ActiveWindowInfo) {
        debug!(
            "App+Info: {} pid={} wid={}",
            info.app_name, info.process_id, info.window_id
        );
        self.focus_wrapper.handle_app_change_info(info);
    }

    fn on_window_change_info(&self, info: ActiveWindowInfo) {
        debug!("Win+Info: '{}' wid={}", info.title, info.window_id);
        self.focus_wrapper.handle_window_change_info(info);
    }
}

pub struct EventCoordinator {
    retention_minutes: u64,
    active_grace_secs: u64,
    idle_threshold_secs: u64,
    ephemeral_max_duration_secs: u64,
    ephemeral_min_distinct_ids: usize,
    max_windows_per_app: usize,
    ephemeral_app_max_duration_secs: u64,
    ephemeral_app_min_distinct_procs: usize,
    pid_cache_capacity: usize,
    title_cache_capacity: usize,
    title_cache_ttl_secs: u64,
    focus_interner_max_strings: usize,
    tracker_enabled: bool,
    tracker_interval_secs: f64,
    tracker_batch_size: usize,
}

impl EventCoordinator {
    pub fn new(
        retention_minutes: u64,
        active_grace_secs: u64,
        idle_threshold_secs: u64,
        ephemeral_max_duration_secs: u64,
        ephemeral_min_distinct_ids: usize,
        max_windows_per_app: usize,
        ephemeral_app_max_duration_secs: u64,
        ephemeral_app_min_distinct_procs: usize,
        pid_cache_capacity: usize,
        title_cache_capacity: usize,
        title_cache_ttl_secs: u64,
        focus_interner_max_strings: usize,
        tracker_enabled: bool,
        tracker_interval_secs: f64,
        tracker_batch_size: usize,
    ) -> Self {
        Self {
            retention_minutes,
            active_grace_secs,
            idle_threshold_secs,
            ephemeral_max_duration_secs,
            ephemeral_min_distinct_ids,
            max_windows_per_app,
            ephemeral_app_max_duration_secs,
            ephemeral_app_min_distinct_procs,
            pid_cache_capacity,
            title_cache_capacity,
            title_cache_ttl_secs,
            focus_interner_max_strings,
            tracker_enabled,
            tracker_interval_secs,
            tracker_batch_size,
        }
    }

    fn convert_kronid_to_raw(event: KronicalEvent) -> Result<crate::daemon::events::RawEvent> {
        use crate::daemon::events::RawEvent;
        use chrono::Utc;

        let now = Utc::now();
        let event_id = chrono::Utc::now().timestamp_millis() as u64;

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
                use crate::daemon::events::WheelAxis;
                use uiohook_rs::hook::wheel::{
                    WHEEL_HORIZONTAL_DIRECTION, WHEEL_VERTICAL_DIRECTION,
                };
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

    // TODO: the pid file should not be passed here, we should have a constant rust file. and we can then get the workspace directory from there too
    pub fn start_main_thread(
        &self,
        data_store: Box<dyn StorageBackend>,
        pid_file: std::path::PathBuf,
    ) -> Result<()> {
        info!("Step A: Starting Kronical on MAIN THREAD (required for hooks)");

        let (sender, receiver) = mpsc::channel();

        // Start API servers (gRPC + HTTP/SSE) via unified API facade.
        let workspace_dir = match pid_file.parent() {
            Some(dir) => dir,
            None => {
                error!(
                    "PID file has no parent; cannot derive workspace dir. Skipping API startup."
                );
                return Err(anyhow::anyhow!(
                    "invalid PID file path; missing parent directory"
                ));
            }
        };
        let uds_grpc = crate::util::paths::grpc_uds(workspace_dir);
        let uds_http = crate::util::paths::http_uds(workspace_dir);
        if let Err(e) = crate::daemon::api::spawn_all(uds_grpc, uds_http) {
            error!("Failed to start API servers: {}", e);
        }

        if self.tracker_enabled {
            let current_pid = std::process::id();
            let tracker_db_path = crate::util::paths::tracker_db(workspace_dir);
            let tracker = DuckDbSystemTracker::new(
                current_pid,
                self.tracker_interval_secs,
                self.tracker_batch_size,
                tracker_db_path.clone(),
            );
            if let Err(e) = tracker.start() {
                error!("Failed to start DuckDB system tracker: {}", e);
            } else {
                info!(
                    "DuckDB system tracker started successfully for PID {}",
                    current_pid
                );

                crate::daemon::api::set_system_tracker_db_path(tracker_db_path.clone());
                info!(
                    "System tracker DB path set for gRPC API: {:?}",
                    tracker_db_path
                );
            }
        }

        let poll_handle_arc = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(2000));
        let data_thread = {
            let mut compression_engine =
                CompressionEngine::with_focus_cap(self.focus_interner_max_strings);
            let mut adapter = EventAdapter::new();
            let mut lock_deriver = LockDeriver::new();
            let mut store = data_store;
            let mut pending_raw_events = Vec::new();
            let mut state_hist: std::collections::VecDeque<char> =
                std::collections::VecDeque::new();
            let mut recent_records: std::collections::VecDeque<ActivityRecord> =
                std::collections::VecDeque::new();
            let mut current_focus: Option<WindowFocusInfo> = None;
            let retention = chrono::Duration::minutes(self.retention_minutes as i64);
            let eph_max = self.ephemeral_max_duration_secs;
            let eph_min = self.ephemeral_min_distinct_ids;
            let max_windows = self.max_windows_per_app;

            let app_eph_max = self.ephemeral_app_max_duration_secs;

            let app_eph_min = self.ephemeral_app_min_distinct_procs;

            let mut counts = crate::daemon::snapshot::Counts {
                signals_seen: 0,
                hints_seen: 0,
                records_emitted: 0,
            };

            let mut last_transition: Option<crate::daemon::snapshot::Transition> = None;
            // Subscribe to live storage metrics updates
            let sm_rx = crate::storage::storage_metrics_watch();

            let poll_handle_arc2 = std::sync::Arc::clone(&poll_handle_arc);
            let ags = self.active_grace_secs;
            let its = self.idle_threshold_secs;

            let cfg_summary = crate::daemon::snapshot::ConfigSummary {
                active_grace_secs: self.active_grace_secs,
                idle_threshold_secs: self.idle_threshold_secs,
                retention_minutes: self.retention_minutes,
                ephemeral_max_duration_secs: self.ephemeral_max_duration_secs,
                ephemeral_min_distinct_ids: self.ephemeral_min_distinct_ids,
                ephemeral_app_max_duration_secs: self.ephemeral_app_max_duration_secs,
                ephemeral_app_min_distinct_procs: self.ephemeral_app_min_distinct_procs,
            };

            fn build_app_tree(
                aggregated_activities: &[crate::daemon::records::AggregatedActivity],
                app_short_max_duration_secs: u64,
                app_short_min_distinct: usize,
            ) -> Vec<crate::daemon::snapshot::SnapshotApp> {
                use crate::daemon::snapshot::{SnapshotApp, SnapshotWindow};
                use std::collections::HashMap as StdHashMap;
                let mut short_per_name: StdHashMap<
                    &str,
                    Vec<&crate::daemon::records::AggregatedActivity>,
                > = StdHashMap::with_capacity(16);
                let mut normal: Vec<&crate::daemon::records::AggregatedActivity> =
                    Vec::with_capacity(aggregated_activities.len());
                for agg in aggregated_activities.iter() {
                    if agg.total_duration_seconds <= app_short_max_duration_secs {
                        short_per_name.entry(&agg.app_name).or_default().push(agg);
                    } else {
                        normal.push(agg);
                    }
                }
                let mut items: Vec<SnapshotApp> =
                    Vec::with_capacity(normal.len() + short_per_name.len());
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
                    let mut groups: Vec<SnapshotWindow> =
                        Vec::with_capacity(agg.ephemeral_groups.len());
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
                            app_name: format!("{} (short-lived)", name),
                            pid: rep_pid,
                            process_start_time: rep_start,
                            windows,
                            total_duration_secs: total,
                            total_duration_pretty: pretty_format_duration(total),
                        });
                    } else {
                        for agg in v {
                            let mut windows: Vec<SnapshotWindow> =
                                Vec::with_capacity(agg.windows.len());
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
                            let mut groups: Vec<SnapshotWindow> =
                                Vec::with_capacity(agg.ephemeral_groups.len());
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
                                total_duration_pretty: pretty_format_duration(
                                    agg.total_duration_seconds,
                                ),
                            });
                        }
                    }
                }
                items.sort_by(|a, b| {
                    let a_last =
                        a.windows
                            .first()
                            .map(|w| w.last_seen)
                            .unwrap_or(chrono::DateTime::<chrono::Utc>::from(
                                std::time::SystemTime::UNIX_EPOCH,
                            ));
                    let b_last =
                        b.windows
                            .first()
                            .map(|w| w.last_seen)
                            .unwrap_or(chrono::DateTime::<chrono::Utc>::from(
                                std::time::SystemTime::UNIX_EPOCH,
                            ));
                    b_last.cmp(&a_last)
                });
                items
            }

            thread::spawn(move || {
                info!("Background data processing thread started");
                let mut event_count = 0;
                let now0 = chrono::Utc::now();
                let since0 = now0 - retention;
                match store.fetch_records_since(since0) {
                    Ok(mut recs) => {
                        for r in recs.drain(..) {
                            recent_records.push_back(r);
                        }
                        let _agg = aggregate_activities_since(
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

                // Channel-based pipeline setup
                let (hints_tx, hints_rx) =
                    mpsc::channel::<crate::daemon::events::model::EventEnvelope>();
                let (rec_fb_tx, rec_fb_rx) =
                    mpsc::channel::<crate::daemon::records::ActivityRecord>();
                let (storage_tx, storage_rx) = mpsc::channel::<StorageCommand>();

                // Move storage into dedicated writer thread
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
                            StorageCommand::Shutdown => {
                                info!("Storage writer shutdown received");
                                break;
                            }
                        }
                    }
                    info!("Storage writer thread exiting");
                });

                // Hints worker thread: builds records from hints
                let hints_storage_tx = storage_tx.clone();
                let hints_fb_tx = rec_fb_tx.clone();
                let hints_thread = thread::spawn(move || {
                    info!("Hints worker thread started");
                    let mut record_builder = crate::daemon::records::RecordBuilder::new(
                        crate::daemon::records::ActivityState::Inactive,
                    );
                    while let Ok(env) = hints_rx.recv() {
                        // Persist hint envelope
                        if let Err(e) = hints_storage_tx.send(StorageCommand::Envelope(env.clone()))
                        {
                            error!("Failed to enqueue hint envelope to storage: {}", e);
                        }
                        if let Some(rec) = record_builder.on_hint(&env) {
                            if let Err(e) =
                                hints_storage_tx.send(StorageCommand::Record(rec.clone()))
                            {
                                error!("Failed to enqueue record to storage: {}", e);
                            }
                            let _ = hints_fb_tx.send(rec);
                        }
                    }
                    // Finalize when channel closes
                    if let Some(final_rec) = record_builder.finalize_all() {
                        let _ = hints_storage_tx.send(StorageCommand::Record(final_rec.clone()));
                        let _ = hints_fb_tx.send(final_rec);
                    }
                    info!("Hints worker thread exiting");
                });

                // Initialize derivation engine and state tracker
                let mut state_deriver = StateDeriver::new(now0, ags as i64, its as i64);
                let mut current_state = crate::daemon::records::ActivityState::Inactive;

                loop {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(KronicalEvent::Shutdown) => {
                            info!(
                                "Shutdown signal received, flushing pending events and finalizing"
                            );
                            if !pending_raw_events.is_empty() {
                                let raw_events_to_process = std::mem::take(&mut pending_raw_events);
                                // Adapt to EventEnvelope + derive lock boundaries
                                let envelopes = adapter.adapt_batch(&raw_events_to_process);
                                let envelopes_with_lock = lock_deriver.derive(&envelopes);
                                // Dispatch envelopes via explicit channels
                                for env in envelopes_with_lock.iter() {
                                    match &env.kind {
                                        EventKind::Hint(_) => {
                                            counts.hints_seen += 1;
                                            if let EventPayload::Focus(fi) = &env.payload {
                                                current_focus = Some(fi.clone());
                                            }
                                            if let EventPayload::State { from, to } =
                                                env.payload.clone()
                                            {
                                                last_transition =
                                                    Some(crate::daemon::snapshot::Transition {
                                                        from,
                                                        to,
                                                        at: env.timestamp,
                                                    });
                                                current_state = to;
                                            }
                                            let _ = hints_tx.send(env.clone());
                                        }
                                        EventKind::Signal(_) => {
                                            counts.signals_seen += 1;
                                            if let EventPayload::Focus(fi) = &env.payload {
                                                current_focus = Some(fi.clone());
                                            }
                                            // Persist signal envelope immediately
                                            let _ = storage_tx
                                                .send(StorageCommand::Envelope(env.clone()));
                                            if let Some(h) = state_deriver.on_signal(env) {
                                                counts.hints_seen += 1;
                                                if let EventPayload::State { from, to } =
                                                    h.payload.clone()
                                                {
                                                    last_transition =
                                                        Some(crate::daemon::snapshot::Transition {
                                                            from,
                                                            to,
                                                            at: h.timestamp,
                                                        });
                                                    current_state = to;
                                                }
                                                let _ = hints_tx.send(h);
                                            }
                                        }
                                    }
                                }
                                // Drain any records produced by hints worker
                                while let Ok(rec) = rec_fb_rx.try_recv() {
                                    recent_records.push_back(rec);
                                }
                                let now = chrono::Utc::now();
                                let since = now - retention;
                                while let Some(front) = recent_records.front() {
                                    let end = front.end_time.unwrap_or(now);
                                    if end < since {
                                        recent_records.pop_front();
                                    } else {
                                        break;
                                    }
                                }

                                let agg = {
                                    aggregate_activities_since(
                                        recent_records.make_contiguous(),
                                        since,
                                        now,
                                        eph_max,
                                        eph_min,
                                        max_windows,
                                    )
                                };
                                // State-aware polling cadence
                                let ms = match current_state {
                                    crate::daemon::records::ActivityState::Active => 2000,
                                    crate::daemon::records::ActivityState::Passive => 10000,
                                    crate::daemon::records::ActivityState::Inactive => 20000,
                                    crate::daemon::records::ActivityState::Locked => 30000,
                                };
                                poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);

                                {
                                    let aggregated_apps =
                                        build_app_tree(&agg, app_eph_max, app_eph_min);
                                    let reason = match current_state {
                                        crate::daemon::records::ActivityState::Active => "Active",
                                        crate::daemon::records::ActivityState::Passive => "Passive",
                                        crate::daemon::records::ActivityState::Inactive => {
                                            "Inactive"
                                        }
                                        crate::daemon::records::ActivityState::Locked => "Locked",
                                    };
                                    let next_timeout = Some(
                                        chrono::Utc::now()
                                            + chrono::Duration::milliseconds(ms as i64),
                                    );
                                    let sm = sm_rx.borrow().clone();
                                    crate::daemon::snapshot::publish_basic(
                                        current_state,
                                        current_focus.clone(),
                                        last_transition.clone(),
                                        counts.clone(),
                                        ms as u32,
                                        reason.to_string(),
                                        next_timeout,
                                        crate::daemon::snapshot::StorageInfo {
                                            backlog_count: sm.backlog_count,
                                            last_flush_at: sm.last_flush_at,
                                        },
                                        cfg_summary.clone(),
                                        crate::daemon::snapshot::current_health(),
                                        aggregated_apps.clone(),
                                    );
                                }
                            }
                            break;
                        }
                        Ok(event) => {
                            event_count += 1;
                            debug!("Processing event #{}: {:?}", event_count, event);

                            if let Ok(raw_event) = Self::convert_kronid_to_raw(event) {
                                pending_raw_events.push(raw_event);
                                trace!(
                                    "Added raw event to pending batch (total: {})",
                                    pending_raw_events.len()
                                );
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if !pending_raw_events.is_empty() {
                                debug!(
                                    "Processing {} pending raw events",
                                    pending_raw_events.len()
                                );

                                let raw_events_to_process = std::mem::take(&mut pending_raw_events);

                                // Adapt to EventEnvelope + derive lock boundaries
                                let envelopes = adapter.adapt_batch(&raw_events_to_process);
                                let envelopes_with_lock = lock_deriver.derive(&envelopes);
                                // Dispatch pipeline: signals and derived hints; hints worker persists hints
                                // Dispatch via explicit channels
                                for env in envelopes_with_lock.iter() {
                                    match &env.kind {
                                        EventKind::Hint(_) => {
                                            counts.hints_seen += 1;
                                            if let EventPayload::Focus(fi) = &env.payload {
                                                current_focus = Some(fi.clone());
                                            }
                                            if let EventPayload::State { from, to } =
                                                env.payload.clone()
                                            {
                                                last_transition =
                                                    Some(crate::daemon::snapshot::Transition {
                                                        from,
                                                        to,
                                                        at: env.timestamp,
                                                    });
                                                current_state = to;
                                            }
                                            let _ = hints_tx.send(env.clone());
                                        }
                                        EventKind::Signal(_) => {
                                            counts.signals_seen += 1;
                                            if let EventPayload::Focus(fi) = &env.payload {
                                                current_focus = Some(fi.clone());
                                            }
                                            let _ = storage_tx
                                                .send(StorageCommand::Envelope(env.clone()));
                                            if let Some(h) = state_deriver.on_signal(env) {
                                                counts.hints_seen += 1;
                                                if let EventPayload::State { from, to } =
                                                    h.payload.clone()
                                                {
                                                    last_transition =
                                                        Some(crate::daemon::snapshot::Transition {
                                                            from,
                                                            to,
                                                            at: h.timestamp,
                                                        });
                                                    current_state = to;
                                                }
                                                let _ = hints_tx.send(h);
                                            }
                                        }
                                    }
                                }
                                // Drain records produced by hints worker
                                while let Ok(r) = rec_fb_rx.try_recv() {
                                    let ch = match r.state {
                                        crate::daemon::records::ActivityState::Active => 'A',
                                        crate::daemon::records::ActivityState::Passive => 'P',
                                        crate::daemon::records::ActivityState::Inactive => 'I',
                                        crate::daemon::records::ActivityState::Locked => 'L',
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

                                match compression_engine
                                    .compress_events(raw_events_to_process.clone())
                                {
                                    Ok((processed_events, compact_events)) => {
                                        debug!(
                                            "Compressed into {} compact events",
                                            compact_events.len()
                                        );

                                        if !processed_events.is_empty() {
                                            // Suppress input events during Locked period using derived envelopes
                                            use {EventKind, SignalKind};
                                            let mut suppress: std::collections::HashSet<u64> =
                                                std::collections::HashSet::new();
                                            let mut locked = false;
                                            for env in &envelopes_with_lock {
                                                match &env.kind {
                                                    EventKind::Signal(SignalKind::LockStart) => {
                                                        locked = true
                                                    }
                                                    EventKind::Signal(SignalKind::LockEnd) => {
                                                        locked = false
                                                    }
                                                    EventKind::Signal(
                                                        SignalKind::KeyboardInput
                                                        | SignalKind::MouseInput,
                                                    ) if locked => {
                                                        suppress.insert(env.id);
                                                    }
                                                    _ => {}
                                                }
                                            }
                                            let to_store: Vec<_> = processed_events
                                                .into_iter()
                                                .filter(|ev| match ev {
                                                    crate::daemon::events::RawEvent::KeyboardInput { event_id, .. } |
                                                    crate::daemon::events::RawEvent::MouseInput { event_id, .. } => !suppress.contains(event_id),
                                                    _ => true,
                                                })
                                                .collect();
                                            for ev in to_store {
                                                if let Err(e) =
                                                    storage_tx.send(StorageCommand::RawEvent(ev))
                                                {
                                                    error!("Failed to enqueue raw event: {}", e);
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to compress events: {}", e);
                                    }
                                }
                                let now = chrono::Utc::now();
                                let since = now - retention;
                                while let Some(front) = recent_records.front() {
                                    let end = front.end_time.unwrap_or(now);
                                    if end < since {
                                        recent_records.pop_front();
                                    } else {
                                        break;
                                    }
                                }

                                let agg = {
                                    aggregate_activities_since(
                                        recent_records.make_contiguous(),
                                        since,
                                        now,
                                        eph_max,
                                        eph_min,
                                        max_windows,
                                    )
                                };
                                // State-aware polling cadence
                                let ms = match current_state {
                                    crate::daemon::records::ActivityState::Active => 2000,
                                    crate::daemon::records::ActivityState::Passive => 10000,
                                    crate::daemon::records::ActivityState::Inactive => 20000,
                                    crate::daemon::records::ActivityState::Locked => 30000,
                                };
                                poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);

                                {
                                    let aggregated_apps =
                                        build_app_tree(&agg, app_eph_max, app_eph_min);
                                    let reason = match current_state {
                                        crate::daemon::records::ActivityState::Active => "Active",
                                        crate::daemon::records::ActivityState::Passive => "Passive",
                                        crate::daemon::records::ActivityState::Inactive => {
                                            "Inactive"
                                        }
                                        crate::daemon::records::ActivityState::Locked => "Locked",
                                    };
                                    let next_timeout = Some(
                                        chrono::Utc::now()
                                            + chrono::Duration::milliseconds(ms as i64),
                                    );
                                    let sm = sm_rx.borrow().clone();
                                    crate::daemon::snapshot::publish_basic(
                                        current_state,
                                        current_focus.clone(),
                                        last_transition.clone(),
                                        counts.clone(),
                                        ms as u32,
                                        reason.to_string(),
                                        next_timeout,
                                        crate::daemon::snapshot::StorageInfo {
                                            backlog_count: sm.backlog_count,
                                            last_flush_at: sm.last_flush_at,
                                        },
                                        cfg_summary.clone(),
                                        crate::daemon::snapshot::current_health(),
                                        aggregated_apps.clone(),
                                    );
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            info!("Channel disconnected, exiting");
                            break;
                        }
                    }

                    if let Some(state_hint) = state_deriver.on_tick(chrono::Utc::now()) {
                        counts.hints_seen += 1;
                        if let EventPayload::State { from, to } = state_hint.payload.clone() {
                            last_transition = Some(crate::daemon::snapshot::Transition {
                                from,
                                to,
                                at: state_hint.timestamp,
                            });
                            current_state = to;
                        }
                        let _ = hints_tx.send(state_hint);
                        while let Ok(rec) = rec_fb_rx.try_recv() {
                            let ch = match rec.state {
                                crate::daemon::records::ActivityState::Active => 'A',
                                crate::daemon::records::ActivityState::Passive => 'P',
                                crate::daemon::records::ActivityState::Inactive => 'I',
                                crate::daemon::records::ActivityState::Locked => 'L',
                            };
                            if state_hist.back().copied() != Some(ch) {
                                state_hist.push_back(ch);
                                if state_hist.len() > 10 {
                                    state_hist.pop_front();
                                }
                                let _hist_str: String = state_hist.iter().collect();
                            }
                            recent_records.push_back(rec);
                            counts.records_emitted += 1;
                        }
                        let now = chrono::Utc::now();
                        let since = now - retention;
                        while let Some(front) = recent_records.front() {
                            let end = front.end_time.unwrap_or(now);
                            if end < since {
                                recent_records.pop_front();
                            } else {
                                break;
                            }
                        }

                        let agg = {
                            aggregate_activities_since(
                                recent_records.make_contiguous(),
                                since,
                                now,
                                eph_max,
                                eph_min,
                                max_windows,
                            )
                        };
                        let ms = match current_state {
                            crate::daemon::records::ActivityState::Active => 2000,
                            crate::daemon::records::ActivityState::Passive => 10000,
                            crate::daemon::records::ActivityState::Inactive => 20000,
                            crate::daemon::records::ActivityState::Locked => 30000,
                        };
                        poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);

                        {
                            let aggregated_apps = build_app_tree(&agg, app_eph_max, app_eph_min);
                            let reason = match current_state {
                                crate::daemon::records::ActivityState::Active => "Active",
                                crate::daemon::records::ActivityState::Passive => "Passive",
                                crate::daemon::records::ActivityState::Inactive => "Inactive",
                                crate::daemon::records::ActivityState::Locked => "Locked",
                            };
                            let next_timeout = Some(
                                chrono::Utc::now() + chrono::Duration::milliseconds(ms as i64),
                            );
                            let sm = sm_rx.borrow().clone();
                            crate::daemon::snapshot::publish_basic(
                                current_state,
                                current_focus.clone(),
                                last_transition.clone(),
                                counts.clone(),
                                ms as u32,
                                reason.to_string(),
                                next_timeout,
                                crate::daemon::snapshot::StorageInfo {
                                    backlog_count: sm.backlog_count,
                                    last_flush_at: sm.last_flush_at,
                                },
                                cfg_summary.clone(),
                                crate::daemon::snapshot::current_health(),
                                aggregated_apps.clone(),
                            );
                        }
                    }
                }

                // Shut down pipeline workers and flush remaining records
                drop(hints_tx);
                // Drain any remaining records from worker
                while let Ok(rec) = rec_fb_rx.recv_timeout(Duration::from_millis(200)) {
                    recent_records.push_back(rec);
                    // stop if channel closes soon after
                    if rec_fb_rx.recv_timeout(Duration::from_millis(1)).is_err() {
                        break;
                    }
                }
                let _ = storage_tx.send(StorageCommand::Shutdown);
                let now = chrono::Utc::now();
                let since = now - retention;
                while let Some(front) = recent_records.front() {
                    let end = front.end_time.unwrap_or(now);
                    if end < since {
                        recent_records.pop_front();
                    } else {
                        break;
                    }
                }

                let agg = {
                    aggregate_activities_since(
                        recent_records.make_contiguous(),
                        since,
                        now,
                        eph_max,
                        eph_min,
                        max_windows,
                    )
                };

                let aggregated_apps = build_app_tree(&agg, app_eph_max, app_eph_min);
                let ms = match current_state {
                    crate::daemon::records::ActivityState::Active => 2000,
                    crate::daemon::records::ActivityState::Passive => 10000,
                    crate::daemon::records::ActivityState::Inactive => 20000,
                    crate::daemon::records::ActivityState::Locked => 30000,
                };
                let reason = match current_state {
                    crate::daemon::records::ActivityState::Active => "Active",
                    crate::daemon::records::ActivityState::Passive => "Passive",
                    crate::daemon::records::ActivityState::Inactive => "Inactive",
                    crate::daemon::records::ActivityState::Locked => "Locked",
                };
                let next_timeout =
                    Some(chrono::Utc::now() + chrono::Duration::milliseconds(ms as i64));
                let sm = sm_rx.borrow().clone();
                crate::daemon::snapshot::publish_basic(
                    current_state,
                    current_focus.clone(),
                    last_transition.clone(),
                    counts.clone(),
                    ms as u32,
                    reason.to_string(),
                    next_timeout,
                    crate::daemon::snapshot::StorageInfo {
                        backlog_count: sm.backlog_count,
                        last_flush_at: sm.last_flush_at,
                    },
                    cfg_summary.clone(),
                    crate::daemon::snapshot::current_health(),
                    aggregated_apps,
                );

                // Join worker threads
                let _ = hints_thread.join();
                let _ = storage_thread.join();
                info!("Background thread completed");
            })
        };

        let shutdown_sender = sender.clone();
        let pid_file_cleanup = pid_file.clone();
        ctrlc::set_handler(move || {
            info!("Step S: Ctrl+C received, sending shutdown signal");
            if let Err(e) = shutdown_sender.send(KronicalEvent::Shutdown) {
                error!("Step S: Failed to send shutdown: {}", e);
            }

            info!("Step S: Stopping winshift hook");
            if let Err(e) = winshift::stop_hook() {
                error!("Step S: Failed to stop winshift: {}", e);
            }

            info!("Step S: Cleaning up PID file");
            let _ = std::fs::remove_file(&pid_file_cleanup);
        })?;

        // poll_handle_arc already created earlier; reuse it for the handler
        let handler = KronicalEventHandler::new(
            sender,
            FocusCacheCaps {
                pid_cache_capacity: self.pid_cache_capacity,
                title_cache_capacity: self.title_cache_capacity,
                title_cache_ttl_secs: self.title_cache_ttl_secs,
            },
            std::sync::Arc::clone(&poll_handle_arc),
        );

        info!("Step M1: Setting up UIohook on main thread");
        let uiohook = Uiohook::new(handler.clone());

        if let Err(e) = uiohook.run() {
            error!("Step M1: UIohook failed: {}", e);
            return Err(e.into());
        }
        info!("Step M1: UIohook setup completed");

        info!("Step M2: Setting up Window hook on main thread");
        let window_hook = WindowFocusHook::with_config(
            handler,
            WindowHookConfig {
                monitoring_mode: MonitoringMode::Combined,
                embed_active_info: true,
            },
        );

        info!("Step M2: Starting window hook (this will block main thread)");
        if let Err(e) = window_hook.run() {
            error!("Step M2: Window hook failed: {}", e);
        }
        info!("Step M2: Window hook completed");

        info!("Step C: Cleaning up UIohook");
        if let Err(e) = uiohook.stop() {
            error!("Step C: Failed to stop UIohook: {}", e);
        }

        info!("Step C: Waiting for background thread to finish");
        if let Err(e) = data_thread.join() {
            error!("Step C: Background thread panicked: {:?}", e);
        }

        info!("Step C: Cleaning up socket files");
        if let Some(socket_dir) = pid_file.parent() {
            let grpc_sock = socket_dir.join("kroni.sock");
            let http_sock = socket_dir.join("kroni.http.sock");

            if grpc_sock.exists() {
                if let Err(e) = std::fs::remove_file(&grpc_sock) {
                    error!("Failed to remove gRPC socket: {}", e);
                } else {
                    info!("Removed gRPC socket file");
                }
            }

            if http_sock.exists() {
                if let Err(e) = std::fs::remove_file(&http_sock) {
                    error!("Failed to remove HTTP socket: {}", e);
                } else {
                    info!("Removed HTTP socket file");
                }
            }
        } else {
            error!("PID file has no parent; skipping socket cleanup");
        }

        info!("Step C: Kronical shutdown complete");
        Ok(())
    }
}

impl Clone for KronicalEventHandler {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            focus_wrapper: self.focus_wrapper.clone(),
        }
    }
}
