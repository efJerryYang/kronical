use crate::events::{RawEvent, WindowFocusInfo};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use size_of::SizeOf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, SizeOf)]
pub enum ActivityState {
    Active,
    Passive,
    Inactive,
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
                log::info!(
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
            log::info!(
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
                log::info!(
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
        log::info!(
            "Started new record #{}",
            self.current_record.as_ref().unwrap().record_id
        );
    }

    pub fn check_timeouts(&mut self) -> Option<ActivityRecord> {
        let now = Instant::now();
        let time_since_activity = now.duration_since(self.last_activity);
        let time_since_keyboard = now.duration_since(self.last_keyboard_activity);

        let target_state = match self.current_state {
            ActivityState::Active if time_since_keyboard >= self.active_timeout => {
                log::info!(
                    "Timeout: Active -> Passive ({}s since keyboard)",
                    time_since_keyboard.as_secs()
                );
                Some(ActivityState::Passive)
            }
            ActivityState::Passive if time_since_activity >= self.passive_timeout => {
                log::info!(
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
                log::info!(
                    "Timeout record #{}: {:?} completed",
                    record.record_id,
                    record.state
                );
            }
            completed
        } else {
            None
        }
    }

    fn handle_keyboard_event(&mut self, event_id: u64) -> Option<ActivityRecord> {
        let now = Instant::now();
        self.last_activity = now;
        self.last_keyboard_activity = now;

        if self.current_record.is_none() {
            self.start_new_record(self.current_focus.clone());
        }

        log::info!("Keyboard event -> transitioning to Active");
        let completed = self.transition_to_state(ActivityState::Active, Some(event_id));
        self.add_event_to_current_record(event_id);
        completed
    }

    fn handle_mouse_event(&mut self, event_id: u64) -> Option<ActivityRecord> {
        let now = Instant::now();
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

        log::info!("Mouse event -> transitioning to {:?}", target_state);
        let completed = self.transition_to_state(target_state, Some(event_id));
        self.add_event_to_current_record(event_id);
        completed
    }

    fn handle_focus_change(
        &mut self,
        event_id: u64,
        focus_info: WindowFocusInfo,
    ) -> Option<ActivityRecord> {
        log::info!(
            "Focus change: '{}' in {} (PID {}, WinID {}, Start {})",
            focus_info.window_title,
            focus_info.app_name,
            focus_info.pid,
            focus_info.window_id,
            focus_info.process_start_time
        );

        let is_same_window = self.current_focus.as_ref().map_or(false, |current| {
            current.pid == focus_info.pid
                && current.process_start_time == focus_info.process_start_time
                && current.window_id == focus_info.window_id
        });

        self.current_focus = Some(focus_info.clone());

        if is_same_window {
            // It's the same window, just a title change. Update the current record.
            if let Some(ref mut record) = self.current_record {
                log::info!("Updating title for existing record #{}", record.record_id);
                record.focus_info = Some(focus_info);
                self.add_event_to_current_record(event_id);
            }
            None // No record was completed
        } else {
            // It's a different window. Complete the old record and start a new one.
            let completed_record = if let Some(current) = self.current_record.take() {
                let mut completed = current;
                completed.end_time = Some(Utc::now());
                log::info!(
                    "Focus change completed record #{}: {:?} with {} events",
                    completed.record_id,
                    completed.state,
                    completed.event_count
                );
                Some(completed)
            } else {
                None
            };

            self.start_new_record(Some(focus_info));
            self.add_event_to_current_record(event_id);
            completed_record
        }
    }

    fn transition_to_state(
        &mut self,
        new_state: ActivityState,
        triggering_event: Option<u64>,
    ) -> Option<ActivityRecord> {
        if self.current_state == new_state {
            return None;
        }

        log::info!(
            "State transition: {:?} -> {:?}",
            self.current_state,
            new_state
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

        log::info!(
            "Record #{}: Started {:?} state at {}",
            self.next_record_id,
            new_state,
            now_utc.format("%H:%M:%S%.3f")
        );

        self.next_record_id += 1;
        completed_record
    }

    fn add_event_to_current_record(&mut self, event_id: u64) {
        if let Some(ref mut record) = self.current_record {
            record.event_count += 1;
            if !record.triggering_events.contains(&event_id) {
                record.triggering_events.push(event_id);
            }
            log::info!(
                "Added event #{} to record #{} (total events: {})",
                event_id,
                record.record_id,
                record.event_count
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
            log::info!(
                "Final record #{}: {:?} finalized",
                record.record_id,
                record.state
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
