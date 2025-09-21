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
        Self {
            current_state: initial_state,
            current_record: None,
            current_focus: None,
            next_record_id: 1,
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
