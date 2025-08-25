use crate::events::WindowFocusInfo;
use active_win_pos_rs::get_active_window;
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

#[derive(Debug, Clone)]
pub struct FocusState {
    pub app_name: String,
    pub pid: i32,
    pub window_title: String,
    pub window_id: String,
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
    pid_cache: Arc<Mutex<HashMap<i32, u64>>>,
    poll_interval: Duration,
    should_stop_polling: Arc<AtomicBool>,
    last_titles: Arc<Mutex<HashMap<String, String>>>, // window_id -> last_title
}

impl FocusEventWrapper {
    pub fn new(
        callback: Arc<dyn FocusChangeCallback + Send + Sync>,
        poll_interval: Duration,
    ) -> Self {
        let wrapper = Self {
            current_state: Arc::new(Mutex::new(FocusState::new())),
            callback,
            pid_cache: Arc::new(Mutex::new(HashMap::new())),
            poll_interval,
            should_stop_polling: Arc::new(AtomicBool::new(false)),
            last_titles: Arc::new(Mutex::new(HashMap::new())),
        };

        // Start polling thread if interval is positive
        if poll_interval.as_millis() > 0 {
            wrapper.start_polling();
        }

        wrapper
    }

    fn get_process_start_time(&self, pid: i32) -> u64 {
        let mut cache = self.pid_cache.lock().unwrap();
        if let Some(start_time) = cache.get(&pid) {
            return *start_time;
        }

        let mut system = System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[Pid::from(pid as usize)]),
            false,
            sysinfo::ProcessRefreshKind::nothing().with_memory(),
        );
        if let Some(process) = system.process(Pid::from(pid as usize)) {
            let start_time = process.start_time();
            cache.insert(pid, start_time);
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
            info!("FocusEventWrapper: Duplicate app change ignored");
            return;
        }

        state.app_name = app_name;
        state.pid = pid;
        state.process_start_time = self.get_process_start_time(pid);
        state.last_app_update = now;

        match get_active_window() {
            Ok(active_window) => {
                if active_window.process_id as i32 == pid {
                    state.window_title = active_window.title;
                    state.window_id = active_window.window_id;
                    state.last_window_update = now;

                    if state.is_complete() {
                        // Wait a moment for the OS to settle the window title after a switch
                        std::thread::sleep(Duration::from_millis(100));
                        // Final refresh to ensure latest data
                        if let Ok(aw) = get_active_window() {
                            state.window_title = aw.title;
                            state.window_id = aw.window_id;
                        }
                        let focus_info = state.to_window_focus_info();
                        info!(
                            "FocusEventWrapper: Emitting consolidated focus change from app change: {} -> {}",
                            focus_info.app_name, focus_info.window_title
                        );
                        drop(state);
                        self.callback.on_focus_change(focus_info);
                    }
                } else {
                    warn!(
                        "FocusEventWrapper: Active window PID ({}) does not match app change PID ({}). Clearing window info.",
                        active_window.process_id, pid
                    );
                    state.window_title = String::new();
                    state.window_id = String::new();
                }
            }
            Err(e) => {
                error!("FocusEventWrapper: Could not get active window: {:?}", e);
                state.window_title = String::new();
                state.window_id = String::new();
            }
        }
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
            info!("FocusEventWrapper: Duplicate window change ignored");
            return;
        }

        state.window_title = window_title;
        state.last_window_update = now;

        // Also update window_id, as it might have changed (e.g., new tab in browser)
        match get_active_window() {
            Ok(active_window) => {
                if active_window.process_id as i32 == state.pid {
                    state.window_id = active_window.window_id;
                }
            }
            Err(e) => {
                error!(
                    "FocusEventWrapper: Could not get active window on title change: {:?}",
                    e
                );
            }
        }

        if state.is_complete() {
            // Wait a moment for the OS to settle the window title after a switch
            std::thread::sleep(Duration::from_millis(100));
            // Final refresh to ensure latest data
            if let Ok(aw) = get_active_window() {
                state.window_title = aw.title;
                state.window_id = aw.window_id;
            }
            let focus_info = state.to_window_focus_info();
            info!(
                "FocusEventWrapper: Emitting consolidated focus change: {} -> {}",
                focus_info.app_name, focus_info.window_title
            );

            drop(state);
            self.callback.on_focus_change(focus_info);
        } else {
            warn!(
                "FocusEventWrapper: Window change but state is incomplete: app={}, pid={}, window={}",
                state.app_name, state.pid, state.window_title
            );
        }
    }

    pub fn get_current_state(&self) -> Option<WindowFocusInfo> {
        let state = self.current_state.lock().unwrap();
        if state.is_complete() {
            Some(state.to_window_focus_info())
        } else {
            None
        }
    }

    // Force emit current state if complete (useful for initialization)
    pub fn force_emit_current(&self) {
        let state = self.current_state.lock().unwrap();
        if state.is_complete() {
            let focus_info = state.to_window_focus_info();
            info!(
                "FocusEventWrapper: Force emitting current focus: {} -> {}",
                focus_info.app_name, focus_info.window_title
            );

            // Release the lock before calling the callback
            drop(state);
            self.callback.on_focus_change(focus_info);
        }
    }

    fn start_polling(&self) {
        let poll_interval = self.poll_interval;
        let should_stop = Arc::clone(&self.should_stop_polling);
        let last_titles = Arc::clone(&self.last_titles);
        let focus_wrapper = self.clone();

        thread::spawn(move || {
            info!(
                "Starting window title polling with interval: {:?}",
                poll_interval
            );

            while !should_stop.load(Ordering::Relaxed) {
                thread::sleep(poll_interval);

                if should_stop.load(Ordering::Relaxed) {
                    break;
                }

                match get_active_window() {
                    Ok(active_window) => {
                        let window_id = active_window.window_id;
                        let current_title = active_window.title;

                        let mut titles = last_titles.lock().unwrap();
                        if let Some(last_title) = titles.get(&window_id) {
                            if last_title != &current_title {
                                info!(
                                    "Polling detected title change for window {}: '{}' -> '{}'",
                                    window_id, last_title, current_title
                                );
                                focus_wrapper.handle_window_change(current_title.clone());
                            }
                        }

                        // Update stored title
                        titles.insert(window_id, current_title);
                    }
                    Err(e) => {
                        error!("Failed to get active window during polling: {:?}", e);
                    }
                }
            }

            info!("Window title polling stopped");
        });
    }

    pub fn stop_polling(&self) {
        self.should_stop_polling.store(true, Ordering::Relaxed);
    }
}

impl Clone for FocusEventWrapper {
    fn clone(&self) -> Self {
        Self {
            current_state: Arc::clone(&self.current_state),
            callback: Arc::clone(&self.callback),
            pid_cache: Arc::clone(&self.pid_cache),
            poll_interval: self.poll_interval,
            should_stop_polling: Arc::clone(&self.should_stop_polling),
            last_titles: Arc::clone(&self.last_titles),
        }
    }
}
