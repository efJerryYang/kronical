use crate::daemon::events::WindowFocusInfo;
use crate::util::lru::LruCache;
use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};
#[cfg(target_os = "macos")]
use winshift::{get_active_window_info, ActiveWindowInfo};

#[derive(Debug, Clone, Copy)]
pub struct FocusCacheCaps {
    pub pid_cache_capacity: usize,
    pub title_cache_capacity: usize,
    pub title_cache_ttl_secs: u64,
}

#[derive(Debug, Clone)]
pub struct FocusState {
    pub app_name: String,
    pub pid: i32,
    pub window_title: String,
    pub window_id: String,
    pub window_instance_start: DateTime<Utc>,
    pub process_start_time: u64,
    pub last_app_update: Instant,
    pub last_window_update: Instant,
}

impl FocusState {
    fn new() -> Self {
        Self {
            app_name: String::new(),
            pid: 0,
            window_title: String::new(),
            window_id: String::new(),
            window_instance_start: Utc::now(),
            process_start_time: 0,
            last_app_update: Instant::now(),
            last_window_update: Instant::now(),
        }
    }

    fn is_complete(&self) -> bool {
        !self.app_name.is_empty()
            && !self.window_title.is_empty()
            && self.pid > 0
            && !self.window_id.is_empty()
            && self.process_start_time > 0
    }

    fn to_window_focus_info(&self) -> WindowFocusInfo {
        WindowFocusInfo {
            pid: self.pid,
            process_start_time: self.process_start_time,
            app_name: self.app_name.clone(),
            window_title: self.window_title.clone(),
            window_id: self.window_id.clone(),
            window_instance_start: self.window_instance_start,
            window_position: None,
            window_size: None,
        }
    }
}

pub trait FocusChangeCallback {
    fn on_focus_change(&self, focus_info: WindowFocusInfo);
}

pub struct FocusEventWrapper {
    current_state: Arc<Mutex<FocusState>>,
    callback: Arc<dyn FocusChangeCallback + Send + Sync>,
    pid_cache: Arc<Mutex<LruCache<i32, u64>>>,
    poll_ms: Arc<AtomicU64>,
    should_stop_polling: Arc<AtomicBool>,
    last_titles: Arc<Mutex<LruCache<String, String>>>, // window_id -> last_title (LRU-capped)
}

impl FocusEventWrapper {
    pub fn new(
        callback: Arc<dyn FocusChangeCallback + Send + Sync>,
        poll_interval: Duration,
        caps: FocusCacheCaps,
        poll_handle: Arc<AtomicU64>,
    ) -> Self {
        let wrapper = Self {
            current_state: Arc::new(Mutex::new(FocusState::new())),
            callback,
            pid_cache: Arc::new(Mutex::new(LruCache::with_capacity(caps.pid_cache_capacity))),
            poll_ms: {
                poll_handle.store(poll_interval.as_millis() as u64, Ordering::Relaxed);
                poll_handle
            },
            should_stop_polling: Arc::new(AtomicBool::new(false)),
            last_titles: Arc::new(Mutex::new(if caps.title_cache_ttl_secs > 0 {
                LruCache::with_capacity_and_ttl(
                    caps.title_cache_capacity,
                    Duration::from_secs(caps.title_cache_ttl_secs),
                )
            } else {
                LruCache::with_capacity(caps.title_cache_capacity)
            })),
        };

        if poll_interval.as_millis() > 0 {
            wrapper.start_polling();
        }

        wrapper
    }

    fn get_process_start_time(&self, pid: i32) -> u64 {
        let mut cache = self.pid_cache.lock().unwrap();
        if let Some(start_time) = cache.get_cloned(&pid) {
            return start_time;
        }

        let mut system = System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[Pid::from(pid as usize)]),
            false,
            sysinfo::ProcessRefreshKind::nothing().with_memory(),
        );
        if let Some(process) = system.process(Pid::from(pid as usize)) {
            let start_time = process.start_time();
            cache.put(pid, start_time);
            start_time
        } else {
            0
        }
    }

    pub fn handle_app_change(&self, pid: i32, app_name: String) {
        info!(
            "FocusEventWrapper: App change to {} (PID: {})",
            app_name, pid
        );

        let mut state = self.current_state.lock().unwrap();
        let now = Instant::now();

        if state.app_name == app_name && state.pid == pid {
            debug!("FocusEventWrapper: Duplicate app change ignored");
            return;
        }

        state.app_name = app_name;
        state.pid = pid;
        state.process_start_time = self.get_process_start_time(pid);
        state.window_instance_start = Utc::now();
        state.last_app_update = now;

        // Defer emission to let window info settle; a title/window callback is expected soon.
        drop(state);
        let wrapper = self.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            wrapper.try_emit_after_delay();
        });
    }

    pub fn handle_window_change(&self, window_title: String) {
        info!("FocusEventWrapper: Window change to '{}'", window_title);

        if window_title.is_empty() {
            info!("FocusEventWrapper: Ignoring empty window title");
            return;
        }

        let mut state = self.current_state.lock().unwrap();
        let now = Instant::now();

        if state.window_title == window_title {
            debug!("FocusEventWrapper: Duplicate window change ignored");
            return;
        }

        state.window_title = window_title;
        state.last_window_update = now;

        // On macOS, window_id and other details are supplied by winshift info callbacks

        if state.is_complete() {
            // Wait a moment for the OS to settle the window title after a switch
            std::thread::sleep(Duration::from_millis(100));
            // On macOS-only support, assume info callback already provided latest details
            let focus_info = state.to_window_focus_info();
            // Emit immediately on first title after app change; future quick duplicates suppressed by processor
            drop(state);
            info!(
                "FocusEventWrapper: Emitting consolidated focus change: {} -> {}",
                focus_info.app_name, focus_info.window_title
            );
            self.callback.on_focus_change(focus_info);
        } else {
            warn!(
                "FocusEventWrapper: Window change but state is incomplete: app={}, pid={}, window={}",
                state.app_name, state.pid, state.window_title
            );
        }
    }

    #[cfg(target_os = "macos")]
    pub fn handle_app_change_info(&self, info: ActiveWindowInfo) {
        // Trust winshift info: no get_active_window needed
        let mut state = self.current_state.lock().unwrap();
        state.app_name = info.app_name.clone();
        state.pid = info.process_id;
        state.process_start_time = self.get_process_start_time(info.process_id);
        state.window_title = info.title.clone();
        state.window_id = format!("{}", info.window_id);
        state.window_instance_start = Utc::now();
        state.last_app_update = Instant::now();
        state.last_window_update = state.last_app_update;
        if state.is_complete() {
            let focus_info = state.to_window_focus_info();
            drop(state);
            info!(
                "FocusEventWrapper: Emitting consolidated focus change (app+info): {} -> {}",
                focus_info.app_name, focus_info.window_title
            );
            self.callback.on_focus_change(focus_info);
        }
    }

    #[cfg(target_os = "macos")]
    pub fn handle_window_change_info(&self, info: ActiveWindowInfo) {
        let mut state = self.current_state.lock().unwrap();
        if state.pid != info.process_id {
            state.pid = info.process_id;
            state.app_name = info.app_name.clone();
            state.process_start_time = self.get_process_start_time(info.process_id);
        }
        state.window_title = info.title.clone();
        state.window_id = format!("{}", info.window_id);
        state.last_window_update = Instant::now();
        if state.is_complete() {
            let focus_info = state.to_window_focus_info();
            drop(state);
            info!(
                "FocusEventWrapper: Emitting consolidated focus change (win+info): {} -> {}",
                focus_info.app_name, focus_info.window_title
            );
            self.callback.on_focus_change(focus_info);
        }
    }

    fn try_emit_after_delay(&self) {
        // Single attempt to emit a focus change after app change, only if window info has settled
        let state = self.current_state.lock().unwrap();
        if state.is_complete() {
            let focus_info = state.to_window_focus_info();
            drop(state);
            info!(
                "FocusEventWrapper: Emitting consolidated focus change from app change: {} -> {}",
                focus_info.app_name, focus_info.window_title
            );
            self.callback.on_focus_change(focus_info);
        }
    }

    fn start_polling(&self) {
        let poll_ms = Arc::clone(&self.poll_ms);
        let should_stop = Arc::clone(&self.should_stop_polling);
        let last_titles = Arc::clone(&self.last_titles);
        let focus_wrapper = self.clone();

        thread::spawn(move || {
            debug!("Starting window title polling with dynamic interval");

            while !should_stop.load(Ordering::Relaxed) {
                let ms = poll_ms.load(Ordering::Relaxed);
                let ms = if ms == 0 { 2000 } else { ms };
                thread::sleep(Duration::from_millis(ms));

                if should_stop.load(Ordering::Relaxed) {
                    break;
                }

                match get_active_window_info() {
                    Ok(info) => {
                        let window_id = format!("{}", info.window_id);
                        let current_title = info.title.clone();

                        let mut titles = last_titles.lock().unwrap();
                        if let Some(last_title) = titles.get_cloned(&window_id) {
                            if last_title != current_title {
                                debug!(
                                    "Polling detected title change for window {}: '{}' -> '{}'",
                                    window_id, last_title, current_title
                                );
                                focus_wrapper.handle_window_change_info(info.clone());
                            }
                        }

                        titles.put(window_id, current_title);
                    }
                    Err(e) => {
                        error!("Failed to get active window during polling: {:?}", e);
                    }
                }
            }

            debug!("Window title polling stopped");
        });
    }

    // dynamic polling via self.poll_ms updated externally
}

impl Clone for FocusEventWrapper {
    fn clone(&self) -> Self {
        Self {
            current_state: Arc::clone(&self.current_state),
            callback: Arc::clone(&self.callback),
            pid_cache: Arc::clone(&self.pid_cache),
            poll_ms: Arc::clone(&self.poll_ms),
            should_stop_polling: Arc::clone(&self.should_stop_polling),
            last_titles: Arc::clone(&self.last_titles),
        }
    }
}
