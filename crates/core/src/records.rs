use crate::events::WindowFocusInfo;
use crate::events::model::{EventEnvelope, EventKind, EventPayload, HintKind};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use kronical_common::interner::StringInterner;
use kronical_common::maps::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActivityState {
    Active,
    Passive,
    Inactive,
    Locked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityRecord {
    pub record_id: u64,
    pub run_id: Option<String>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub state: ActivityState,
    #[serde(rename = "focus")]
    pub focus_info: Option<WindowFocusInfo>,
    pub event_count: u32,
    pub triggering_events: Vec<u64>,
}

#[derive(Debug, Clone)]
pub struct WindowActivity {
    pub window_id: u32,
    pub window_title: Arc<String>,
    pub duration_seconds: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub record_count: u32,
}

#[derive(Debug, Clone)]
pub struct AggregatedActivity {
    pub app_name: Arc<String>,
    pub pid: i32,
    pub process_start_time: u64,
    pub windows: HashMap<u32, WindowActivity>,
    pub ephemeral_groups: HashMap<String, EphemeralGroup>,
    pub temporal_groups: Vec<TemporalGroup>,
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

#[derive(Debug, Clone)]
pub struct TemporalGroup {
    pub title_key: String,
    pub distinct_ids: HashSet<u32>,
    pub occurrence_count: u32,
    pub total_duration_seconds: u64,
    pub max_duration_seconds: u64,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub anchor_last_seen: DateTime<Utc>,
}

pub struct RecordBuilder {
    current_state: ActivityState,
    current_record: Option<ActivityRecord>,
    current_focus: Option<WindowFocusInfo>,
    next_record_id: u64,
    string_interner: StringInterner,
}

impl RecordBuilder {
    pub fn new(initial_state: ActivityState) -> Self {
        Self::with_next_record_id(initial_state, 0)
    }

    pub fn with_next_record_id(initial_state: ActivityState, next_record_id: u64) -> Self {
        Self {
            current_state: initial_state,
            current_record: None,
            current_focus: None,
            next_record_id,
            string_interner: StringInterner::new(),
        }
    }

    pub fn on_hint(&mut self, env: &EventEnvelope) -> Option<ActivityRecord> {
        match &env.kind {
            EventKind::Hint(HintKind::FocusChanged) => {
                if let EventPayload::Focus(f) = &env.payload {
                    return self.apply_focus_change(f.clone());
                }
                None
            }
            EventKind::Hint(HintKind::StateChanged) => {
                if let EventPayload::State { from: _, to } = env.payload {
                    return self.transition_to(to);
                }
                None
            }
            EventKind::Hint(HintKind::TitleChanged) => {
                if let EventPayload::Title {
                    window_id,
                    ref title,
                } = env.payload
                {
                    return self.apply_title_change(window_id, title);
                }
                None
            }
            _ => None,
        }
    }

    pub fn finalize_all(&mut self) -> Option<ActivityRecord> {
        self.finalize_current()
    }

    fn apply_focus_change(&mut self, focus_info: WindowFocusInfo) -> Option<ActivityRecord> {
        self.current_focus = Some(focus_info);
        let completed = self.finalize_current();
        self.start_new_current();
        completed
    }

    fn apply_title_change(&mut self, window_id: u32, new_title: &str) -> Option<ActivityRecord> {
        let should_update = self
            .current_focus
            .as_ref()
            .map(|cf| cf.window_id == window_id)
            .unwrap_or(false);
        if should_update {
            let completed = self.finalize_current();
            if let Some(cf) = &mut self.current_focus {
                cf.window_title = self.string_interner.intern(new_title);
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
            run_id: None,
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

    fn transition_to(&mut self, to: ActivityState) -> Option<ActivityRecord> {
        if self.current_state != to {
            let completed = self.finalize_current();
            self.current_state = to;
            self.start_new_current();
            completed
        } else {
            None
        }
    }
}

pub fn aggregate_activities_since(
    records: &[ActivityRecord],
    since: DateTime<Utc>,
    now: DateTime<Utc>,
    short_thresh_secs: u64,
    ephemeral_min_distinct_ids: usize,
    max_windows_per_app: usize,
) -> Vec<AggregatedActivity> {
    let mut app_map: HashMap<(u32, u64), AggregatedActivity> = HashMap::new();

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
                    temporal_groups: Vec::new(),
                    total_duration_seconds: 0,
                    first_seen: rec_start,
                    last_seen: rec_end,
                });

            aggregated.total_duration_seconds += duration;
            aggregated.first_seen = aggregated.first_seen.min(rec_start);
            aggregated.last_seen = aggregated.last_seen.max(rec_end);

            if !focus_info.window_title.is_empty() && focus_info.window_id > 0 {
                let window_id = focus_info.window_id;
                let window_title_arc = focus_info.window_title.clone();
                let window_activity =
                    aggregated
                        .windows
                        .entry(window_id)
                        .or_insert_with(|| WindowActivity {
                            window_id,
                            window_title: window_title_arc.clone(),
                            duration_seconds: 0,
                            first_seen: rec_start,
                            last_seen: rec_end,
                            record_count: 0,
                        });

                window_activity.window_title = window_title_arc;
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

        // Secondary pass: temporal locality groups for windows with similar recent activity.
        let mut temporal_clusters: Vec<TemporalGroup> = Vec::new();
        let mut grouped_ids: HashSet<u32> = HashSet::new();
        let mut by_title: HashMap<String, Vec<(u32, WindowActivity)>> = HashMap::new();
        for (id, w) in agg.windows.iter() {
            let title_key = normalize_title(&w.window_title);
            by_title
                .entry(title_key)
                .or_default()
                .push((*id, w.clone()));
        }
        let threshold = Duration::minutes(60);
        for (title_key, mut items) in by_title.into_iter() {
            if items.len() < 2 {
                continue;
            }
            items.sort_by(|a, b| b.1.last_seen.cmp(&a.1.last_seen));
            let mut current: Vec<(u32, WindowActivity)> = Vec::new();
            let mut prev_last_seen: Option<DateTime<Utc>> = None;
            for (win_id, win) in items.into_iter() {
                if current.is_empty() {
                    prev_last_seen = Some(win.last_seen);
                    current.push((win_id, win));
                    continue;
                }
                let prev = prev_last_seen.expect("cluster should have last_seen");
                let diff = prev.signed_duration_since(win.last_seen);
                if diff <= threshold {
                    prev_last_seen = Some(win.last_seen);
                    current.push((win_id, win));
                } else {
                    if current.len() >= 2 {
                        grouped_ids.extend(current.iter().map(|(wid, _)| *wid));
                        temporal_clusters.push(build_temporal_group(&title_key, &current));
                    }
                    current = vec![(win_id, win)];
                    prev_last_seen = Some(current[0].1.last_seen);
                }
            }
            if current.len() >= 2 {
                grouped_ids.extend(current.iter().map(|(wid, _)| *wid));
                temporal_clusters.push(build_temporal_group(&title_key, &current));
            }
        }
        for id in grouped_ids {
            agg.windows.remove(&id);
        }
        if !temporal_clusters.is_empty() {
            agg.temporal_groups = temporal_clusters;
        } else {
            agg.temporal_groups.clear();
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

fn build_temporal_group(title_key: &str, windows: &[(u32, WindowActivity)]) -> TemporalGroup {
    debug_assert!(windows.len() >= 2);
    let mut distinct_ids: HashSet<u32> = HashSet::new();
    let mut total: u64 = 0;
    let mut max_duration: u64 = 0;
    let mut first_seen = windows[0].1.first_seen;
    let mut last_seen = windows[0].1.last_seen;
    for (win_id, win) in windows.iter() {
        distinct_ids.insert(*win_id);
        total = total.saturating_add(win.duration_seconds);
        max_duration = max_duration.max(win.duration_seconds);
        if win.first_seen < first_seen {
            first_seen = win.first_seen;
        }
        if win.last_seen > last_seen {
            last_seen = win.last_seen;
        }
    }

    let occurrence = distinct_ids.len() as u32;

    TemporalGroup {
        title_key: title_key.to_string(),
        distinct_ids,
        occurrence_count: occurrence,
        total_duration_seconds: total,
        max_duration_seconds: max_duration,
        first_seen,
        last_seen,
        anchor_last_seen: last_seen,
    }
}

fn normalize_title(s: &str) -> String {
    s.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use crate::events::model::EventSource;

    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use std::sync::Arc;

    fn make_focus(
        pid: i32,
        process_start: u64,
        app: &str,
        window_id: u32,
        title: &str,
        at: DateTime<Utc>,
    ) -> WindowFocusInfo {
        WindowFocusInfo {
            pid,
            process_start_time: process_start,
            app_name: Arc::new(app.to_string()),
            window_title: Arc::new(title.to_string()),
            window_id,
            window_instance_start: at,
            window_position: None,
            window_size: None,
        }
    }

    fn focus_hint(id: u64, focus: WindowFocusInfo, ts: DateTime<Utc>) -> EventEnvelope {
        EventEnvelope {
            id,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Hint(HintKind::FocusChanged),
            payload: EventPayload::Focus(focus),
            derived: false,
            polling: false,
            sensitive: false,
        }
    }

    fn title_hint(id: u64, window_id: u32, title: &str, ts: DateTime<Utc>) -> EventEnvelope {
        EventEnvelope {
            id,
            timestamp: ts,
            source: EventSource::Polling,
            kind: EventKind::Hint(HintKind::TitleChanged),
            payload: EventPayload::Title {
                window_id,
                title: title.to_string(),
            },
            derived: false,
            polling: true,
            sensitive: false,
        }
    }

    fn state_hint(
        id: u64,
        ts: DateTime<Utc>,
        from: ActivityState,
        to: ActivityState,
    ) -> EventEnvelope {
        EventEnvelope {
            id,
            timestamp: ts,
            source: EventSource::Derived,
            kind: EventKind::Hint(HintKind::StateChanged),
            payload: EventPayload::State { from, to },
            derived: true,
            polling: false,
            sensitive: false,
        }
    }

    fn make_record(
        record_id: u64,
        focus: WindowFocusInfo,
        start: DateTime<Utc>,
        duration_secs: i64,
    ) -> ActivityRecord {
        ActivityRecord {
            record_id,
            run_id: None,
            start_time: start,
            end_time: Some(start + Duration::seconds(duration_secs)),
            state: ActivityState::Active,
            focus_info: Some(focus),
            event_count: 1,
            triggering_events: Vec::new(),
        }
    }

    #[test]
    fn record_builder_emits_previous_focus_on_switch() {
        let mut builder = RecordBuilder::new(ActivityState::Inactive);
        let now = Utc::now();
        let focus_a = make_focus(42, 1, "Vim", 100, "Alpha", now);
        let focus_b = make_focus(42, 1, "Vim", 101, "Beta", now + Duration::milliseconds(5));

        assert!(
            builder
                .on_hint(&focus_hint(1, focus_a.clone(), now))
                .is_none()
        );
        assert_eq!(
            builder
                .current_focus()
                .expect("focus tracked")
                .window_title
                .as_str(),
            "Alpha"
        );

        let first_record = builder
            .on_hint(&focus_hint(
                2,
                focus_b.clone(),
                now + Duration::milliseconds(10),
            ))
            .expect("previous focus should close");

        let first_focus = first_record.focus_info.expect("focus info present");
        assert_eq!(first_focus.window_id, 100);
        assert_eq!(first_focus.window_title.as_str(), "Alpha");
        assert!(first_record.end_time.unwrap() >= first_record.start_time);

        assert_eq!(
            builder
                .current_focus()
                .expect("new focus retained")
                .window_title
                .as_str(),
            "Beta"
        );
        assert_eq!(builder.current_state(), ActivityState::Inactive);

        let final_record = builder.finalize_all().expect("final record emitted");
        assert_eq!(
            final_record.focus_info.unwrap().window_title.as_str(),
            "Beta"
        );
    }

    #[test]
    fn record_builder_updates_title_for_active_window() {
        let mut builder = RecordBuilder::new(ActivityState::Inactive);
        let now = Utc::now();
        let focus = make_focus(7, 2, "Browser", 200, "Inbox", now);

        builder.on_hint(&focus_hint(1, focus.clone(), now));

        let record = builder
            .on_hint(&title_hint(
                2,
                200,
                "Inbox – Reply",
                now + Duration::milliseconds(5),
            ))
            .expect("title change closes previous slice");

        assert_eq!(record.focus_info.unwrap().window_title.as_str(), "Inbox");
        assert_eq!(builder.current_state(), ActivityState::Inactive);

        let updated_focus = builder.current_focus().expect("focus retained");
        assert_eq!(updated_focus.window_id, 200);
        assert_eq!(updated_focus.window_title.as_str(), "Inbox – Reply");
    }

    #[test]
    fn record_builder_ignores_title_change_for_non_focused_window() {
        let mut builder = RecordBuilder::new(ActivityState::Inactive);
        let now = Utc::now();
        let focus = make_focus(8, 2, "Editor", 210, "plan.rs", now);
        builder.on_hint(&focus_hint(1, focus.clone(), now));

        // Title change for another window id should be ignored.
        assert!(
            builder
                .on_hint(&title_hint(
                    2,
                    999,
                    "release-notes",
                    now + Duration::seconds(1)
                ))
                .is_none()
        );
        assert_eq!(
            builder
                .current_focus()
                .expect("focus intact")
                .window_title
                .as_str(),
            "plan.rs"
        );
    }

    #[test]
    fn record_builder_tracks_state_transitions() {
        let mut builder = RecordBuilder::new(ActivityState::Inactive);
        let now = Utc::now();
        let focus = make_focus(5, 3, "Editor", 300, "Doc", now);

        builder.on_hint(&focus_hint(1, focus, now));

        let to_active = state_hint(
            2,
            now + Duration::milliseconds(1),
            ActivityState::Inactive,
            ActivityState::Active,
        );
        let inactive_slice = builder
            .on_hint(&to_active)
            .expect("transition to active finalizes inactive slice");
        assert_eq!(inactive_slice.state, ActivityState::Inactive);
        assert_eq!(builder.current_state(), ActivityState::Active);

        let stay_active = state_hint(
            3,
            now + Duration::milliseconds(2),
            ActivityState::Active,
            ActivityState::Active,
        );
        assert!(builder.on_hint(&stay_active).is_none());

        let back_inactive = state_hint(
            4,
            now + Duration::milliseconds(3),
            ActivityState::Active,
            ActivityState::Inactive,
        );
        let active_slice = builder
            .on_hint(&back_inactive)
            .expect("transition back closes active slice");
        assert_eq!(active_slice.state, ActivityState::Active);
        assert_eq!(builder.current_state(), ActivityState::Inactive);
    }

    #[test]
    fn record_builder_respects_next_record_id() {
        let mut builder = RecordBuilder::with_next_record_id(ActivityState::Inactive, 42);
        let now = Utc::now();
        let focus_a = make_focus(1, 10, "Editor", 1, "A", now);
        let focus_b = make_focus(1, 10, "Editor", 2, "B", now + Duration::milliseconds(10));

        assert!(builder.on_hint(&focus_hint(1, focus_a, now)).is_none());

        let record = builder
            .on_hint(&focus_hint(2, focus_b, now + Duration::milliseconds(20)))
            .expect("previous focus should close");

        assert_eq!(record.record_id, 42);
    }

    #[test]
    fn aggregation_groups_ephemeral_and_temporal_windows() {
        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let since = base - Duration::seconds(10);
        let now = base + Duration::seconds(1_000);

        let focus_small_a = make_focus(10, 77, "Slack", 1, "Foo", base);
        let focus_small_b = make_focus(10, 77, "Slack", 2, "foo ", base + Duration::seconds(40));
        let focus_meeting_a =
            make_focus(10, 77, "Slack", 3, "Meeting", base + Duration::seconds(100));
        let focus_meeting_b =
            make_focus(10, 77, "Slack", 4, "Meeting", base + Duration::seconds(360));
        let focus_solo = make_focus(10, 77, "Slack", 5, "Solo", base + Duration::seconds(800));

        let records = vec![
            make_record(1, focus_small_a.clone(), base, 30),
            make_record(2, focus_small_b.clone(), base + Duration::seconds(40), 30),
            make_record(
                3,
                focus_meeting_a.clone(),
                base + Duration::seconds(100),
                200,
            ),
            make_record(
                4,
                focus_meeting_b.clone(),
                base + Duration::seconds(360),
                150,
            ),
            make_record(5, focus_solo.clone(), base + Duration::seconds(800), 100),
        ];

        let apps = aggregate_activities_since(&records, since, now, 60, 2, 10);
        assert_eq!(apps.len(), 1);
        let agg = &apps[0];
        assert_eq!(agg.pid, 10);
        assert_eq!(agg.total_duration_seconds, 30 + 30 + 200 + 150 + 100);
        assert_eq!(agg.first_seen, base);
        assert_eq!(agg.last_seen, base + Duration::seconds(900));

        let eph = agg
            .ephemeral_groups
            .get("foo")
            .expect("ephemeral group created");
        assert_eq!(eph.distinct_ids.len(), 2);
        assert_eq!(eph.occurrence_count, 2);
        assert_eq!(eph.total_duration_seconds, 60);
        assert_eq!(eph.first_seen, base);
        assert_eq!(eph.last_seen, base + Duration::seconds(70));

        assert_eq!(agg.temporal_groups.len(), 1);
        let temporal = &agg.temporal_groups[0];
        assert_eq!(temporal.title_key, "meeting");
        assert_eq!(temporal.distinct_ids.len(), 2);
        assert_eq!(temporal.total_duration_seconds, 200 + 150);
        assert_eq!(temporal.max_duration_seconds, 200);
        assert_eq!(temporal.first_seen, focus_meeting_a.window_instance_start);
        assert_eq!(
            temporal.last_seen,
            focus_meeting_b.window_instance_start + Duration::seconds(150)
        );

        assert_eq!(agg.windows.len(), 1);
        let solo = agg.windows.get(&5).expect("solo window retained");
        assert_eq!(solo.window_title.as_str(), "Solo");
        assert_eq!(solo.duration_seconds, 100);
        assert_eq!(solo.record_count, 1);
    }

    #[test]
    fn aggregation_trims_windows_by_recent_activity() {
        let base = Utc.with_ymd_and_hms(2024, 2, 1, 12, 0, 0).unwrap();
        let since = base - Duration::seconds(1);
        let now = base + Duration::seconds(1_000);

        let records = vec![
            make_record(1, make_focus(20, 99, "Editor", 10, "Spec", base), base, 30),
            make_record(
                2,
                make_focus(
                    20,
                    99,
                    "Editor",
                    11,
                    "Review",
                    base + Duration::seconds(200),
                ),
                base + Duration::seconds(200),
                120,
            ),
            make_record(
                3,
                make_focus(
                    20,
                    99,
                    "Editor",
                    12,
                    "Summary",
                    base + Duration::seconds(600),
                ),
                base + Duration::seconds(600),
                160,
            ),
        ];

        let apps = aggregate_activities_since(&records, since, now, 10, 10, 2);
        let agg = &apps[0];
        assert_eq!(agg.windows.len(), 2);
        assert!(agg.windows.get(&10).is_none());
        assert!(agg.windows.contains_key(&11));
        assert!(agg.windows.contains_key(&12));
    }
}
