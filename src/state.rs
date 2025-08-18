use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ActivityState {
    Active,
    Passive,
    Inactive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityRecord {
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub state: ActivityState,
    pub window_title: Option<String>,
    pub app_name: Option<String>,
    pub pid: Option<i32>,
}

pub struct StateMachine {
    current_state: ActivityState,
    last_activity: Instant,
    last_keyboard_activity: Instant,
    active_timeout: Duration,
    passive_timeout: Duration,
    current_record: Option<ActivityRecord>,
}

impl StateMachine {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            current_state: ActivityState::Inactive,
            last_activity: now,
            last_keyboard_activity: now,
            active_timeout: Duration::from_secs(30),
            passive_timeout: Duration::from_secs(60),
            current_record: None,
        }
    }

    pub fn on_keyboard_event(&mut self) -> Option<ActivityRecord> {
        let now = Instant::now();
        self.last_activity = now;
        self.last_keyboard_activity = now;
        self.transition_to_state(ActivityState::Active)
    }

    pub fn on_mouse_event(&mut self) -> Option<ActivityRecord> {
        let now = Instant::now();
        self.last_activity = now;
        
        let target_state = if now.duration_since(self.last_keyboard_activity) < self.active_timeout {
            ActivityState::Active
        } else {
            ActivityState::Passive
        };
        
        self.transition_to_state(target_state)
    }

    pub fn on_window_change(&mut self, window_title: String, app_name: String, pid: i32) {
        if let Some(record) = &mut self.current_record {
            record.window_title = Some(window_title);
            record.app_name = Some(app_name);
            record.pid = Some(pid);
        }
    }

    pub fn check_timeouts(&mut self) -> Option<ActivityRecord> {
        let now = Instant::now();
        let time_since_activity = now.duration_since(self.last_activity);
        let time_since_keyboard = now.duration_since(self.last_keyboard_activity);

        let target_state = match self.current_state {
            ActivityState::Active if time_since_keyboard >= self.active_timeout => {
                ActivityState::Passive
            }
            ActivityState::Passive if time_since_activity >= self.passive_timeout => {
                ActivityState::Inactive
            }
            _ => return None,
        };

        self.transition_to_state(target_state)
    }

    fn transition_to_state(&mut self, new_state: ActivityState) -> Option<ActivityRecord> {
        if self.current_state == new_state {
            return None;
        }

        let now_utc = Utc::now();
        let mut completed_record = None;

        if let Some(mut record) = self.current_record.take() {
            record.end_time = Some(now_utc);
            completed_record = Some(record);
        }

        self.current_state = new_state;
        self.current_record = Some(ActivityRecord {
            start_time: now_utc,
            end_time: None,
            state: new_state,
            window_title: None,
            app_name: None,
            pid: None,
        });

        completed_record
    }

    pub fn finalize_current_record(&mut self) -> Option<ActivityRecord> {
        if let Some(mut record) = self.current_record.take() {
            record.end_time = Some(Utc::now());
            Some(record)
        } else {
            None
        }
    }

    pub fn current_state(&self) -> ActivityState {
        self.current_state
    }
}