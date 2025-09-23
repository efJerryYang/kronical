use crate::daemon::events::MouseEventKind;
use crate::daemon::events::WindowFocusInfo;
use crate::daemon::runtime::ThreadRegistry;
use crate::daemon::tracker::SystemTracker;
use crate::daemon::tracker::{FocusCacheCaps, FocusChangeCallback, FocusEventWrapper};
use crate::storage::StorageBackend;
use anyhow::Result;
use log::{debug, error, info, trace};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, mpsc};
use std::time::Duration;
use uiohook_rs::{EventHandler, Uiohook, UiohookEvent};

use winshift::ActiveWindowInfo;
use winshift::{FocusChangeHandler, MonitoringMode, WindowFocusHook, WindowHookConfig};

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
        threads: ThreadRegistry,
    ) -> Result<Self> {
        let callback = Arc::new(FocusCallback {
            sender: sender.clone(),
        });
        let initial_ms = poll_handle.load(std::sync::atomic::Ordering::Relaxed);
        let focus_wrapper = FocusEventWrapper::new(
            callback,
            Duration::from_millis(initial_ms),
            caps,
            poll_handle,
            threads,
        )?;

        Ok(Self {
            sender,
            focus_wrapper,
        })
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
    tracker_db_backend: crate::util::config::DatabaseBackendConfig,
    duckdb_memory_limit_mb_tracker: u64,
    threads: ThreadRegistry,
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
        tracker_db_backend: crate::util::config::DatabaseBackendConfig,
        duckdb_memory_limit_mb_tracker: u64,
    ) -> Self {
        let threads = ThreadRegistry::with_slots([
            "api-grpc",
            "api-http",
            "system-tracker",
            "pipeline-data",
            "pipeline-storage",
            "pipeline-hints",
            "focus-worker",
        ]);

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
            tracker_db_backend,
            duckdb_memory_limit_mb_tracker,
            threads,
        }
    }
    pub fn from_app_config(config: &crate::util::config::AppConfig) -> Self {
        Self::new(
            config.retention_minutes,
            config.active_grace_secs,
            config.idle_threshold_secs,
            config.ephemeral_max_duration_secs,
            config.ephemeral_min_distinct_ids,
            config.max_windows_per_app,
            config.ephemeral_app_max_duration_secs,
            config.ephemeral_app_min_distinct_procs,
            config.pid_cache_capacity,
            config.title_cache_capacity,
            config.title_cache_ttl_secs,
            config.focus_interner_max_strings,
            config.tracker_enabled,
            config.tracker_interval_secs,
            config.tracker_batch_size,
            config.tracker_db_backend.clone(),
            config.duckdb_memory_limit_mb_tracker,
        )
    }

    pub fn thread_registry(&self) -> ThreadRegistry {
        self.threads.clone()
    }

    pub fn spawn_tracker_thread(
        &self,
        workspace_dir: &std::path::PathBuf,
        thread_registry: ThreadRegistry,
    ) -> Result<()> {
        if !self.tracker_enabled {
            info!("System tracker disabled in configuration");
            return Ok(());
        }

        let current_pid = std::process::id();
        let tracker_db_path =
            crate::util::paths::tracker_db_with_backend(workspace_dir, &self.tracker_db_backend);

        let mut tracker = SystemTracker::new(
            current_pid,
            self.tracker_interval_secs,
            self.tracker_batch_size,
            tracker_db_path.clone(),
            self.tracker_db_backend.clone(),
            self.duckdb_memory_limit_mb_tracker,
        );

        if let Err(e) = tracker.start_with_registry(thread_registry) {
            error!("Failed to start system tracker: {}", e);
            return Err(anyhow::anyhow!("Failed to start system tracker: {}", e));
        }

        info!(
            "System tracker started successfully for PID {}",
            current_pid
        );
        crate::daemon::api::set_system_tracker_db_path(tracker_db_path.clone());
        info!(
            "System tracker DB path set for gRPC API: {:?}",
            tracker_db_path
        );

        Ok(())
    }

    pub fn start_main_thread(
        &self,
        data_store: Box<dyn StorageBackend>,
        workspace_dir: std::path::PathBuf,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
    ) -> Result<()> {
        info!("Step A: Starting Kronical on MAIN THREAD (required for hooks)");

        let (sender, receiver) = mpsc::channel();
        let thread_registry = self.thread_registry();

        // Start API servers (gRPC + HTTP/SSE) via unified API facade.
        let uds_grpc = crate::util::paths::grpc_uds(&workspace_dir);
        let uds_http = crate::util::paths::http_uds(&workspace_dir);
        let api_handles = crate::daemon::api::spawn_all(
            uds_grpc,
            uds_http,
            Arc::clone(&snapshot_bus),
            thread_registry.clone(),
        )?;

        self.spawn_tracker_thread(&workspace_dir, thread_registry.clone())?;

        let poll_handle_arc = Arc::new(AtomicU64::new(2000));
        let pipeline_config = crate::daemon::pipeline::PipelineConfig {
            retention_minutes: self.retention_minutes,
            active_grace_secs: self.active_grace_secs,
            idle_threshold_secs: self.idle_threshold_secs,
            ephemeral_max_duration_secs: self.ephemeral_max_duration_secs,
            ephemeral_min_distinct_ids: self.ephemeral_min_distinct_ids,
            max_windows_per_app: self.max_windows_per_app,
            ephemeral_app_max_duration_secs: self.ephemeral_app_max_duration_secs,
            ephemeral_app_min_distinct_procs: self.ephemeral_app_min_distinct_procs,
            focus_interner_max_strings: self.focus_interner_max_strings,
        };
        let pipeline_handles = crate::daemon::pipeline::spawn_pipeline(
            pipeline_config,
            crate::daemon::pipeline::PipelineResources {
                storage: data_store,
                event_rx: receiver,
                poll_handle: Arc::clone(&poll_handle_arc),
                snapshot_bus: Arc::clone(&snapshot_bus),
            },
            thread_registry.clone(),
        )?;

        let shutdown_sender = sender.clone();
        ctrlc::set_handler(move || {
            info!("Step S: Ctrl+C received, sending shutdown signal");
            if let Err(e) = shutdown_sender.send(KronicalEvent::Shutdown) {
                error!("Step S: Failed to send shutdown: {}", e);
            }

            info!("Step S: Stopping winshift hook");
            if let Err(e) = winshift::stop_hook() {
                error!("Step S: Failed to stop winshift: {}", e);
            }
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
            thread_registry.clone(),
        )?;

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
        if let Err(e) = pipeline_handles.join() {
            error!("Step C: Background thread panicked: {:?}", e);
        }

        info!("Step C: Cleaning up socket files");
        let grpc_sock = crate::util::paths::grpc_uds(&workspace_dir);
        let http_sock = crate::util::paths::http_uds(&workspace_dir);

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

        api_handles.join_all();

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
