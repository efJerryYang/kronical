use crate::daemon::events::WindowFocusInfo;
use crate::util::interner::StringInterner;
use crate::util::logging::{debug, error, info, warn};
use crate::util::lru::LruCache;
use anyhow::Result;
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender};
use kronical_common::threading::{ThreadHandle, ThreadRegistry};
use std::borrow::Cow;
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
    pub focus_window_coalesce_ms: u64,
    pub focus_allow_zero_window_id: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconcileSource {
    Pending,
    Cached,
}

enum ReconcileDecision {
    Accept(String, ReconcileSource),
    Hold(String),
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
    let mut poll_lock_active = false;
    let mut poll_lock_since: Option<Instant> = None;
    let mut coalesce_until: Option<Instant> = None;
    let mut pending_window_title: Option<String> = None;
    let mut last_app_window: LruCache<String, (String, u32)> =
        LruCache::with_capacity(caps.title_cache_capacity);
    let mut last_app_before_change: Option<String> = None;
    let mut awaiting_poll_reconcile = false;
    let mut reconcile_active = false;
    let coalesce_ms = caps.focus_window_coalesce_ms;
    let allow_zero_window_id = caps.focus_allow_zero_window_id;
    let can_emit = |state: &FocusState| {
        !state.app_name.is_empty()
            && !state.window_title.is_empty()
            && state.pid > 0
            && state.process_start_time > 0
            && (allow_zero_window_id || state.window_id > 0)
    };

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

    let emit_loginwindow_focus = |interner: &mut StringInterner| {
        let now = Utc::now();
        let focus_info = WindowFocusInfo {
            pid: 0,
            process_start_time: now.timestamp_millis() as u64,
            app_name: interner.intern(LOGINWINDOW_APP),
            window_title: interner.intern("Login"),
            window_id: LOGINWINDOW_WINDOW_ID,
            window_instance_start: now,
            window_position: None,
            window_size: None,
        };
        callback.on_focus_change(focus_info);
    };

    fn escape_log_value(value: &str) -> Cow<'_, str> {
        if !value.contains('\n') && !value.contains('\r') && !value.contains('\t') {
            Cow::Borrowed(value)
        } else {
            let mut escaped = String::with_capacity(value.len());
            for ch in value.chars() {
                match ch {
                    '\n' => escaped.push_str("\\n"),
                    '\r' => escaped.push_str("\\r"),
                    '\t' => escaped.push_str("\\t"),
                    _ => escaped.push(ch),
                }
            }
            Cow::Owned(escaped)
        }
    }

    let schedule_emit = |target: &mut Option<Instant>, delay_ms: u64| {
        let when = Instant::now() + Duration::from_millis(delay_ms);
        *target = Some(match *target {
            Some(existing) if existing < when => when,
            _ => when,
        });
    };

    let record_last_app_window =
        |state: &FocusState, cache: &mut LruCache<String, (String, u32)>| {
            if !state.app_name.is_empty() && !state.window_title.is_empty() {
                cache.put(
                    state.app_name.clone(),
                    (state.window_title.clone(), state.window_id),
                );
            }
        };

    let reconcile_window_title = |app_name: &String,
                                  pending: &str,
                                  cache: &mut LruCache<String, (String, u32)>,
                                  last_app: Option<&String>|
     -> ReconcileDecision {
        if app_name.eq_ignore_ascii_case(LOGINWINDOW_APP) && pending == "Login" {
            return ReconcileDecision::Accept(pending.to_string(), ReconcileSource::Pending);
        }
        if let Some((cached_title, _cached_id)) = cache.get_cloned(app_name) {
            if cached_title == pending {
                return ReconcileDecision::Accept(pending.to_string(), ReconcileSource::Pending);
            }
            if let Some(prev_app) = last_app {
                if let Some((prev_title, _)) = cache.get_cloned(prev_app) {
                    if prev_title == pending {
                        warn!(
                            "Focus worker: reconciling window title for app {} using cached '{}' (pending='{}')",
                            app_name,
                            escape_log_value(&cached_title),
                            escape_log_value(pending)
                        );
                        return ReconcileDecision::Accept(cached_title, ReconcileSource::Cached);
                    }
                }
            }
            return ReconcileDecision::Hold(pending.to_string());
        }
        ReconcileDecision::Hold(pending.to_string())
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
                        if !state.app_name.is_empty() {
                            last_app_before_change = Some(state.app_name.clone());
                        } else {
                            last_app_before_change = None;
                        }
                        if coalesce_until.is_some() {
                            let pending_title = pending_window_title.take();
                            if pending_title.is_some() || !state.window_title.is_empty() {
                                if let Some(title) = pending_title {
                                    match reconcile_window_title(
                                        &state.app_name,
                                        &title,
                                        &mut last_app_window,
                                        last_app_before_change.as_ref(),
                                    ) {
                                        ReconcileDecision::Accept(value, _) => {
                                            state.window_title = value;
                                        }
                                        ReconcileDecision::Hold(value) => {
                                            state.window_title = value;
                                            warn!(
                                                "Focus worker: accepting unverified window title '{}' for app {} after reconcile timeout (flush)",
                                                escape_log_value(&state.window_title),
                                                state.app_name
                                            );
                                        }
                                    }
                                }
                                if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP)
                                    && state.window_id == 0
                                {
                                    state.window_id = LOGINWINDOW_WINDOW_ID;
                                }
                                state.last_window_update = Instant::now();
                                if can_emit(&state) {
                                    let focus_info = state.to_window_focus_info(&mut interner);
                                    info!(
                                        "Focus worker: Emitting consolidated focus change: {} -> {} (reason=app_change_flush)",
                                        focus_info.app_name,
                                        escape_log_value(&focus_info.window_title)
                                    );
                                    callback.on_focus_change(focus_info);
                                    record_last_app_window(&state, &mut last_app_window);
                                }
                            }
                        }
                        let is_login = app_name.eq_ignore_ascii_case(LOGINWINDOW_APP);
                        if !is_login {
                            poll_lock_active = false;
                            poll_lock_since = None;
                        }
                        if coalesce_ms > 0 {
                            coalesce_until =
                                Some(Instant::now() + Duration::from_millis(coalesce_ms));
                        } else {
                            coalesce_until = None;
                        }
                        pending_window_title = None;
                        reconcile_active = true;
                        state.app_name = app_name;
                        state.pid = pid;
                        state.window_title.clear();
                        state.window_id = 0;
                        state.process_start_time = get_process_start_time(pid);
                        if state.process_start_time == 0 {
                            state.process_start_time = Utc::now().timestamp_millis() as u64;
                        }
                        if is_login && state.window_id == 0 {
                            state.window_id = LOGINWINDOW_WINDOW_ID;
                        }
                        state.window_instance_start = Utc::now();
                        state.last_app_update = Instant::now();
                        awaiting_poll_reconcile = true;
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
                        if reconcile_active {
                            if let Some(until) = coalesce_until {
                                if Instant::now() < until {
                                    pending_window_title = Some(window_title);
                                    continue;
                                }
                            }
                            match reconcile_window_title(
                                &state.app_name,
                                &window_title,
                                &mut last_app_window,
                                last_app_before_change.as_ref(),
                            ) {
                                ReconcileDecision::Accept(value, source) => {
                                    if source == ReconcileSource::Cached {
                                        warn!(
                                            "Focus worker: using cached window title '{}' for app {} during reconcile",
                                            escape_log_value(&value),
                                            state.app_name
                                        );
                                    }
                                    state.window_title = value;
                                    reconcile_active = false;
                                }
                                ReconcileDecision::Hold(value) => {
                                    if coalesce_until.is_none() {
                                        state.window_title = value;
                                        warn!(
                                            "Focus worker: accepting unverified window title '{}' for app {} (no coalesce window)",
                                            escape_log_value(&state.window_title),
                                            state.app_name
                                        );
                                        reconcile_active = false;
                                    } else {
                                        pending_window_title = Some(value);
                                        continue;
                                    }
                                }
                            }
                        } else {
                            state.window_title = window_title;
                        }
                        if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP)
                            && state.window_id == 0
                        {
                            state.window_id = LOGINWINDOW_WINDOW_ID;
                        }
                        state.last_window_update = Instant::now();
                        if state.is_complete() {
                            // small settle delay
                            schedule_emit(&mut scheduled_emit_at, 100);
                            reconcile_active = false;
                        } else {
                            warn!(
                                "Focus worker: window change but state incomplete: app={}, pid={}, window={}",
                                state.app_name,
                                state.pid,
                                escape_log_value(&state.window_title)
                            );
                        }
                    }
                }
                FocusMsg::AppChangeInfo(info) => {
                    coalesce_until = None;
                    pending_window_title = None;
                    state.app_name = info.app_name.clone();
                    if !state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP) {
                        poll_lock_active = false;
                        poll_lock_since = None;
                    }
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
                    awaiting_poll_reconcile = false;
                    reconcile_active = false;
                    if state.is_complete() {
                        schedule_emit(&mut scheduled_emit_at, 0);
                    }
                }
                FocusMsg::WindowChangeInfo(info) => {
                    coalesce_until = None;
                    pending_window_title = None;
                    if state.pid != info.process_id {
                        state.pid = info.process_id;
                        state.app_name = info.app_name.clone();
                        if !state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP) {
                            poll_lock_active = false;
                            poll_lock_since = None;
                        }
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
                    awaiting_poll_reconcile = false;
                    reconcile_active = false;
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
                        if poll_lock_active {
                            let elapsed_ms = poll_lock_since
                                .map(|since| since.elapsed().as_millis())
                                .unwrap_or(0);
                            info!(
                                "Focus worker: Polling recovered from lock-like error after {}ms (previous_app='{}' app='{}' title='{}' pid={} wid={})",
                                elapsed_ms,
                                state.app_name,
                                info.app_name,
                                escape_log_value(&info.title),
                                info.process_id,
                                info.window_id
                            );
                            poll_lock_active = false;
                            poll_lock_since = None;
                            state.app_name = info.app_name.clone();
                            state.pid = info.process_id;
                            state.process_start_time = get_process_start_time(info.process_id);
                            if state.process_start_time == 0 {
                                state.process_start_time = Utc::now().timestamp_millis() as u64;
                            }
                            state.window_title = info.title.clone();
                            state.window_id = info.window_id;
                            state.window_instance_start = Utc::now();
                            state.last_app_update = Instant::now();
                            state.last_window_update = state.last_app_update;
                            awaiting_poll_reconcile = false;
                            reconcile_active = false;
                            schedule_emit(&mut scheduled_emit_at, 0);
                        }
                        if awaiting_poll_reconcile && state.app_name == info.app_name {
                            state.pid = info.process_id;
                            state.process_start_time = get_process_start_time(info.process_id);
                            if state.process_start_time == 0 {
                                state.process_start_time = Utc::now().timestamp_millis() as u64;
                            }
                            state.window_title = info.title.clone();
                            state.window_id =
                                if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP)
                                    && info.window_id == 0
                                {
                                    LOGINWINDOW_WINDOW_ID
                                } else {
                                    info.window_id
                                };
                            state.window_instance_start = Utc::now();
                            state.last_window_update = Instant::now();
                            awaiting_poll_reconcile = false;
                            reconcile_active = false;
                            schedule_emit(&mut scheduled_emit_at, 0);
                        }
                        let window_id = info.window_id;
                        let current_title = info.title.clone();
                        if let Some(last_title) = last_titles.get_cloned(&window_id) {
                            if last_title != current_title {
                                debug!(
                                    "Polling detected title change for window {}: '{}' -> '{}'",
                                    window_id,
                                    escape_log_value(&last_title),
                                    escape_log_value(&current_title)
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
                    Err(e) => {
                        let err_text = format!("{:?}", e);
                        let is_lock_error = err_text.contains("No qualifying window found");
                        if is_lock_error {
                            if !poll_lock_active {
                                poll_lock_active = true;
                                poll_lock_since = Some(Instant::now());
                                if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP) {
                                    info!(
                                        "Focus worker: Polling returned lock-like error while app=loginwindow; synthesizing focus (app='{}' title='{}' pid={} wid={})",
                                        state.app_name,
                                        escape_log_value(&state.window_title),
                                        state.pid,
                                        state.window_id
                                    );
                                    emit_loginwindow_focus(&mut interner);
                                } else {
                                    warn!(
                                        "Focus worker: Polling returned lock-like error but app is not loginwindow; skipping lock synthesis (app='{}' title='{}' pid={} wid={})",
                                        state.app_name,
                                        escape_log_value(&state.window_title),
                                        state.pid,
                                        state.window_id
                                    );
                                }
                            } else {
                                debug!("Polling failed while locked: {:?}", e);
                            }
                        } else {
                            error!("Failed to get active window during polling: {:?}", e);
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(until) = coalesce_until {
            if Instant::now() >= until {
                if let Some(title) = pending_window_title.take() {
                    match reconcile_window_title(
                        &state.app_name,
                        &title,
                        &mut last_app_window,
                        last_app_before_change.as_ref(),
                    ) {
                        ReconcileDecision::Accept(value, source) => {
                            if source == ReconcileSource::Cached {
                                warn!(
                                    "Focus worker: using cached window title '{}' for app {} during reconcile",
                                    escape_log_value(&value),
                                    state.app_name
                                );
                            }
                            state.window_title = value;
                        }
                        ReconcileDecision::Hold(value) => {
                            state.window_title = value;
                            warn!(
                                "Focus worker: accepting unverified window title '{}' for app {} after reconcile timeout",
                                escape_log_value(&state.window_title),
                                state.app_name
                            );
                        }
                    }
                    if state.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP) && state.window_id == 0
                    {
                        state.window_id = LOGINWINDOW_WINDOW_ID;
                    }
                    state.last_window_update = Instant::now();
                    awaiting_poll_reconcile = true;
                }
                coalesce_until = None;
                reconcile_active = false;
                if can_emit(&state) {
                    schedule_emit(&mut scheduled_emit_at, 0);
                }
            }
        }

        if let Some(when) = scheduled_emit_at {
            if Instant::now() >= when && can_emit(&state) {
                let focus_info = state.to_window_focus_info(&mut interner);
                info!(
                    "Focus worker: Emitting consolidated focus change: {} -> {}",
                    focus_info.app_name,
                    escape_log_value(&focus_info.window_title)
                );
                callback.on_focus_change(focus_info);
                record_last_app_window(&state, &mut last_app_window);
                scheduled_emit_at = None;
            }
        }
    }

    debug!("Focus worker stopped");
}
