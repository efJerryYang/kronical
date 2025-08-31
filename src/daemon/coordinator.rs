use crate::daemon::compression::CompressionEngine;
use crate::daemon::event_adapter::EventAdapter;
use crate::daemon::event_deriver::LockDeriver;
use crate::daemon::events::{KeyboardEventData, MouseEventData, MousePosition, WindowFocusInfo};
use crate::daemon::focus_tracker::{FocusCacheCaps, FocusChangeCallback, FocusEventWrapper};
use crate::daemon::records::{aggregate_activities_since, ActivityRecord, RecordProcessor};
use crate::daemon::socket_server::SocketServer;
use crate::storage::StorageBackend;
use anyhow::Result;
use log::{debug, error, info, trace};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use uiohook_rs::{EventHandler, Uiohook, UiohookEvent};
use winshift::{FocusChangeHandler, MonitoringMode, WindowFocusHook, WindowHookConfig};

#[derive(Debug, Clone)]
pub enum ChronicleEvent {
    KeyboardInput,
    MouseInput,
    WindowFocusChange { focus_info: WindowFocusInfo },
    Shutdown,
}

pub struct ChronicleEventHandler {
    sender: mpsc::Sender<ChronicleEvent>,
    focus_wrapper: FocusEventWrapper,
}

impl ChronicleEventHandler {
    fn new(
        sender: mpsc::Sender<ChronicleEvent>,
        caps: FocusCacheCaps,
        poll_handle: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        let callback = Arc::new(FocusCallback {
            sender: sender.clone(),
        });
        let focus_wrapper = FocusEventWrapper::new(
            callback,
            Duration::from_secs(2),
            caps,
            std::sync::Arc::clone(&poll_handle),
        );

        Self {
            sender,
            focus_wrapper,
        }
    }
}

struct FocusCallback {
    sender: mpsc::Sender<ChronicleEvent>,
}

impl FocusChangeCallback for FocusCallback {
    fn on_focus_change(&self, focus_info: WindowFocusInfo) {
        if let Err(e) = self
            .sender
            .send(ChronicleEvent::WindowFocusChange { focus_info })
        {
            error!("Failed to send unified focus change event: {}", e);
        } else {
            info!("Unified focus change event sent successfully");
        }
    }
}

impl EventHandler for ChronicleEventHandler {
    fn handle_event(&self, event: &UiohookEvent) {
        trace!("UIohook event received: {:?}", event);
        let chronicle_event = match event {
            UiohookEvent::Keyboard(_) => {
                trace!("Keyboard event detected");
                ChronicleEvent::KeyboardInput
            }
            UiohookEvent::Mouse(_) => {
                trace!("Mouse event detected");
                ChronicleEvent::MouseInput
            }
            UiohookEvent::Wheel(_) => {
                trace!("Wheel event detected");
                ChronicleEvent::MouseInput
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

        if let Err(e) = self.sender.send(chronicle_event) {
            error!("Failed to send input event: {}", e);
        } else {
            trace!("Event sent to background thread successfully");
        }
    }
}

impl FocusChangeHandler for ChronicleEventHandler {
    fn on_app_change(&self, pid: i32, app_name: String) {
        debug!("App changed: {} (PID: {})", app_name, pid);
        self.focus_wrapper.handle_app_change(pid, app_name);
    }

    fn on_window_change(&self, window_title: String) {
        debug!("Window changed: {}", window_title);
        self.focus_wrapper.handle_window_change(window_title);
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
        }
    }

    fn convert_chronicle_to_raw(event: ChronicleEvent) -> Result<crate::daemon::events::RawEvent> {
        use crate::daemon::events::RawEvent;
        use chrono::Utc;

        let now = Utc::now();
        let event_id = chrono::Utc::now().timestamp_millis() as u64;

        match event {
            ChronicleEvent::KeyboardInput => Ok(RawEvent::KeyboardInput {
                timestamp: now,
                event_id,
                data: KeyboardEventData {
                    key_code: None,
                    key_char: None,
                    modifiers: Vec::new(),
                },
            }),
            ChronicleEvent::MouseInput => Ok(RawEvent::MouseInput {
                timestamp: now,
                event_id,
                data: MouseEventData {
                    position: MousePosition { x: 0, y: 0 },
                    button: None,
                    click_count: None,
                    wheel_delta: None,
                },
            }),
            ChronicleEvent::WindowFocusChange { focus_info } => Ok(RawEvent::WindowFocusChange {
                timestamp: now,
                event_id,
                focus_info,
            }),
            ChronicleEvent::Shutdown => {
                Err(anyhow::anyhow!("Cannot convert Shutdown event to RawEvent"))
            }
        }
    }

    pub fn start_main_thread(
        &self,
        data_store: Box<dyn StorageBackend>,
        pid_file: std::path::PathBuf,
    ) -> Result<()> {
        info!("Step A: Starting Chronicle on MAIN THREAD (required for hooks)");

        let (sender, receiver) = mpsc::channel();

        let socket_path = pid_file.parent().unwrap().join("chronicle.sock");
        let socket_server = Arc::new(SocketServer::new(
            socket_path,
            self.ephemeral_app_max_duration_secs,
            self.ephemeral_app_min_distinct_procs,
        ));

        if let Err(e) = socket_server.start() {
            error!("Failed to start socket server: {}", e);
        } else {
            info!("Socket server started successfully");
        }

        let poll_handle_arc = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(2000));
        let data_thread = {
            let mut compression_engine =
                CompressionEngine::with_focus_cap(self.focus_interner_max_strings);
            let mut record_processor =
                RecordProcessor::with_thresholds(self.active_grace_secs, self.idle_threshold_secs);
            let mut adapter = EventAdapter::new();
            let mut lock_deriver = LockDeriver::new();
            let mut store = data_store;
            let socket_server_clone = Arc::clone(&socket_server);
            let mut pending_raw_events = Vec::new();
            let mut recent_records: std::collections::VecDeque<ActivityRecord> =
                std::collections::VecDeque::new();
            let retention = chrono::Duration::minutes(self.retention_minutes as i64);
            let eph_max = self.ephemeral_max_duration_secs;
            let eph_min = self.ephemeral_min_distinct_ids;
            let max_windows = self.max_windows_per_app;

            let poll_handle_arc2 = std::sync::Arc::clone(&poll_handle_arc);
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
                        let records_vec: Vec<ActivityRecord> =
                            recent_records.iter().cloned().collect();
                        let agg = aggregate_activities_since(
                            &records_vec,
                            since0,
                            now0,
                            eph_max,
                            eph_min,
                            max_windows,
                        );
                        socket_server_clone.update_aggregated_data(agg);
                        info!(
                            "Hydrated {} records from DB since {}",
                            recent_records.len(),
                            since0
                        );
                    }
                    Err(e) => error!("Failed to hydrate records from DB: {}", e),
                }

                loop {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(ChronicleEvent::Shutdown) => {
                            info!(
                                "Shutdown signal received, flushing pending events and finalizing"
                            );
                            if !pending_raw_events.is_empty() {
                                let raw_events_to_process = std::mem::take(&mut pending_raw_events);
                                // Adapt to EventEnvelope + derive lock boundaries (scaffold for future pipeline)
                                let envelopes = adapter.adapt_batch(&raw_events_to_process);
                                let _envelopes_with_lock = lock_deriver.derive(&envelopes);
                                let new_records = record_processor
                                    .process_envelopes(_envelopes_with_lock.clone());
                                if !new_records.is_empty() {
                                    let _ = store.add_records(new_records.clone());
                                    for r in new_records {
                                        recent_records.push_back(r);
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
                                let records_vec: Vec<ActivityRecord> =
                                    recent_records.iter().cloned().collect();
                                let agg = aggregate_activities_since(
                                    &records_vec,
                                    since,
                                    now,
                                    eph_max,
                                    eph_min,
                                    max_windows,
                                );
                                socket_server_clone.update_aggregated_data(agg);
                                // State-aware polling cadence
                                let ms = match record_processor.current_state() {
                                    crate::daemon::records::ActivityState::Active => 2000,
                                    crate::daemon::records::ActivityState::Passive => 10000,
                                    crate::daemon::records::ActivityState::Inactive => 20000,
                                    crate::daemon::records::ActivityState::Locked => 30000,
                                };
                                poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);
                            }
                            break;
                        }
                        Ok(event) => {
                            event_count += 1;
                            debug!("Processing event #{}: {:?}", event_count, event);

                            if let Ok(raw_event) = Self::convert_chronicle_to_raw(event) {
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

                                // Adapt to EventEnvelope + derive lock boundaries (scaffold for future pipeline)
                                let envelopes = adapter.adapt_batch(&raw_events_to_process);
                                let _envelopes_with_lock = lock_deriver.derive(&envelopes);

                                // Persist envelopes for replay diagnostics
                                if let Err(e) = store.add_envelopes(_envelopes_with_lock.clone()) {
                                    error!("Failed to store envelopes: {}", e);
                                }
                                let new_records = record_processor
                                    .process_envelopes(_envelopes_with_lock.clone());
                                if !new_records.is_empty() {
                                    info!("Generated {} new records", new_records.len());
                                    if let Err(e) = store.add_records(new_records.clone()) {
                                        error!("Failed to store records: {}", e);
                                    }

                                    for r in new_records.into_iter() {
                                        recent_records.push_back(r);
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
                                            for env in &_envelopes_with_lock {
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
                                let records_vec: Vec<ActivityRecord> =
                                    recent_records.iter().cloned().collect();
                                let agg = aggregate_activities_since(
                                    &records_vec,
                                    since,
                                    now,
                                    eph_max,
                                    eph_min,
                                    max_windows,
                                );
                                socket_server_clone.update_aggregated_data(agg);
                                // State-aware polling cadence
                                let ms = match record_processor.current_state() {
                                    crate::daemon::records::ActivityState::Active => 2000,
                                    crate::daemon::records::ActivityState::Passive => 10000,
                                    crate::daemon::records::ActivityState::Inactive => 20000,
                                    crate::daemon::records::ActivityState::Locked => 30000,
                                };
                                poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            info!("Channel disconnected, exiting");
                            break;
                        }
                    }

                    if let Some(timeout_record) = record_processor.check_timeouts() {
                        info!(
                            "Timeout detected, storing record: {:?}",
                            timeout_record.state
                        );
                        if let Err(e) = store.add_records(vec![timeout_record.clone()]) {
                            error!("Failed to store timeout record: {}", e);
                        }

                        recent_records.push_back(timeout_record);
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
                        let records_vec: Vec<ActivityRecord> =
                            recent_records.iter().cloned().collect();
                        let agg = aggregate_activities_since(
                            &records_vec,
                            since,
                            now,
                            eph_max,
                            eph_min,
                            max_windows,
                        );
                        socket_server_clone.update_aggregated_data(agg);
                        let ms = match record_processor.current_state() {
                            crate::daemon::records::ActivityState::Active => 2000,
                            crate::daemon::records::ActivityState::Passive => 10000,
                            crate::daemon::records::ActivityState::Inactive => 20000,
                            crate::daemon::records::ActivityState::Locked => 30000,
                        };
                        poll_handle_arc2.store(ms, std::sync::atomic::Ordering::Relaxed);
                    }
                }

                if let Some(final_record) = record_processor.finalize_all() {
                    info!("Storing final record: {:?}", final_record.state);
                    if let Err(e) = store.add_records(vec![final_record.clone()]) {
                        error!("Failed to store final record: {}", e);
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
                    let records_vec: Vec<ActivityRecord> = recent_records.iter().cloned().collect();
                    let agg = aggregate_activities_since(
                        &records_vec,
                        since,
                        now,
                        eph_max,
                        eph_min,
                        max_windows,
                    );
                    socket_server_clone.update_aggregated_data(agg);
                }

                info!("Background thread completed");
            })
        };

        let shutdown_sender = sender.clone();
        let pid_file_cleanup = pid_file.clone();
        ctrlc::set_handler(move || {
            info!("Step S: Ctrl+C received, sending shutdown signal");
            if let Err(e) = shutdown_sender.send(ChronicleEvent::Shutdown) {
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
        let handler = ChronicleEventHandler::new(
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

        info!("Step C: Chronicle shutdown complete");
        Ok(())
    }
}

impl Clone for ChronicleEventHandler {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            focus_wrapper: self.focus_wrapper.clone(),
        }
    }
}
