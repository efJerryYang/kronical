use crate::daemon::events::{RawEvent, WindowFocusInfo};
use crate::util::maps::{HashMap, HashSet};
use chrono::{DateTime, Utc};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use size_of::SizeOf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, SizeOf)]
pub enum ActivityState {
    Active,
    Passive,
    Inactive,
    Locked,
}

#[derive(Debug, Clone, Serialize, Deserialize, SizeOf)]
pub struct ActivityRecord {
    pub record_id: u64,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub state: ActivityState,
    pub focus_info: Option<WindowFocusInfo>,
    pub event_count: u32,
    pub triggering_events: Vec<u64>,
}

#[derive(Debug, Clone)]
pub struct WindowActivity {
    pub window_id: String,
    pub window_title: String,
    pub duration_seconds: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub record_count: u32,
}

#[derive(Debug, Clone)]
pub struct AggregatedActivity {
    pub app_name: String,
    pub pid: i32,
    pub process_start_time: u64,
    pub windows: HashMap<u32, WindowActivity>,
    pub ephemeral_groups: HashMap<String, EphemeralGroup>,
    pub total_duration_seconds: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct EphemeralGroup {
    pub title_key: String,
    pub distinct_ids: HashSet<u32>,
    pub occurrence_count: u32,
    pub total_duration_seconds: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
}

pub struct RecordProcessor {
    current_state: ActivityState,
    last_activity: Instant,
    last_keyboard_activity: Instant,
    active_timeout: Duration,
    passive_timeout: Duration,
    current_record: Option<ActivityRecord>,
    current_focus: Option<WindowFocusInfo>,
    next_record_id: u64,
}

impl RecordProcessor {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            current_state: ActivityState::Inactive,
            last_activity: now,
            last_keyboard_activity: now,
            active_timeout: Duration::from_secs(30),
            passive_timeout: Duration::from_secs(60),
            current_record: None,
            current_focus: None,
            next_record_id: 1,
        }
    }

    pub fn process_events(&mut self, events: Vec<RawEvent>) -> Vec<ActivityRecord> {
        let mut new_records = Vec::new();

        // Check for periodic completion of long-running records (every 5 minutes)
        if let Some(ref record) = self.current_record {
            let record_duration = Utc::now().signed_duration_since(record.start_time);
            if record_duration > chrono::Duration::minutes(5) {
                info!(
                    "Auto-completing long-running record #{} after {} minutes",
                    record.record_id,
                    record_duration.num_minutes()
                );
                if let Some(completed) = self.force_complete_current_record() {
                    new_records.push(completed);
                }
            }
        }

        for event in events {
            debug!(
                "Processing event #{}: {} at {}",
                event.event_id(),
                event.event_type(),
                event.timestamp()
            );

            let completed_record = match &event {
                RawEvent::KeyboardInput { event_id, .. } => self.handle_keyboard_event(*event_id),
                RawEvent::MouseInput { event_id, .. } => self.handle_mouse_event(*event_id),
                RawEvent::WindowFocusChange {
                    event_id,
                    focus_info,
                    ..
                } => self.handle_focus_change(*event_id, focus_info.clone()),
            };

            if let Some(record) = completed_record {
                info!(
                    "Record #{}: {} completed after {} events from {} to {}",
                    record.record_id,
                    format!("{:?}", record.state),
                    record.event_count,
                    record.start_time.format("%H:%M:%S%.3f"),
                    record
                        .end_time
                        .map(|t| t.format("%H:%M:%S%.3f").to_string())
                        .unwrap_or_else(|| "ongoing".to_string())
                );
                new_records.push(record);
            }
        }

        new_records
    }

    fn force_complete_current_record(&mut self) -> Option<ActivityRecord> {
        if let Some(current) = self.current_record.take() {
            let mut completed = current;
            completed.end_time = Some(Utc::now());
            Some(completed)
        } else {
            None
        }
    }

    fn start_new_record(&mut self, focus_info: Option<WindowFocusInfo>) {
        let now_utc = Utc::now();
        self.current_record = Some(ActivityRecord {
            record_id: self.next_record_id,
            start_time: now_utc,
            end_time: None,
            state: ActivityState::Passive, // Start with passive state
            focus_info,
            event_count: 0,
            triggering_events: Vec::new(),
        });
        self.next_record_id += 1;
        info!(
            "Started new record #{}",
            self.current_record.as_ref().unwrap().record_id
        );
    }

    fn start_new_record_with_state(
        &mut self,
        focus_info: Option<WindowFocusInfo>,
        state: ActivityState,
    ) {
        let now_utc = Utc::now();
        self.current_record = Some(ActivityRecord {
            record_id: self.next_record_id,
            start_time: now_utc,
            end_time: None,
            state,
            focus_info,
            event_count: 0,
            triggering_events: Vec::new(),
        });
        self.next_record_id += 1;
        info!(
            "Started new record #{} with state {:?}",
            self.current_record.as_ref().unwrap().record_id,
            state
        );
    }

    pub fn check_timeouts(&mut self) -> Option<ActivityRecord> {
        let now = Instant::now();
        if self.current_state == ActivityState::Locked {
            return None;
        }
        let time_since_activity = now.duration_since(self.last_activity);
        let time_since_keyboard = now.duration_since(self.last_keyboard_activity);

        let target_state = match self.current_state {
            ActivityState::Active if time_since_keyboard >= self.active_timeout => {
                debug!(
                    "Timeout: Active -> Passive ({}s since keyboard)",
                    time_since_keyboard.as_secs()
                );
                Some(ActivityState::Passive)
            }
            ActivityState::Passive if time_since_activity >= self.passive_timeout => {
                debug!(
                    "Timeout: Passive -> Inactive ({}s since activity)",
                    time_since_activity.as_secs()
                );
                Some(ActivityState::Inactive)
            }
            _ => None,
        };

        if let Some(new_state) = target_state {
            let completed = self.transition_to_state(new_state, None);
            if let Some(ref record) = completed {
                info!(
                    "Timeout record #{}: {:?} completed",
                    record.record_id, record.state
                );
            }
            completed
        } else {
            None
        }
    }

    fn handle_keyboard_event(&mut self, event_id: u64) -> Option<ActivityRecord> {
        let now = Instant::now();
        if self.current_state == ActivityState::Locked {
            // Ignore inputs while locked
            return None;
        }
        self.last_activity = now;
        self.last_keyboard_activity = now;

        if self.current_record.is_none() {
            self.start_new_record(self.current_focus.clone());
        }

        debug!("Keyboard event -> transitioning to Active");
        let completed = self.transition_to_state(ActivityState::Active, Some(event_id));
        self.add_event_to_current_record(event_id);
        completed
    }

    fn handle_mouse_event(&mut self, event_id: u64) -> Option<ActivityRecord> {
        let now = Instant::now();
        if self.current_state == ActivityState::Locked {
            // Ignore inputs while locked
            return None;
        }
        self.last_activity = now;

        if self.current_record.is_none() {
            self.start_new_record(self.current_focus.clone());
        }

        let target_state = if now.duration_since(self.last_keyboard_activity) < self.active_timeout
        {
            ActivityState::Active
        } else {
            ActivityState::Passive
        };

        debug!("Mouse event -> transitioning to {:?}", target_state);
        let completed = self.transition_to_state(target_state, Some(event_id));
        self.add_event_to_current_record(event_id);
        completed
    }

    fn handle_focus_change(
        &mut self,
        event_id: u64,
        focus_info: WindowFocusInfo,
    ) -> Option<ActivityRecord> {
        info!(
            "Focus change: '{}' in {} (PID {}, WinID {}, Start {})",
            focus_info.window_title,
            focus_info.app_name,
            focus_info.pid,
            focus_info.window_id,
            focus_info.process_start_time
        );

        let is_same_window = self.current_focus.as_ref().is_some_and(|current| {
            current.pid == focus_info.pid
                && current.process_start_time == focus_info.process_start_time
                && current.window_id == focus_info.window_id
                && current.window_instance_start == focus_info.window_instance_start
        });

        self.current_focus = Some(focus_info.clone());
        // Any real focus signal counts as activity
        self.last_activity = Instant::now();

        // Handle macOS lock based on loginwindow app name
        let is_loginwindow = focus_info
            .app_name
            .eq_ignore_ascii_case("loginwindow");

        if is_loginwindow {
            // Entering Locked: finalize current and start Locked record with loginwindow focus
            let completed_record = if let Some(current) = self.current_record.take() {
                let mut completed = current;
                completed.end_time = Some(Utc::now());
                info!(
                    "Entering Locked, completed record #{}: {:?} with {} events",
                    completed.record_id, completed.state, completed.event_count
                );
                Some(completed)
            } else {
                None
            };

            self.current_state = ActivityState::Locked;
            self.start_new_record_with_state(Some(focus_info), ActivityState::Locked);
            return completed_record;
        }

        if self.current_state == ActivityState::Locked {
            // Exiting Locked on non-loginwindow focus: finalize locked and start Active
            let completed_record = if let Some(current) = self.current_record.take() {
                let mut completed = current;
                completed.end_time = Some(Utc::now());
                info!(
                    "Exiting Locked, completed record #{}",
                    completed.record_id
                );
                Some(completed)
            } else {
                None
            };

            self.current_state = ActivityState::Active;
            self.start_new_record_with_state(Some(focus_info), ActivityState::Active);
            self.add_event_to_current_record(event_id);
            return completed_record;
        }

        if is_same_window {
            // It's the same window, just a title change. Complete the current record and start a new one
            // to ensure the UI gets updated with the new title.
            let completed_record = if let Some(current) = self.current_record.take() {
                let mut completed = current;
                completed.end_time = Some(Utc::now());
                info!(
                    "Title change completed record #{}: {:?} with {} events",
                    completed.record_id, completed.state, completed.event_count
                );
                Some(completed)
            } else {
                None
            };

            // Keep state unchanged on title-only change
            let state = self.current_state;
            self.start_new_record_with_state(Some(focus_info), state);
            self.add_event_to_current_record(event_id);
            completed_record
        } else {
            // It's a different window. Complete the old record and start a new one.
            let completed_record = if let Some(current) = self.current_record.take() {
                let mut completed = current;
                completed.end_time = Some(Utc::now());
                info!(
                    "Focus change completed record #{}: {:?} with {} events",
                    completed.record_id, completed.state, completed.event_count
                );
                Some(completed)
            } else {
                None
            };

            // Focus change should mark as Active immediately
            self.current_state = ActivityState::Active;
            self.start_new_record_with_state(Some(focus_info), ActivityState::Active);
            self.add_event_to_current_record(event_id);
            completed_record
        }
    }

    fn transition_to_state(
        &mut self,
        new_state: ActivityState,
        triggering_event: Option<u64>,
    ) -> Option<ActivityRecord> {
        if self.current_state == ActivityState::Locked {
            return None;
        }
        if self.current_state == new_state {
            return None;
        }

        debug!(
            "State transition: {:?} -> {:?}",
            self.current_state, new_state
        );

        let now_utc = Utc::now();
        let completed_record = self.finalize_current_record(now_utc);

        self.current_state = new_state;
        let mut triggering_events = Vec::new();
        if let Some(event_id) = triggering_event {
            triggering_events.push(event_id);
        }

        self.current_record = Some(ActivityRecord {
            record_id: self.next_record_id,
            start_time: now_utc,
            end_time: None,
            state: new_state,
            focus_info: self.current_focus.clone(),
            event_count: 0,
            triggering_events,
        });

        debug!(
            "Record #{}: Started {:?} state at {}",
            self.next_record_id,
            new_state,
            now_utc.format("%H:%M:%S%.3f")
        );

        self.next_record_id += 1;
        completed_record
    }

    fn add_event_to_current_record(&mut self, event_id: u64) {
        if self.current_state == ActivityState::Locked {
            return;
        }
        if let Some(ref mut record) = self.current_record {
            record.event_count += 1;
            if !record.triggering_events.contains(&event_id) {
                record.triggering_events.push(event_id);
            }
            debug!(
                "Added event #{} to record #{} (total events: {})",
                event_id, record.record_id, record.event_count
            );
        }
    }

    fn finalize_current_record(&mut self, end_time: DateTime<Utc>) -> Option<ActivityRecord> {
        if let Some(mut record) = self.current_record.take() {
            record.end_time = Some(end_time);
            Some(record)
        } else {
            None
        }
    }

    pub fn finalize_all(&mut self) -> Option<ActivityRecord> {
        let final_record = self.finalize_current_record(Utc::now());
        if let Some(ref record) = final_record {
            info!(
                "Final record #{}: {:?} finalized",
                record.record_id, record.state
            );
        }
        final_record
    }

    pub fn current_state(&self) -> ActivityState {
        self.current_state
    }

    pub fn current_record_info(&self) -> Option<(u64, ActivityState, DateTime<Utc>, u32)> {
        self.current_record
            .as_ref()
            .map(|r| (r.record_id, r.state, r.start_time, r.event_count))
    }
}

// Aggregate only the portion of records overlapping [since, now].
pub fn aggregate_activities_since(
    records: &[ActivityRecord],
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    short_thresh_secs: u64,
    ephemeral_min_distinct_ids: usize,
    max_windows_per_app: usize,
) -> Vec<AggregatedActivity> {
    let mut app_map: HashMap<(u32, u64), AggregatedActivity> = HashMap::new();
    let mut interner = WindowIdInterner::new();

    for record in records {
        if let Some(focus_info) = &record.focus_info {
            let app_key = (focus_info.pid as u32, focus_info.process_start_time);
            let rec_start = record.start_time.max(since);
            let rec_end = record.end_time.unwrap_or(now);
            if rec_end <= since {
                continue;
            }
            let dur = (rec_end - rec_start).num_seconds();
            if dur <= 0 {
                continue;
            }
            let duration = dur as u64;

            let aggregated = app_map
                .entry(app_key)
                .or_insert_with(|| AggregatedActivity {
                    app_name: focus_info.app_name.clone(),
                    pid: focus_info.pid,
                    process_start_time: focus_info.process_start_time,
                    windows: HashMap::new(),
                    ephemeral_groups: HashMap::new(),
                    total_duration_seconds: 0,
                    first_seen: rec_start,
                    last_seen: rec_end,
                });

            aggregated.total_duration_seconds += duration;
            aggregated.first_seen = aggregated.first_seen.min(rec_start);
            aggregated.last_seen = aggregated.last_seen.max(rec_end);

            if !focus_info.window_title.is_empty() && !focus_info.window_id.is_empty() {
                let wid_id = interner.intern(&focus_info.window_id);
                let window_activity =
                    aggregated
                        .windows
                        .entry(wid_id)
                        .or_insert_with(|| WindowActivity {
                            window_id: focus_info.window_id.clone(),
                            window_title: focus_info.window_title.clone(),
                            duration_seconds: 0,
                            first_seen: rec_start,
                            last_seen: rec_end,
                            record_count: 0,
                        });

                window_activity.window_title = focus_info.window_title.clone();
                window_activity.duration_seconds += duration;
                window_activity.first_seen = window_activity.first_seen.min(rec_start);
                window_activity.last_seen = window_activity.last_seen.max(rec_end);
                window_activity.record_count += 1;
            }
        }
    }

    // Build ephemeral groups (short-lived windows) per app for the since-window aggregation.
    for agg in app_map.values_mut() {
        // First pass: collect short-lived windows and stage groups without mutating lanes yet.
        let mut staged: HashMap<String, EphemeralGroup> = HashMap::new();
        let mut group_members: HashMap<String, Vec<u32>> = HashMap::new();

        for (id, w) in agg.windows.iter() {
            if w.duration_seconds < short_thresh_secs {
                let title_key = normalize_title(&w.window_title);
                let entry = staged
                    .entry(title_key.clone())
                    .or_insert_with(|| EphemeralGroup {
                        title_key: title_key.clone(),
                        distinct_ids: HashSet::new(),
                        occurrence_count: 0,
                        total_duration_seconds: 0,
                        first_seen: w.first_seen,
                        last_seen: w.last_seen,
                    });
                entry.distinct_ids.insert(*id);
                entry.occurrence_count += 1;
                entry.total_duration_seconds += w.duration_seconds;
                entry.first_seen = entry.first_seen.min(w.first_seen);
                entry.last_seen = entry.last_seen.max(w.last_seen);

                group_members.entry(title_key).or_default().push(*id);
            }
        }

        // Keep groups meeting the distinct-ID threshold and remove their members from the normal lane
        for (title_key, group) in staged.into_iter() {
            if group.distinct_ids.len() >= ephemeral_min_distinct_ids {
                agg.ephemeral_groups.insert(title_key.clone(), group);
                if let Some(members) = group_members.get(&title_key) {
                    for id in members.iter() {
                        agg.windows.remove(id);
                    }
                }
            }
        }

        // LRU trim: keep most recent windows by last_seen
        if max_windows_per_app > 0 && agg.windows.len() > max_windows_per_app {
            let mut win_vec: Vec<(u32, WindowActivity)> =
                agg.windows.iter().map(|(k, v)| (*k, v.clone())).collect();
            win_vec.sort_by(|a, b| b.1.last_seen.cmp(&a.1.last_seen));
            agg.windows.clear();
            for (i, (k, v)) in win_vec.into_iter().enumerate() {
                if i < max_windows_per_app {
                    agg.windows.insert(k, v);
                } else {
                    break;
                }
            }
        }
    }

    let mut apps: Vec<AggregatedActivity> = app_map.into_values().collect();
    apps.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    apps
}
fn normalize_title(s: &str) -> String {
    s.trim().to_lowercase()
}

struct WindowIdInterner {
    map: HashMap<String, u32>,
    next: u32,
}
impl WindowIdInterner {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next: 1,
        }
    }
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.next;
        self.map.insert(s.to_string(), id);
        self.next = self.next.saturating_add(1);
        id
    }
}
