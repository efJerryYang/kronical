use crate::daemon::compression::CompressionEngine;
use crate::daemon::duckdb_system_tracker::DuckDbSystemTracker;
use crate::daemon::event_adapter::EventAdapter;
use crate::daemon::event_deriver::LockDeriver;
use crate::daemon::events::{KeyboardEventData, MouseEventData, MousePosition, WindowFocusInfo};
use crate::daemon::focus_tracker::{FocusCacheCaps, FocusChangeCallback, FocusEventWrapper};
use crate::daemon::records::{ActivityRecord, aggregate_activities_since};
use crate::storage::StorageBackend;
use anyhow::Result;
use log::{debug, error, info, trace};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;
use uiohook_rs::{EventHandler, Uiohook, UiohookEvent};
#[cfg(target_os = "macos")]
use winshift::ActiveWindowInfo;
use winshift::{FocusChangeHandler, MonitoringMode, WindowFocusHook, WindowHookConfig};

#[cfg(feature = "kroni-api")]
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
    MouseInput,
    WindowFocusChange { focus_info: WindowFocusInfo },
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
            UiohookEvent::Mouse(_) => {
                trace!("Mouse event detected");
                KronicalEvent::MouseInput
            }
            UiohookEvent::Wheel(_) => {
                trace!("Wheel event detected");
                KronicalEvent::MouseInput
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

    #[cfg(target_os = "macos")]
    fn on_app_change_info(&self, info: ActiveWindowInfo) {
        debug!(
            "App+Info: {} pid={} wid={}",
            info.app_name, info.process_id, info.window_id
        );
        self.focus_wrapper.handle_app_change_info(info);
    }

    #[cfg(target_os = "macos")]
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
            KronicalEvent::MouseInput => Ok(RawEvent::MouseInput {
                timestamp: now,
                event_id,
                data: MouseEventData {
                    position: MousePosition { x: 0, y: 0 },
                    button: None,
                    click_count: None,
                    wheel_delta: None,
                },
            }),
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

    pub fn start_main_thread(
        &self,
        data_store: Box<dyn StorageBackend>,
        pid_file: std::path::PathBuf,
    ) -> Result<()> {
        info!("Step A: Starting Kronical on MAIN THREAD (required for hooks)");

        let (sender, receiver) = mpsc::channel();

        // Legacy monitor socket (to be removed): now using kroni.* names
        // legacy monitor socket removed
        // legacy monitor socket removed

        #[cfg(feature = "kroni-api")]
        {
            // Start Kroni gRPC API over UDS alongside the legacy socket.
            let uds_path = pid_file.parent().unwrap().join("kroni.sock");
            if let Err(e) = crate::daemon::kroni_server::spawn_server(uds_path) {
                error!("Failed to start kroni API server: {}", e);
            } else {
                info!("Kroni API server started successfully");
            }
        }

        #[cfg(feature = "http-admin")]
        {
            let http_sock = pid_file.parent().unwrap().join("kroni.http.sock");
            if let Err(e) = crate::daemon::http_admin::spawn_http_admin(http_sock) {
                error!("Failed to start HTTP admin: {}", e);
            } else {
                info!("HTTP admin server started successfully");
            }
        }

        if self.tracker_enabled {
            let current_pid = std::process::id();
            let tracker_db_path = pid_file.parent().unwrap().join("system-tracker.duckdb");
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

                #[cfg(feature = "kroni-api")]
                {
                    crate::daemon::kroni_server::set_system_tracker_db_path(
                        tracker_db_path.clone(),
                    );
                    info!(
                        "System tracker DB path set for gRPC API: {:?}",
                        tracker_db_path
                    );
                }
            }
        }

        let poll_handle_arc = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(2000));
        let data_thread = {
            let mut compression_engine =
                CompressionEngine::with_focus_cap(self.focus_interner_max_strings);
            let mut adapter = EventAdapter::new();
            let mut lock_deriver = LockDeriver::new();
            let mut store = data_store;
            // legacy socket server removed
            let mut pending_raw_events = Vec::new();
            let mut state_hist: std::collections::VecDeque<char> =
                std::collections::VecDeque::new();
            let mut recent_records: std::collections::VecDeque<ActivityRecord> =
                std::collections::VecDeque::new();
            let retention = chrono::Duration::minutes(self.retention_minutes as i64);
            let eph_max = self.ephemeral_max_duration_secs;
            let eph_min = self.ephemeral_min_distinct_ids;
            let max_windows = self.max_windows_per_app;
            #[cfg(feature = "kroni-api")]
            let app_eph_max = self.ephemeral_app_max_duration_secs;
            #[cfg(feature = "kroni-api")]
            let app_eph_min = self.ephemeral_app_min_distinct_procs;
            #[cfg(feature = "kroni-api")]
            let mut counts = crate::daemon::snapshot::Counts {
                signals_seen: 0,
                hints_seen: 0,
                records_emitted: 0,
            };
            #[cfg(feature = "kroni-api")]
            let mut last_transition: Option<crate::daemon::snapshot::Transition> = None;

            let poll_handle_arc2 = std::sync::Arc::clone(&poll_handle_arc);
            let ags = self.active_grace_secs;
            let its = self.idle_threshold_secs;
            #[cfg(feature = "kroni-api")]
            let cfg_summary = crate::daemon::snapshot::ConfigSummary {
                active_grace_secs: self.active_grace_secs,
                idle_threshold_secs: self.idle_threshold_secs,
                retention_minutes: self.retention_minutes,
                ephemeral_max_duration_secs: self.ephemeral_max_duration_secs,
                ephemeral_min_distinct_ids: self.ephemeral_min_distinct_ids,
                ephemeral_app_max_duration_secs: self.ephemeral_app_max_duration_secs,
                ephemeral_app_min_distinct_procs: self.ephemeral_app_min_distinct_procs,
            };

            #[cfg(feature = "kroni-api")]
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

                // Initialize new state + record components
                let mut state_deriver =
                    crate::daemon::event_deriver::StateDeriver::new(now0, ags as i64, its as i64);
                let mut record_builder = crate::daemon::records::RecordBuilder::new(
                    crate::daemon::records::ActivityState::Inactive,
                );

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
                                // Persist all envelopes
                                let _ = store.add_envelopes(envelopes_with_lock.clone());
                                // Feed pipeline (hint-first ordering per timestamp)
                                for env in envelopes_with_lock.iter().filter(|e| {
                                    matches!(e.kind, crate::daemon::event_model::EventKind::Hint(_))
                                }) {
                                    if let Some(rec) = record_builder.on_hint(env) {
                                        let _ = store.add_records(vec![rec.clone()]);
                                        recent_records.push_back(rec);
                                    }
                                    #[cfg(feature = "kroni-api")]
                                    {
                                        counts.hints_seen += 1;
                                        if let crate::daemon::event_model::EventPayload::State {
                                            from,
                                            to,
                                        } = &env.payload
                                        {
                                            last_transition =
                                                Some(crate::daemon::snapshot::Transition {
                                                    from: *from,
                                                    to: *to,
                                                    at: env.timestamp,
                                                });
                                        }
                                    }
                                }
                                for env in envelopes_with_lock.iter().filter(|e| {
                                    matches!(
                                        e.kind,
                                        crate::daemon::event_model::EventKind::Signal(_)
                                    )
                                }) {
                                    #[cfg(feature = "kroni-api")]
                                    {
                                        counts.signals_seen += 1;
                                    }
                                    if let Some(h) = state_deriver.on_signal(env) {
                                        let _ = store.add_envelopes(vec![h.clone()]);
                                        if let Some(rec) = record_builder.on_hint(&h) {
                                            let _ = store.add_records(vec![rec.clone()]);
                                            recent_records.push_back(rec);
                                        }
                                        #[cfg(feature = "kroni-api")]
                                        {
                                            counts.hints_seen += 1;
                                            if let crate::daemon::event_model::EventPayload::State { from, to } = &h.payload {
                                                last_transition = Some(crate::daemon::snapshot::Transition { from: *from, to: *to, at: h.timestamp });
                                            }
                                        }
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
                                #[cfg(feature = "kroni-api")]
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
                                let ms = match record_builder.current_state() {
                                    crate::daemon::records::ActivityState::Active => 2000,
                                    crate::daemon::records::ActivityState::Passive => 10000,
                                    crate::daemon::records::ActivityState::Inactive => 20000,
                                    crate::daemon::records::ActivityState::Locked => 30000,
                                };
                                poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);
                                #[cfg(feature = "kroni-api")]
                                {
                                    let aggregated_apps =
                                        build_app_tree(&agg, app_eph_max, app_eph_min);
                                    let reason = match record_builder.current_state() {
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
                                    let sm = crate::storage::sqlite3::storage_metrics();
                                    crate::daemon::snapshot::publish_basic(
                                        record_builder.current_state(),
                                        record_builder.current_focus(),
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
                                        crate::daemon::snapshot::ReplayInfo {
                                            mode: "live".into(),
                                            position: None,
                                        },
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

                                // Persist envelopes for replay diagnostics
                                if let Err(e) = store.add_envelopes(envelopes_with_lock.clone()) {
                                    error!("Failed to store envelopes: {}", e);
                                    crate::daemon::snapshot::push_health(format!(
                                        "store envelopes error: {}",
                                        e
                                    ));
                                }

                                // New pipeline: feed signals to StateDeriver, hints to RecordBuilder
                                let mut new_records = Vec::new();
                                // Hints first
                                for env in envelopes_with_lock.iter().filter(|e| {
                                    matches!(e.kind, crate::daemon::event_model::EventKind::Hint(_))
                                }) {
                                    if let Some(rec) = record_builder.on_hint(env) {
                                        new_records.push(rec);
                                    }
                                    #[cfg(feature = "kroni-api")]
                                    {
                                        counts.hints_seen += 1;
                                        if let crate::daemon::event_model::EventPayload::State {
                                            from,
                                            to,
                                        } = &env.payload
                                        {
                                            last_transition =
                                                Some(crate::daemon::snapshot::Transition {
                                                    from: *from,
                                                    to: *to,
                                                    at: env.timestamp,
                                                });
                                        }
                                    }
                                }
                                // Then signals (which may derive state-change hints)
                                for env in envelopes_with_lock.iter().filter(|e| {
                                    matches!(
                                        e.kind,
                                        crate::daemon::event_model::EventKind::Signal(_)
                                    )
                                }) {
                                    #[cfg(feature = "kroni-api")]
                                    {
                                        counts.signals_seen += 1;
                                    }
                                    if let Some(h) = state_deriver.on_signal(env) {
                                        if let Err(e) = store.add_envelopes(vec![h.clone()]) {
                                            error!("Failed to store derived state hint: {}", e);
                                            crate::daemon::snapshot::push_health(format!(
                                                "store derived hint error: {}",
                                                e
                                            ));
                                        }
                                        if let Some(rec) = record_builder.on_hint(&h) {
                                            new_records.push(rec);
                                        }
                                        #[cfg(feature = "kroni-api")]
                                        {
                                            counts.hints_seen += 1;
                                            if let crate::daemon::event_model::EventPayload::State { from, to } = &h.payload {
                                                last_transition = Some(crate::daemon::snapshot::Transition { from: *from, to: *to, at: h.timestamp });
                                            }
                                        }
                                    }
                                }
                                if !new_records.is_empty() {
                                    info!("Generated {} new records", new_records.len());
                                    for r in &new_records {
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
                                    }
                                    let _hist_str: String = state_hist.iter().collect();
                                    if let Err(e) = store.add_records(new_records.clone()) {
                                        error!("Failed to store records: {}", e);
                                        crate::daemon::snapshot::push_health(format!(
                                            "store records error: {}",
                                            e
                                        ));
                                    }
                                    for r in new_records.into_iter() {
                                        recent_records.push_back(r);
                                        #[cfg(feature = "kroni-api")]
                                        {
                                            counts.records_emitted += 1;
                                        }
                                    }
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
                                            use crate::daemon::event_model::{
                                                EventKind, SignalKind,
                                            };
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
                                            if let Err(e) = store.add_events(to_store) {
                                                error!("Failed to store raw events: {}", e);
                                                crate::daemon::snapshot::push_health(format!(
                                                    "store raw events error: {}",
                                                    e
                                                ));
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
                                #[cfg(feature = "kroni-api")]
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
                                let ms = match record_builder.current_state() {
                                    crate::daemon::records::ActivityState::Active => 2000,
                                    crate::daemon::records::ActivityState::Passive => 10000,
                                    crate::daemon::records::ActivityState::Inactive => 20000,
                                    crate::daemon::records::ActivityState::Locked => 30000,
                                };
                                poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);
                                #[cfg(feature = "kroni-api")]
                                {
                                    let aggregated_apps =
                                        build_app_tree(&agg, app_eph_max, app_eph_min);
                                    let reason = match record_builder.current_state() {
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
                                    let sm = crate::storage::sqlite3::storage_metrics();
                                    crate::daemon::snapshot::publish_basic(
                                        record_builder.current_state(),
                                        record_builder.current_focus(),
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
                                        crate::daemon::snapshot::ReplayInfo {
                                            mode: "live".into(),
                                            position: None,
                                        },
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
                        if let Err(e) = store.add_envelopes(vec![state_hint.clone()]) {
                            error!("Failed to store tick-derived state hint: {}", e);
                            crate::daemon::snapshot::push_health(format!(
                                "store tick hint error: {}",
                                e
                            ));
                        }
                        if let Some(rec) = record_builder.on_hint(&state_hint) {
                            if let Err(e) = store.add_records(vec![rec.clone()]) {
                                error!("Failed to store timeout record: {}", e);
                                crate::daemon::snapshot::push_health(format!(
                                    "store timeout record error: {}",
                                    e
                                ));
                            }
                            recent_records.push_back(rec.clone());
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
                        }
                        #[cfg(feature = "kroni-api")]
                        {
                            counts.hints_seen += 1;
                            if let crate::daemon::event_model::EventPayload::State { from, to } =
                                &state_hint.payload
                            {
                                last_transition = Some(crate::daemon::snapshot::Transition {
                                    from: *from,
                                    to: *to,
                                    at: state_hint.timestamp,
                                });
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
                        #[cfg(feature = "kroni-api")]
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
                        let ms = match record_builder.current_state() {
                            crate::daemon::records::ActivityState::Active => 2000,
                            crate::daemon::records::ActivityState::Passive => 10000,
                            crate::daemon::records::ActivityState::Inactive => 20000,
                            crate::daemon::records::ActivityState::Locked => 30000,
                        };
                        poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);
                        #[cfg(feature = "kroni-api")]
                        {
                            let aggregated_apps = build_app_tree(&agg, app_eph_max, app_eph_min);
                            let reason = match record_builder.current_state() {
                                crate::daemon::records::ActivityState::Active => "Active",
                                crate::daemon::records::ActivityState::Passive => "Passive",
                                crate::daemon::records::ActivityState::Inactive => "Inactive",
                                crate::daemon::records::ActivityState::Locked => "Locked",
                            };
                            let next_timeout = Some(
                                chrono::Utc::now() + chrono::Duration::milliseconds(ms as i64),
                            );
                            let sm = crate::storage::sqlite3::storage_metrics();
                            crate::daemon::snapshot::publish_basic(
                                record_builder.current_state(),
                                record_builder.current_focus(),
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
                                crate::daemon::snapshot::ReplayInfo {
                                    mode: "live".into(),
                                    position: None,
                                },
                                crate::daemon::snapshot::current_health(),
                                aggregated_apps.clone(),
                            );
                        }
                    }
                }

                if let Some(final_record) = record_builder.finalize_all() {
                    info!("Storing final record: {:?}", final_record.state);
                    if let Err(e) = store.add_records(vec![final_record.clone()]) {
                        error!("Failed to store final record: {}", e);
                        crate::daemon::snapshot::push_health(format!(
                            "store final record error: {}",
                            e
                        ));
                    }

                    recent_records.push_back(final_record);
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
                    #[cfg(feature = "kroni-api")]
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
                    #[cfg(feature = "kroni-api")]
                    {
                        let aggregated_apps = build_app_tree(&agg, app_eph_max, app_eph_min);
                        let ms = match record_builder.current_state() {
                            crate::daemon::records::ActivityState::Active => 2000,
                            crate::daemon::records::ActivityState::Passive => 10000,
                            crate::daemon::records::ActivityState::Inactive => 20000,
                            crate::daemon::records::ActivityState::Locked => 30000,
                        };
                        let reason = match record_builder.current_state() {
                            crate::daemon::records::ActivityState::Active => "Active",
                            crate::daemon::records::ActivityState::Passive => "Passive",
                            crate::daemon::records::ActivityState::Inactive => "Inactive",
                            crate::daemon::records::ActivityState::Locked => "Locked",
                        };
                        let next_timeout =
                            Some(chrono::Utc::now() + chrono::Duration::milliseconds(ms as i64));
                        let sm = crate::storage::sqlite3::storage_metrics();
                        crate::daemon::snapshot::publish_basic(
                            record_builder.current_state(),
                            record_builder.current_focus(),
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
                            crate::daemon::snapshot::ReplayInfo {
                                mode: "live".into(),
                                position: None,
                            },
                            crate::daemon::snapshot::current_health(),
                            aggregated_apps,
                        );
                    }
                }

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

            #[cfg(target_os = "macos")]
            {
                info!("Step S: Stopping winshift hook");
                if let Err(e) = winshift::stop_hook() {
                    error!("Step S: Failed to stop winshift: {}", e);
                }
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
        let socket_dir = pid_file.parent().unwrap();
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
