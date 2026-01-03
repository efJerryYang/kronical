use crate::daemon::api::ApiHandles;
use crate::daemon::events::WindowFocusInfo;
use crate::daemon::events::{MouseButton, MouseEventKind, WheelAxis, WheelScrollType};
use crate::daemon::pipeline::PipelineHandles;
use crate::daemon::runtime::ThreadRegistry;
use crate::daemon::tracker::SystemTracker;
use crate::daemon::tracker::{FocusCacheCaps, FocusChangeCallback, FocusEventWrapper};
use crate::storage::StorageBackend;
use crate::util::logging::{debug, error, info, trace};
use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, Sender};
use kronical_input::mouse::{
    button_from_hook, event_kind_from_hook, wheel_axis_from_direction, wheel_scroll_type_from_raw,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
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
        button: Option<MouseButton>,
        clicks: u16,
        kind: MouseEventKind,
    },
    MouseWheel {
        clicks: u16,
        x: i32,
        y: i32,
        scroll_type: WheelScrollType,
        amount: u16,
        rotation: i16,
        axis: Option<WheelAxis>,
    },
    WindowFocusChange {
        focus_info: WindowFocusInfo,
    },
    Shutdown,
}

struct CoordinatorShutdownGuard<'a> {
    coordinator: &'a EventCoordinator,
    workspace_dir: &'a Path,
    api_handles: Option<ApiHandles>,
    pipeline_handles: Option<PipelineHandles>,
    uiohook: Option<Uiohook>,
    focus_hook: Option<Arc<WindowFocusHook>>,
}

impl<'a> CoordinatorShutdownGuard<'a> {
    fn new(coordinator: &'a EventCoordinator, workspace_dir: &'a Path) -> Self {
        Self {
            coordinator,
            workspace_dir,
            api_handles: None,
            pipeline_handles: None,
            uiohook: None,
            focus_hook: None,
        }
    }
}

impl Drop for CoordinatorShutdownGuard<'_> {
    fn drop(&mut self) {
        if let Some(uiohook) = self.uiohook.take() {
            let _ = self.coordinator.cleanup_uiohook(uiohook);
        }
        if let Some(focus_hook) = self.focus_hook.take() {
            let _ = self.coordinator.cleanup_focus_hook(focus_hook);
        }
        if let Some(pipeline) = self.pipeline_handles.take() {
            let _ = self.coordinator.cleanup_pipeline(pipeline);
        }
        if let Some(api) = self.api_handles.take() {
            let _ = self.coordinator.cleanup_api_servers(api);
        }
        self.coordinator.cleanup_socket_files(self.workspace_dir);
    }
}

#[derive(Clone)]
pub struct KronicalEventHandler {
    sender: Sender<KronicalEvent>,
    focus_wrapper: Option<FocusEventWrapper>,
}

impl KronicalEventHandler {
    fn new_with_focus(
        sender: Sender<KronicalEvent>,
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
            focus_wrapper: Some(focus_wrapper),
        })
    }

    fn new_input_only(sender: Sender<KronicalEvent>) -> Self {
        Self {
            sender,
            focus_wrapper: None,
        }
    }
}

struct FocusCallback {
    sender: Sender<KronicalEvent>,
}

impl FocusChangeCallback for FocusCallback {
    fn on_focus_change(&self, focus_info: WindowFocusInfo) {
        if let Err(e) = self
            .sender
            .send(KronicalEvent::WindowFocusChange { focus_info })
        {
            error!("Failed to send unified focus change event: {}", e);
        } else {
            trace!("Unified focus change event sent successfully");
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
                trace!("Mouse event detected");
                let kind = event_kind_from_hook(m.event_type);
                let button = button_from_hook(m.button);
                KronicalEvent::MouseInput {
                    x: m.x as i32,
                    y: m.y as i32,
                    button,
                    clicks: m.clicks,
                    kind,
                }
            }
            UiohookEvent::Wheel(w) => {
                trace!("Wheel event detected");
                let scroll_type = wheel_scroll_type_from_raw(w.type_);
                let axis = wheel_axis_from_direction(w.direction);
                KronicalEvent::MouseWheel {
                    clicks: w.clicks,
                    x: w.x as i32,
                    y: w.y as i32,
                    scroll_type,
                    amount: w.amount,
                    rotation: w.rotation,
                    axis,
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
        if let Some(focus_wrapper) = &self.focus_wrapper {
            focus_wrapper.handle_app_change(pid, app_name);
        }
    }

    fn on_window_change(&self, window_title: String) {
        debug!("Window changed: {}", window_title);
        if let Some(focus_wrapper) = &self.focus_wrapper {
            focus_wrapper.handle_window_change(window_title);
        }
    }

    fn on_app_change_info(&self, info: ActiveWindowInfo) {
        debug!(
            "App+Info: {} pid={} wid={}",
            info.app_name, info.process_id, info.window_id
        );
        if let Some(focus_wrapper) = &self.focus_wrapper {
            focus_wrapper.handle_app_change_info(info);
        }
    }

    fn on_window_change_info(&self, info: ActiveWindowInfo) {
        debug!("Win+Info: '{}' wid={}", info.title, info.window_id);
        if let Some(focus_wrapper) = &self.focus_wrapper {
            focus_wrapper.handle_window_change_info(info);
        }
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
    focus_window_coalesce_ms: u64,
    focus_allow_zero_window_id: bool,
    persist_raw_events: bool,
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
        focus_window_coalesce_ms: u64,
        focus_allow_zero_window_id: bool,
        persist_raw_events: bool,
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
            focus_window_coalesce_ms,
            focus_allow_zero_window_id,
            persist_raw_events,
            tracker_enabled,
            tracker_interval_secs,
            tracker_batch_size,
            tracker_db_backend,
            duckdb_memory_limit_mb_tracker,
            threads,
        }
    }
    pub fn initialize(config: &crate::util::config::AppConfig) -> Self {
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
            config.focus_window_coalesce_ms,
            config.focus_allow_zero_window_id,
            config.persist_raw_events,
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

    pub fn spawn_tracker(
        &self,
        workspace_dir: &Path,
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
            return Err(anyhow!("Failed to start system tracker: {}", e));
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

    pub fn spawn_api_servers(
        &self,
        workspace_dir: &Path,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
        thread_registry: ThreadRegistry,
    ) -> Result<ApiHandles> {
        let uds_grpc = crate::util::paths::grpc_uds(workspace_dir);
        let uds_http = crate::util::paths::http_uds(workspace_dir);
        let api_handles = crate::daemon::api::spawn_all(
            uds_grpc,
            uds_http,
            Arc::clone(&snapshot_bus),
            thread_registry,
        )?;
        info!("API servers started successfully");
        Ok(api_handles)
    }

    pub fn spawn_pipeline(
        &self,
        data_store: Box<dyn StorageBackend>,
        event_rx: Receiver<KronicalEvent>,
        poll_handle: Arc<AtomicU64>,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
        thread_registry: ThreadRegistry,
    ) -> Result<PipelineHandles> {
        info!(
            "Raw event persistence {}",
            if self.persist_raw_events {
                "enabled"
            } else {
                "disabled"
            }
        );
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
            persist_raw_events: self.persist_raw_events,
        };

        let pipeline_handles = crate::daemon::pipeline::spawn_pipeline(
            pipeline_config,
            crate::daemon::pipeline::PipelineResources {
                storage: data_store,
                event_rx,
                poll_handle: Arc::clone(&poll_handle),
                snapshot_bus: Arc::clone(&snapshot_bus),
            },
            thread_registry,
        )?;
        info!("Pipeline thread started successfully");
        Ok(pipeline_handles)
    }

    pub fn run_uiohook(&self, sender: Sender<KronicalEvent>) -> Result<Uiohook> {
        info!("Setting up UIohook on main thread");
        let handler = KronicalEventHandler::new_input_only(sender);

        let uiohook = Uiohook::new(handler);
        if let Err(e) = uiohook.run() {
            error!("UIohook failed: {}", e);
            return Err(anyhow!("UIohook failed: {}", e));
        }
        info!("UIohook setup completed");
        Ok(uiohook)
    }

    pub fn build_focus_hook(
        &self,
        sender: Sender<KronicalEvent>,
        poll_handle: Arc<AtomicU64>,
    ) -> Result<Arc<WindowFocusHook>> {
        info!("Setting up focus hook on main thread");
        let handler = KronicalEventHandler::new_with_focus(
            sender,
            FocusCacheCaps {
                pid_cache_capacity: self.pid_cache_capacity,
                title_cache_capacity: self.title_cache_capacity,
                title_cache_ttl_secs: self.title_cache_ttl_secs,
                focus_window_coalesce_ms: self.focus_window_coalesce_ms,
                focus_allow_zero_window_id: self.focus_allow_zero_window_id,
            },
            Arc::clone(&poll_handle),
            self.thread_registry(),
        )?;

        let focus_hook = Arc::new(WindowFocusHook::with_config(
            handler,
            WindowHookConfig {
                monitoring_mode: MonitoringMode::Combined,
                embed_active_info: true,
            },
        ));
        info!("Focus hook setup completed");
        Ok(focus_hook)
    }

    pub fn run_focus_hook(&self, focus_hook: Arc<WindowFocusHook>) -> Result<()> {
        info!("Starting focus hook (this will block main thread)");
        if let Err(e) = focus_hook.run() {
            error!("Focus hook failed: {}", e);
            return Err(anyhow!("Focus hook failed: {}", e));
        }
        info!("Focus hook completed");
        Ok(())
    }

    fn setup_shutdown_handler(
        &self,
        sender: Sender<KronicalEvent>,
        focus_hook: Option<Arc<WindowFocusHook>>,
    ) -> Result<()> {
        let focus_hook = focus_hook.clone();
        ctrlc::set_handler(move || {
            info!("Ctrl+C received, sending shutdown signal");
            if let Err(e) = sender.send(KronicalEvent::Shutdown) {
                error!("Failed to send shutdown: {}", e);
            }
            if let Some(focus_hook) = &focus_hook {
                if let Err(e) = focus_hook.stop() {
                    error!("Failed to stop winshift: {}", e);
                }
            }
        })?;
        Ok(())
    }

    pub fn start_main_thread(
        &self,
        data_store: Box<dyn StorageBackend>,
        workspace_dir: std::path::PathBuf,
        snapshot_bus: Arc<crate::daemon::snapshot::SnapshotBus>,
    ) -> Result<()> {
        info!("Step A: Starting Kronical on MAIN THREAD (required for hooks)");

        let (sender, receiver) = crossbeam_channel::unbounded();
        let thread_registry = self.thread_registry();
        let poll_handle_arc = Arc::new(AtomicU64::new(2000));
        let workspace_path = workspace_dir.as_path();
        let mut shutdown_guard = CoordinatorShutdownGuard::new(self, workspace_path);

        let api_handles = self.spawn_api_servers(
            workspace_path,
            Arc::clone(&snapshot_bus),
            thread_registry.clone(),
        )?;
        shutdown_guard.api_handles = Some(api_handles);

        self.spawn_tracker(workspace_path, thread_registry.clone())?;
        let pipeline_handles = self.spawn_pipeline(
            data_store,
            receiver,
            Arc::clone(&poll_handle_arc),
            Arc::clone(&snapshot_bus),
            thread_registry.clone(),
        )?;
        shutdown_guard.pipeline_handles = Some(pipeline_handles);

        let focus_hook = self.build_focus_hook(sender.clone(), Arc::clone(&poll_handle_arc))?;
        shutdown_guard.focus_hook = Some(Arc::clone(&focus_hook));
        self.setup_shutdown_handler(sender.clone(), Some(Arc::clone(&focus_hook)))?;
        let uiohook = self.run_uiohook(sender.clone())?;
        shutdown_guard.uiohook = Some(uiohook);
        self.run_focus_hook(Arc::clone(&focus_hook))?;

        drop(shutdown_guard);
        info!("Step C: Kronical shutdown complete");
        Ok(())
    }

    fn cleanup_uiohook(&self, uiohook: Uiohook) -> Result<()> {
        info!("Step C: Cleaning up UIohook");
        if let Err(e) = uiohook.stop() {
            error!("Step C: Failed to stop UIohook: {}", e);
        }
        Ok(())
    }

    fn cleanup_focus_hook(&self, focus_hook: Arc<WindowFocusHook>) -> Result<()> {
        info!("Step C: Cleaning up focus hook");
        if let Err(e) = focus_hook.stop() {
            error!("Step C: Failed to stop focus hook: {}", e);
        }
        Ok(())
    }

    fn cleanup_pipeline(&self, pipeline_handles: PipelineHandles) -> Result<()> {
        info!("Step C: Waiting for background thread to finish");
        if let Err(e) = pipeline_handles.join() {
            error!("Step C: Background thread panicked: {:?}", e);
        }
        Ok(())
    }

    fn cleanup_api_servers(&self, api_handles: ApiHandles) -> Result<()> {
        info!("Step C: Cleaning up API servers");
        api_handles.join_all();
        Ok(())
    }

    fn cleanup_socket_files(&self, workspace_dir: &Path) {
        info!("Cleaning up socket files");
        let grpc_sock = crate::util::paths::grpc_uds(workspace_dir);
        let http_sock = crate::util::paths::http_uds(workspace_dir);

        for sock in [grpc_sock, http_sock] {
            if sock.exists() {
                if let Err(e) = std::fs::remove_file(&sock) {
                    error!("Failed to remove socket file {}: {}", sock.display(), e);
                } else {
                    info!("Removed socket file: {}", sock.display());
                }
            }
        }
    }
}
