use crate::records::{ActivityRecord, AggregatedActivity, WindowActivity};
use chrono::{DateTime, Utc};
use std::collections::HashMap;

pub struct AggregationManager {
    aggregated_activities: HashMap<String, AggregatedActivity>,
}

impl AggregationManager {
    pub fn new() -> Self {
        Self {
            aggregated_activities: HashMap::new(),
        }
    }

    pub fn update_with_record(&mut self, record: &ActivityRecord) {
        if let Some(focus_info) = &record.focus_info {
            let app_key = format!("{}_{}", focus_info.pid, focus_info.process_start_time);

            let duration = if let Some(end_time) = record.end_time {
                (end_time - record.start_time).num_seconds() as u64
            } else {
                0
            };

            let aggregated = self
                .aggregated_activities
                .entry(app_key.clone())
                .or_insert_with(|| AggregatedActivity {
                    app_name: focus_info.app_name.clone(),
                    pid: focus_info.pid,
                    process_start_time: focus_info.process_start_time,
                    windows: HashMap::new(),
                    total_duration_seconds: 0,
                    first_seen: record.start_time,
                    last_seen: record.start_time,
                });

            aggregated.total_duration_seconds += duration;
            aggregated.first_seen = aggregated.first_seen.min(record.start_time);
            aggregated.last_seen = aggregated.last_seen.max(record.start_time);

            if !focus_info.window_title.is_empty() && !focus_info.window_id.is_empty() {
                let window_activity = aggregated
                    .windows
                    .entry(focus_info.window_id.clone())
                    .or_insert_with(|| WindowActivity {
                        window_id: focus_info.window_id.clone(),
                        window_title: focus_info.window_title.clone(),
                        duration_seconds: 0,
                        first_seen: record.start_time,
                        last_seen: record.start_time,
                        record_count: 0,
                    });

                window_activity.window_title = focus_info.window_title.clone();
                window_activity.duration_seconds += duration;
                window_activity.first_seen = window_activity.first_seen.min(record.start_time);
                window_activity.last_seen = window_activity.last_seen.max(record.start_time);
                window_activity.record_count += 1;
            }
        }
    }

    pub fn get_aggregated_activities(&self) -> Vec<AggregatedActivity> {
        let mut apps: Vec<AggregatedActivity> =
            self.aggregated_activities.values().cloned().collect();
        apps.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        apps
    }

    pub fn clear(&mut self) {
        self.aggregated_activities.clear();
    }

    pub fn len(&self) -> usize {
        self.aggregated_activities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.aggregated_activities.is_empty()
    }
}
