use crate::compression::CompressionEngine;
use crate::events::{KeyboardEventData, MouseEventData, MousePosition, RawEvent, WindowFocusInfo};
use crate::focus_tracker::{FocusChangeCallback, FocusEventWrapper};
use crate::records::{ActivityRecord, RecordProcessor};
use crate::socket_server::SocketServer;
use crate::storage_backend::StorageBackend;
use anyhow::Result;
use log::{error, info};
use std::sync::{mpsc, Arc, Mutex};
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
    fn new(sender: mpsc::Sender<ChronicleEvent>) -> Self {
        let callback = Arc::new(FocusCallback {
            sender: sender.clone(),
        });
        let focus_wrapper = FocusEventWrapper::new(callback);

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
        info!("Step 1: UIohook event received: {:?}", event);
        let chronicle_event = match event {
            UiohookEvent::Keyboard(_) => {
                info!("Step 2: Keyboard event detected");
                ChronicleEvent::KeyboardInput
            }
            UiohookEvent::Mouse(_) => {
                info!("Step 2: Mouse event detected");
                ChronicleEvent::MouseInput
            }
            UiohookEvent::Wheel(_) => {
                info!("Step 2: Wheel event detected");
                ChronicleEvent::MouseInput
            }
            UiohookEvent::HookEnabled => {
                info!("Step 2: UIohook enabled");
                return;
            }
            UiohookEvent::HookDisabled => {
                info!("Step 2: UIohook disabled");
                return;
            }
        };

        if let Err(e) = self.sender.send(chronicle_event) {
            error!("Step 3: Failed to send input event: {}", e);
        } else {
            info!("Step 3: Event sent to background thread successfully");
        }
    }
}

impl FocusChangeHandler for ChronicleEventHandler {
    fn on_app_change(&self, pid: i32, app_name: String) {
        info!("Step 1: App changed: {} (PID: {})", app_name, pid);
        self.focus_wrapper.handle_app_change(pid, app_name);
    }

    fn on_window_change(&self, window_title: String) {
        info!("Step 1: Window changed: {}", window_title);
        self.focus_wrapper.handle_window_change(window_title);
    }
}

pub struct EventCoordinator;

impl EventCoordinator {
    pub fn new() -> Self {
        Self
    }

    fn convert_chronicle_to_raw(event: ChronicleEvent) -> Result<crate::events::RawEvent> {
        use crate::events::RawEvent;
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
        let socket_server = Arc::new(SocketServer::new(socket_path));

        if let Err(e) = socket_server.start() {
            error!("Failed to start socket server: {}", e);
        } else {
            info!("Socket server started successfully");
        }

        let data_thread = {
            let mut compression_engine = CompressionEngine::new();
            let mut record_processor = RecordProcessor::new();
            let mut store = data_store;
            let socket_server_clone = Arc::clone(&socket_server);
            let mut pending_raw_events = Vec::new();
            let mut recent_records: Vec<ActivityRecord> = Vec::new();

            thread::spawn(move || {
                info!("Step B: Background data processing thread started");
                let mut event_count = 0;

                loop {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(ChronicleEvent::Shutdown) => {
                            info!("Step Z: Shutdown signal received, finalizing");
                            break;
                        }
                        Ok(event) => {
                            event_count += 1;
                            info!("Step 4: Processing event #{}: {:?}", event_count, event);

                            if let Ok(raw_event) = Self::convert_chronicle_to_raw(event) {
                                pending_raw_events.push(raw_event);
                                info!(
                                    "Step 5a: Added raw event to pending batch (total: {})",
                                    pending_raw_events.len()
                                );
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if !pending_raw_events.is_empty() {
                                info!(
                                    "Step 5b: Processing {} pending raw events",
                                    pending_raw_events.len()
                                );

                                let raw_events_to_process = std::mem::take(&mut pending_raw_events);

                                let new_records =
                                    record_processor.process_events(raw_events_to_process.clone());
                                if !new_records.is_empty() {
                                    info!("Step 5c: Generated {} new records", new_records.len());

                                    for record in new_records.iter() {
                                        recent_records.push(record.clone());
                                    }

                                    if let Err(e) = store.add_records(new_records) {
                                        error!("Step 5c: Failed to store records: {}", e);
                                    } else {
                                        info!("Step 5c: Records stored successfully");
                                        socket_server_clone.update_records(
                                            recent_records.iter().cloned().collect(),
                                        );
                                    }
                                }

                                match compression_engine.compress_events(raw_events_to_process) {
                                    Ok((processed_events, compact_events)) => {
                                        info!(
                                            "Step 5b: Compressed into {} compact events",
                                            compact_events.len()
                                        );

                                        if !processed_events.is_empty() {
                                            if let Err(e) = store.add_events(processed_events) {
                                                error!(
                                                    "Step 5b: Failed to store raw events: {}",
                                                    e
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("Step 5b: Failed to compress events: {}", e);
                                    }
                                }
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            info!("Step Z: Channel disconnected, exiting");
                            break;
                        }
                    }

                    // Step 3: Check for timeouts
                    if let Some(timeout_record) = record_processor.check_timeouts() {
                        info!(
                            "Step T: Timeout detected, storing record: {:?}",
                            timeout_record.state
                        );

                        recent_records.push(timeout_record.clone());

                        if let Err(e) = store.add_records(vec![timeout_record.clone()]) {
                            error!("Step T: Failed to store timeout record: {}", e);
                        } else {
                            socket_server_clone
                                .update_records(recent_records.iter().cloned().collect());
                        }
                    }
                }

                // Finalize
                if let Some(final_record) = record_processor.finalize_all() {
                    info!("Step Z: Storing final record: {:?}", final_record.state);
                    if let Err(e) = store.add_records(vec![final_record]) {
                        error!("Step Z: Failed to store final record: {}", e);
                    }
                }

                info!("Step Z: Background thread completed");
            })
        };

        // Setup signal handling
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

        // MAIN THREAD: Setup both hooks with shared event handler
        let handler = ChronicleEventHandler::new(sender);

        info!("Step M1: Setting up UIohook on main thread");
        let uiohook = Uiohook::new(handler.clone());

        // Start UIohook (this sets up but may not block on macOS)
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

        // This will block the main thread with CFRunLoop
        info!("Step M2: Starting window hook (this will block main thread)");
        if let Err(e) = window_hook.run() {
            error!("Step M2: Window hook failed: {}", e);
        }
        info!("Step M2: Window hook completed");

        // Cleanup
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

// Clone implementation for ChronicleEventHandler
impl Clone for ChronicleEventHandler {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            focus_wrapper: self.focus_wrapper.clone(),
        }
    }
}
