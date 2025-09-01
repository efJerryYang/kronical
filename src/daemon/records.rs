use crate::daemon::event_model::{EventEnvelope, EventKind, EventPayload, HintKind};
use crate::daemon::events::WindowFocusInfo;
use crate::util::maps::{HashMap, HashSet};
use chrono::{DateTime, Utc};
// no-op: keep minimal imports to avoid unused warnings
use serde::{Deserialize, Serialize};
use size_of::SizeOf;

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

// Legacy RecordProcessor removed in favor of StateDeriver + RecordBuilder.

// New builder strictly for records; it reacts only to hints and explicit state transitions.
pub struct RecordBuilder {
    current_state: ActivityState,
    current_record: Option<ActivityRecord>,
    current_focus: Option<WindowFocusInfo>,
    next_record_id: u64,
}

impl RecordBuilder {
    pub fn new(initial_state: ActivityState) -> Self {
        Self {
            current_state: initial_state,
            current_record: None,
            current_focus: None,
            next_record_id: 1,
        }
    }

    // Apply a hint envelope to update focus/title and split records as needed.
    pub fn on_hint(&mut self, env: &EventEnvelope) -> Option<ActivityRecord> {
        match &env.kind {
            EventKind::Hint(HintKind::FocusChanged) => {
                if let EventPayload::Focus(fi) = &env.payload {
                    self.apply_focus_change(fi.clone())
                } else {
                    None
                }
            }
            EventKind::Hint(HintKind::StateChanged) => {
                if let EventPayload::State { from: _, to } = &env.payload {
                    self.on_state_transition(*to)
                } else {
                    None
                }
            }
            EventKind::Hint(HintKind::TitleChanged) => {
                if let EventPayload::Title { window_id, title } = &env.payload {
                    self.apply_title_change(window_id, title)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    // Apply a state transition to split records and start a new slice in the new state.
    pub fn on_state_transition(&mut self, new_state: ActivityState) -> Option<ActivityRecord> {
        if self.current_state == new_state {
            return None;
        }
        let completed = self.finalize_current();
        self.current_state = new_state;
        self.start_new_current();
        completed
    }

    pub fn finalize_all(&mut self) -> Option<ActivityRecord> {
        self.finalize_current()
    }

    fn apply_focus_change(&mut self, focus_info: WindowFocusInfo) -> Option<ActivityRecord> {
        // Any focus change splits the record with current state and updates focus
        self.current_focus = Some(focus_info);
        let completed = self.finalize_current();
        self.start_new_current();
        completed
    }

    fn apply_title_change(&mut self, window_id: &str, new_title: &str) -> Option<ActivityRecord> {
        let should_update = self
            .current_focus
            .as_ref()
            .map(|cf| cf.window_id == window_id)
            .unwrap_or(false);
        if should_update {
            let completed = self.finalize_current();
            if let Some(cf) = &mut self.current_focus {
                cf.window_title = new_title.to_string();
            }
            self.start_new_current();
            return completed;
        }
        None
    }

    fn start_new_current(&mut self) {
        let now_utc = Utc::now();
        self.current_record = Some(ActivityRecord {
            record_id: self.next_record_id,
            start_time: now_utc,
            end_time: None,
            state: self.current_state,
            focus_info: self.current_focus.clone(),
            event_count: 0,
            triggering_events: Vec::new(),
        });
        self.next_record_id += 1;
    }

    fn finalize_current(&mut self) -> Option<ActivityRecord> {
        if let Some(mut rec) = self.current_record.take() {
            rec.end_time = Some(Utc::now());
            Some(rec)
        } else {
            None
        }
    }

    pub fn current_state(&self) -> ActivityState {
        self.current_state
    }

    pub fn current_focus(&self) -> Option<WindowFocusInfo> {
        self.current_focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::event_adapter::EventAdapter;
    use crate::daemon::event_deriver::StateDeriver;
    use crate::daemon::event_model::{DefaultStateEngine, EventSource, SignalKind, StateEngine};
    use crate::daemon::events::RawEvent;

    fn mk_env_signal(kind: SignalKind) -> EventEnvelope {
        EventEnvelope {
            id: 1,
            timestamp: Utc::now(),
            source: EventSource::Hook,
            kind: EventKind::Signal(kind),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        }
    }

    fn mk_focus(pid: i32, app: &str, wid: &str, title: &str) -> WindowFocusInfo {
        WindowFocusInfo {
            pid,
            process_start_time: 1,
            app_name: app.to_string(),
            window_title: title.to_string(),
            window_id: wid.to_string(),
            window_instance_start: Utc::now(),
            window_position: None,
            window_size: None,
        }
    }

    fn mk_env_focus_changed(fi: WindowFocusInfo) -> EventEnvelope {
        EventEnvelope {
            id: 2,
            timestamp: Utc::now(),
            source: EventSource::Hook,
            kind: EventKind::Hint(HintKind::FocusChanged),
            payload: EventPayload::Focus(fi),
            derived: false,
            polling: false,
            sensitive: false,
        }
    }

    fn mk_env_title_changed(wid: &str, title: &str) -> EventEnvelope {
        EventEnvelope {
            id: 3,
            timestamp: Utc::now(),
            source: EventSource::Polling,
            kind: EventKind::Hint(HintKind::TitleChanged),
            payload: EventPayload::Title {
                window_id: wid.to_string(),
                title: title.to_string(),
            },
            derived: false,
            polling: true,
            sensitive: false,
        }
    }

    #[test]
    fn test_state_engine_transitions_keyboard_mouse_timeouts() {
        let now = Utc::now();
        let mut engine = DefaultStateEngine::new(now, 30, 300);
        assert_eq!(engine.current(), ActivityState::Inactive);

        // keyboard input transitions to Active
        let ev = mk_env_signal(SignalKind::KeyboardInput);
        let tr = engine.on_signal(&ev, now).expect("transition");
        assert_eq!(tr.from, ActivityState::Inactive);
        assert_eq!(tr.to, ActivityState::Active);
        assert_eq!(engine.current(), ActivityState::Active);

        // after 31s, Active -> Passive
        let tr = engine
            .on_tick(now + chrono::Duration::seconds(31))
            .expect("tick transition to passive");
        assert_eq!(tr.from, ActivityState::Active);
        assert_eq!(tr.to, ActivityState::Passive);
        assert_eq!(engine.current(), ActivityState::Passive);

        // after 301s, Passive -> Inactive
        let tr = engine
            .on_tick(now + chrono::Duration::seconds(301))
            .expect("tick transition to inactive");
        assert_eq!(tr.from, ActivityState::Passive);
        assert_eq!(tr.to, ActivityState::Inactive);
        assert_eq!(engine.current(), ActivityState::Inactive);
    }

    #[test]
    fn test_record_builder_focus_title_and_state_splits() {
        let mut rb = RecordBuilder::new(ActivityState::Inactive);

        // First focus -> start record (Inactive)
        let f1 = mk_focus(100, "Safari", "w1", "Tab A");
        let _none = rb.on_hint(&mk_env_focus_changed(f1));
        assert!(rb.current_record.is_some());
        assert_eq!(
            rb.current_record.as_ref().unwrap().state,
            ActivityState::Inactive
        );

        // State transition to Active
        let completed1 = rb.on_state_transition(ActivityState::Active);
        assert!(completed1.is_some());
        assert_eq!(completed1.unwrap().state, ActivityState::Inactive);
        assert_eq!(
            rb.current_record.as_ref().unwrap().state,
            ActivityState::Active
        );

        // Title change on same window
        let completed2 = rb.on_hint(&mk_env_title_changed("w1", "Tab B"));
        assert!(completed2.is_some());
        assert_eq!(completed2.unwrap().state, ActivityState::Active);
        assert_eq!(
            rb.current_record.as_ref().unwrap().state,
            ActivityState::Active
        );

        // Focus change to another window
        let f2 = mk_focus(100, "Safari", "w2", "Tab C");
        let completed3 = rb.on_hint(&mk_env_focus_changed(f2));
        assert!(completed3.is_some());
        assert_eq!(completed3.unwrap().state, ActivityState::Active);
        assert_eq!(
            rb.current_record.as_ref().unwrap().state,
            ActivityState::Active
        );

        // State transition to Passive
        let completed4 = rb.on_state_transition(ActivityState::Passive);
        assert!(completed4.is_some());
        assert_eq!(completed4.unwrap().state, ActivityState::Active);
        assert_eq!(
            rb.current_record.as_ref().unwrap().state,
            ActivityState::Passive
        );
    }

    #[test]
    fn test_integration_adapter_deriver_builder_flow() {
        let now = Utc::now();
        let mut adapter = EventAdapter::new();
        let mut deriver = StateDeriver::new(now, 30, 300);
        let mut builder = RecordBuilder::new(ActivityState::Inactive);

        // t0: focus change to w1 title A
        let f1 = mk_focus(200, "Safari", "w1", "A");
        let ev1 = RawEvent::WindowFocusChange {
            timestamp: now,
            event_id: 10,
            focus_info: f1,
        };
        let envs1 = adapter.adapt_batch(&vec![ev1]);
        let mut completed: Vec<ActivityRecord> = Vec::new();
        // Apply hints first, then signals for same-timestamp batch
        for e in envs1
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Hint(_)))
        {
            if let Some(r) = builder.on_hint(e) {
                completed.push(r);
            }
        }
        for e in envs1
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Signal(_)))
        {
            if let Some(h) = deriver.on_signal(e) {
                if let Some(r) = builder.on_hint(&h) {
                    completed.push(r);
                }
            }
        }

        // Expect one completion (Inactive) due to FocusChanged then StateChanged->Active
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].state, ActivityState::Inactive);
        completed.clear();

        // t0+2s: same window, title changed to B
        let f1b = mk_focus(200, "Safari", "w1", "B");
        let ev2 = RawEvent::WindowFocusChange {
            timestamp: now + chrono::Duration::seconds(2),
            event_id: 11,
            focus_info: f1b,
        };
        let envs2 = adapter.adapt_batch(&vec![ev2]);
        for e in envs2
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Hint(_)))
        {
            if let Some(r) = builder.on_hint(e) {
                completed.push(r);
            }
        }
        for e in envs2
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Signal(_)))
        {
            if let Some(h) = deriver.on_signal(e) {
                if let Some(r) = builder.on_hint(&h) {
                    completed.push(r);
                }
            }
        }
        // Expect one completion (Active) due to TitleChanged
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].state, ActivityState::Active);
        completed.clear();

        // Tick at 40s -> Active -> Passive
        if let Some(h) = deriver.on_tick(now + chrono::Duration::seconds(40)) {
            if let Some(r) = builder.on_hint(&h) {
                completed.push(r);
            }
        }
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].state, ActivityState::Active);
        assert_eq!(builder.current_state(), ActivityState::Passive);
    }

    #[test]
    fn test_lock_splits_with_state_hints() {
        let now = Utc::now();
        let mut builder = RecordBuilder::new(ActivityState::Inactive);
        let mut deriver = StateDeriver::new(now, 30, 300);

        // Focus to normal app (start a record)
        let f = mk_focus(123, "Safari", "w1", "Home");
        let _ = builder.on_hint(&mk_env_focus_changed(f));

        // Become Active via keyboard
        if let Some(h) = deriver.on_signal(&mk_env_signal(SignalKind::KeyboardInput)) {
            let rec = builder.on_hint(&h).expect("split on state active");
            assert_eq!(rec.state, ActivityState::Inactive);
        } else {
            panic!("expected state hint active");
        }

        // LockStart -> Locked split
        if let Some(h) = deriver.on_signal(&EventEnvelope {
            id: 9,
            timestamp: now + chrono::Duration::seconds(10),
            source: EventSource::Derived,
            kind: EventKind::Signal(SignalKind::LockStart),
            payload: EventPayload::None,
            derived: true,
            polling: false,
            sensitive: false,
        }) {
            let rec = builder.on_hint(&h).expect("split on locked");
            assert_eq!(rec.state, ActivityState::Active);
            assert_eq!(builder.current_state(), ActivityState::Locked);
        } else {
            panic!("expected lock state hint");
        }

        // Exit lock by ending lock; expect transition to Passive (since > active_grace but < idle_threshold from keyboard)
        if let Some(h) = deriver.on_signal(&EventEnvelope {
            id: 10,
            timestamp: now + chrono::Duration::seconds(120),
            source: EventSource::Derived,
            kind: EventKind::Signal(SignalKind::LockEnd),
            payload: EventPayload::None,
            derived: true,
            polling: false,
            sensitive: false,
        }) {
            let rec = builder.on_hint(&h).expect("split on unlock");
            assert_eq!(rec.state, ActivityState::Locked);
            assert_eq!(builder.current_state(), ActivityState::Passive);
        } else {
            panic!("expected unlock state hint");
        }
    }

    #[test]
    fn test_title_change_ignored_for_different_window() {
        let mut rb = RecordBuilder::new(ActivityState::Active);
        let f = mk_focus(1, "Terminal", "w1", "A");
        let _ = rb.on_hint(&mk_env_focus_changed(f));
        // Title change for a different window id should not split
        let none = rb.on_hint(&mk_env_title_changed("w2", "Other"));
        assert!(none.is_none());
        // State remains
        assert_eq!(rb.current_state(), ActivityState::Active);
    }

    #[test]
    fn test_aggregation_ephemeral_grouping() {
        // Build short-lived windows under threshold and ensure grouping
        let since = Utc::now() - chrono::Duration::minutes(10);
        let base = since + chrono::Duration::seconds(60);
        let mk_rec = |id: u64, start: DateTime<Utc>, dur_s: i64, title: &str| -> ActivityRecord {
            let fi = WindowFocusInfo {
                pid: 999,
                process_start_time: 42,
                app_name: "EphemeralApp".to_string(),
                window_title: title.to_string(),
                window_id: format!("win-{}", id),
                window_instance_start: start,
                window_position: None,
                window_size: None,
            };
            ActivityRecord {
                record_id: id,
                start_time: start,
                end_time: Some(start + chrono::Duration::seconds(dur_s)),
                state: ActivityState::Active,
                focus_info: Some(fi),
                event_count: 0,
                triggering_events: vec![],
            }
        };

        // Three short windows with similar titles that normalize to same key
        let r1 = mk_rec(1, base, 5, "Task A");
        let r2 = mk_rec(2, base + chrono::Duration::seconds(10), 6, "Task A");
        let r3 = mk_rec(3, base + chrono::Duration::seconds(30), 4, "Task A");

        let now = base + chrono::Duration::minutes(5);
        let out = aggregate_activities_since(&[r1, r2, r3], since, now, 10, 2, 100);
        assert_eq!(out.len(), 1);
        let agg = &out[0];
        // Expect ephemeral group created (distinct_ids >= 2) and windows lane reduced accordingly
        assert!(agg.ephemeral_groups.len() >= 1);
        // No individual windows for grouped short-lived ones
        assert_eq!(agg.windows.len(), 0);
        // Total duration sums
        let g = agg.ephemeral_groups.values().next().unwrap();
        assert_eq!(g.occurrence_count, 3);
        assert_eq!(g.total_duration_seconds, (5 + 6 + 4) as u64);
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
    max_entries: usize,
}
impl WindowIdInterner {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            next: 1,
            max_entries: 4096,
        }
    }
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        if self.map.len() >= self.max_entries {
            self.map.clear();
            self.map.shrink_to_fit();
            self.next = 1;
        }
        let id = self.next;
        self.map.insert(s.to_string(), id);
        self.next = self.next.saturating_add(1);
        id
    }
}
