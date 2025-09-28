use crate::daemon::events::WindowFocusInfo;
use crate::util::interner::StringInterner;
use crate::util::lru::LruCache;
use anyhow::Result;
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender};
use kronical_common::threading::{ThreadHandle, ThreadRegistry};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};

use winshift::{ActiveWindowInfo, get_active_window_info};

const LOGINWINDOW_APP: &str = "loginwindow";
const LOGINWINDOW_WINDOW_ID: u32 = u32::MAX;
const POLL_SUSPEND_MS: u64 = 0;

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
    pub window_id: u32,
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
            window_id: 0,
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
            && self.window_id > 0
            && self.process_start_time > 0
    }

    fn to_window_focus_info(&self, interner: &mut StringInterner) -> WindowFocusInfo {
        WindowFocusInfo {
            pid: self.pid,
            process_start_time: self.process_start_time,
            app_name: interner.intern(&self.app_name),
            window_title: interner.intern(&self.window_title),
            window_id: self.window_id,
            window_instance_start: self.window_instance_start,
            window_position: None,
            window_size: None,
        }
    }
}

pub trait FocusChangeCallback {
    fn on_focus_change(&self, focus_info: WindowFocusInfo);
}

#[derive(Clone)]
pub struct FocusEventWrapper {
    tx: Sender<FocusMsg>,
    #[allow(dead_code)]
    poll_ms: Arc<AtomicU64>,
    should_stop: Arc<AtomicBool>,
    worker_handle: Arc<Mutex<Option<ThreadHandle>>>,
}

enum FocusMsg {
    AppChange { pid: i32, app_name: String },
    WindowChange { window_title: String },
    AppChangeInfo(ActiveWindowInfo),
    WindowChangeInfo(ActiveWindowInfo),
    Stop,
}

impl FocusEventWrapper {
    pub fn new(
        callback: Arc<dyn FocusChangeCallback + Send + Sync>,
        poll_interval: Duration,
        caps: FocusCacheCaps,
        poll_handle: Arc<AtomicU64>,
        threads: ThreadRegistry,
    ) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded::<FocusMsg>();
        poll_handle.store(poll_interval.as_millis() as u64, Ordering::Relaxed);
        let poll_ms = poll_handle;
        let should_stop = Arc::new(AtomicBool::new(false));

        let worker_stop = Arc::clone(&should_stop);
        let worker_poll_ms = Arc::clone(&poll_ms);
        let worker_handle = threads.spawn("focus-worker", move || {
            run_focus_worker(callback, rx, worker_poll_ms, worker_stop, caps);
        })?;

        Ok(Self {
            tx,
            poll_ms,
            should_stop,
            worker_handle: Arc::new(Mutex::new(Some(worker_handle))),
        })
    }

    fn send(&self, msg: FocusMsg) {
        if let Err(e) = self.tx.send(msg) {
            warn!("FocusEventWrapper: failed to send message: {}", e);
        }
    }

    pub fn handle_app_change(&self, pid: i32, app_name: String) {
        info!(
            "FocusEventWrapper: App change to {} (PID: {})",
            app_name, pid
        );
        self.send(FocusMsg::AppChange { pid, app_name });
    }

    pub fn handle_window_change(&self, window_title: String) {
        info!("FocusEventWrapper: Window change to '{}'", window_title);
        self.send(FocusMsg::WindowChange { window_title });
    }

    pub fn handle_app_change_info(&self, info: ActiveWindowInfo) {
        self.send(FocusMsg::AppChangeInfo(info));
    }

    pub fn handle_window_change_info(&self, info: ActiveWindowInfo) {
        self.send(FocusMsg::WindowChangeInfo(info));
    }
}
// dynamic polling via self.poll_ms (read by worker)

impl Drop for FocusEventWrapper {
    fn drop(&mut self) {
        // Best-effort stop
        let _ = self.tx.send(FocusMsg::Stop);
        self.should_stop.store(true, Ordering::Relaxed);
        if let Ok(mut handle) = self.worker_handle.lock() {
            if let Some(thread) = handle.take() {
                if let Err(e) = thread.join() {
                    warn!("Focus worker thread panicked: {:?}", e);
                }
            }
        }
    }
}

fn run_focus_worker(
    callback: Arc<dyn FocusChangeCallback + Send + Sync>,
    rx: Receiver<FocusMsg>,
    poll_ms: Arc<AtomicU64>,
    should_stop: Arc<AtomicBool>,
    caps: FocusCacheCaps,
) {
    debug!("Starting focus worker (channel-driven)");
    let mut state = FocusState::new();
    let mut pid_cache: LruCache<i32, u64> = LruCache::with_capacity(caps.pid_cache_capacity);
    let mut last_titles: LruCache<u32, String> = if caps.title_cache_ttl_secs > 0 {
        LruCache::with_capacity_and_ttl(
            caps.title_cache_capacity,
            Duration::from_secs(caps.title_cache_ttl_secs),
        )
    } else {
        LruCache::with_capacity(caps.title_cache_capacity)
    };
    let mut interner = StringInterner::new();

    // Optional delayed emit scheduling
    let mut scheduled_emit_at: Option<Instant> = None;

    let mut get_process_start_time = |pid: i32| -> u64 {
        if let Some(start_time) = pid_cache.get_cloned(&pid) {
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
            pid_cache.put(pid, start_time);
            start_time
        } else {
            0
        }
    };

    let schedule_emit = |target: &mut Option<Instant>, delay_ms: u64| {
        let when = Instant::now() + Duration::from_millis(delay_ms);
        *target = Some(match *target {
            Some(existing) if existing < when => when,
            _ => when,
        });
    };

    loop {
        if should_stop.load(Ordering::Relaxed) {
            break;
        }
        let poll_value = poll_ms.load(Ordering::Relaxed);
        let timeout_ms = if poll_value == POLL_SUSPEND_MS {
            2000
        } else {
            poll_value.max(1)
        };

        match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
            Ok(msg) => match msg {
                FocusMsg::AppChange { pid, app_name } => {
                    if state.app_name == app_name && state.pid == pid {
                        debug!("Focus worker: duplicate app change ignored");
                    } else {
                        let is_login = app_name.eq_ignore_ascii_case(LOGINWINDOW_APP);
                        state.app_name = app_name;
                        state.pid = pid;
                        state.process_start_time = get_process_start_time(pid);
                        if state.process_start_time == 0 {
                            state.process_start_time = Utc::now().timestamp_millis() as u64;
                        }
                        if is_login && state.window_id == 0 {
                            state.window_id = LOGINWINDOW_WINDOW_ID;
                        }
                        state.window_instance_start = Utc::now();
                        state.last_app_update = Instant::now();
                        // Defer emission to allow window info to arrive
                        if is_login {
                            schedule_emit(&mut scheduled_emit_at, 50);
                        } else {
                            schedule_emit(&mut scheduled_emit_at, 150);
                        }
                    }
                }
                FocusMsg::WindowChange { window_title } => {
                    if window_title.is_empty() {
                        info!("Focus worker: ignoring empty window title");
                    } else if state.window_title == window_title {
                        debug!("Focus worker: duplicate window change ignored");
                    } else {
                        state.window_title = window_title;
                        if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP)
                            && state.window_id == 0
                        {
                            state.window_id = LOGINWINDOW_WINDOW_ID;
                        }
                        state.last_window_update = Instant::now();
                        if state.is_complete() {
                            // small settle delay
                            schedule_emit(&mut scheduled_emit_at, 100);
                        } else {
                            warn!(
                                "Focus worker: window change but state incomplete: app={}, pid={}, window={}",
                                state.app_name, state.pid, state.window_title
                            );
                        }
                    }
                }
                FocusMsg::AppChangeInfo(info) => {
                    state.app_name = info.app_name.clone();
                    state.pid = info.process_id;
                    state.process_start_time = get_process_start_time(info.process_id);
                    if state.process_start_time == 0 {
                        state.process_start_time = Utc::now().timestamp_millis() as u64;
                    }
                    state.window_title = info.title.clone();
                    state.window_id = if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP)
                        && info.window_id == 0
                    {
                        LOGINWINDOW_WINDOW_ID
                    } else {
                        info.window_id
                    };
                    state.window_instance_start = Utc::now();
                    state.last_app_update = Instant::now();
                    state.last_window_update = state.last_app_update;
                    if state.is_complete() {
                        schedule_emit(&mut scheduled_emit_at, 0);
                    }
                }
                FocusMsg::WindowChangeInfo(info) => {
                    if state.pid != info.process_id {
                        state.pid = info.process_id;
                        state.app_name = info.app_name.clone();
                        state.process_start_time = get_process_start_time(info.process_id);
                        if state.process_start_time == 0 {
                            state.process_start_time = Utc::now().timestamp_millis() as u64;
                        }
                    }
                    state.window_title = info.title.clone();
                    state.window_id = if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP)
                        && info.window_id == 0
                    {
                        LOGINWINDOW_WINDOW_ID
                    } else {
                        info.window_id
                    };
                    state.last_window_update = Instant::now();
                    if state.is_complete() {
                        schedule_emit(&mut scheduled_emit_at, 0);
                    }
                }
                FocusMsg::Stop => break,
            },
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if poll_value == POLL_SUSPEND_MS {
                    continue;
                }
                // Polling tick
                match get_active_window_info() {
                    Ok(info) => {
                        let window_id = info.window_id;
                        let current_title = info.title.clone();
                        if let Some(last_title) = last_titles.get_cloned(&window_id) {
                            if last_title != current_title {
                                debug!(
                                    "Polling detected title change for window {}: '{}' -> '{}'",
                                    window_id, last_title, current_title
                                );
                                if state.pid != info.process_id {
                                    state.pid = info.process_id;
                                    state.app_name = info.app_name.clone();
                                    state.process_start_time =
                                        get_process_start_time(info.process_id);
                                }
                                state.window_title = info.title.clone();
                                state.window_id = info.window_id;
                                state.last_window_update = Instant::now();
                                if state.is_complete() {
                                    schedule_emit(&mut scheduled_emit_at, 0);
                                }
                            }
                        }
                        last_titles.put(window_id, current_title);
                    }
                    Err(e) => error!("Failed to get active window during polling: {:?}", e),
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(when) = scheduled_emit_at {
            if Instant::now() >= when && state.is_complete() {
                let focus_info = state.to_window_focus_info(&mut interner);
                info!(
                    "Focus worker: Emitting consolidated focus change: {} -> {}",
                    focus_info.app_name, focus_info.window_title
                );
                callback.on_focus_change(focus_info);
                scheduled_emit_at = None;
            }
        }
    }

    debug!("Focus worker stopped");
}
