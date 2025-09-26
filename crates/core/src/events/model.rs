use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::events::{KeyboardEventData, MouseEventData, WindowFocusInfo};
use crate::records::ActivityState;

// Event provenance
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventSource {
    Hook,
    Derived,
    Polling,
}

// Signals drive state transitions and activity timers
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SignalKind {
    KeyboardInput,
    MouseInput,
    AppChanged,
    WindowChanged,
    ActivityPulse,
    LockStart,
    LockEnd,
}

// Hints split records or enrich metadata, but do not affect state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HintKind {
    FocusChanged,
    StateChanged,
    TitleChanged,
    PositionChanged,
    SizeChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventKind {
    Signal(SignalKind),
    Hint(HintKind),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPayload {
    Keyboard(KeyboardEventData),
    Mouse(MouseEventData),
    Focus(WindowFocusInfo),
    Lock {
        reason: String,
    },
    Title {
        window_id: u32,
        title: String,
    },
    State {
        from: ActivityState,
        to: ActivityState,
    },
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: u64,
    pub timestamp: DateTime<Utc>,
    pub source: EventSource,
    pub kind: EventKind,
    pub payload: EventPayload,
    // Flags useful for storage and privacy handling
    pub derived: bool,
    pub polling: bool,
    pub sensitive: bool,
}

#[allow(dead_code)]
impl EventEnvelope {
    pub fn is_signal(&self) -> bool {
        matches!(self.kind, EventKind::Signal(_))
    }
    pub fn is_hint(&self) -> bool {
        matches!(self.kind, EventKind::Hint(_))
    }
}

// A producer provides events from some source (hook, poller, or derived)
#[allow(dead_code)]
pub trait EventProducer {
    fn poll(&mut self) -> Vec<EventEnvelope>;
}

// A deriver consumes upstream envelopes and may produce derived envelopes (e.g., LockStart/End)
#[allow(dead_code)]
pub trait EventDeriver {
    fn derive(&mut self, input: &[EventEnvelope]) -> Vec<EventEnvelope>;
}

// Record splitting reasons, to keep boundaries explicit and replayable
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SplitReason {
    StateChange(ActivityState),
    AppWindowChanged,
    TitleChanged,
    LockBoundary,
}

// Decides whether an event should split the current record
#[allow(dead_code)]
pub trait SplitPolicy {
    fn split_reason(&self, event: &EventEnvelope) -> Option<SplitReason>;
}

pub struct DefaultSplitPolicy;
#[allow(dead_code)]
impl SplitPolicy for DefaultSplitPolicy {
    fn split_reason(&self, event: &EventEnvelope) -> Option<SplitReason> {
        match &event.kind {
            EventKind::Signal(SignalKind::AppChanged | SignalKind::WindowChanged) => {
                Some(SplitReason::AppWindowChanged)
            }
            EventKind::Hint(HintKind::TitleChanged) => Some(SplitReason::TitleChanged),
            _ => None,
        }
    }
}

// State transitions computed from signals and timeouts
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateTransition {
    pub from: ActivityState,
    pub to: ActivityState,
}

#[allow(dead_code)]
pub trait StateEngine {
    fn current(&self) -> ActivityState;
    fn on_signal(&mut self, event: &EventEnvelope, now: DateTime<Utc>) -> Option<StateTransition>;
    fn on_tick(&mut self, now: DateTime<Utc>) -> Option<StateTransition>;
}

pub struct DefaultStateEngine {
    current: ActivityState,
    last_signal_time: DateTime<Utc>,
    active_grace_secs: i64,
    idle_threshold_secs: i64,
    locked: bool,
}

#[allow(dead_code)]
impl DefaultStateEngine {
    pub fn new(now: DateTime<Utc>, active_grace_secs: i64, idle_threshold_secs: i64) -> Self {
        Self {
            current: ActivityState::Inactive,
            last_signal_time: now,
            active_grace_secs,
            idle_threshold_secs,
            locked: false,
        }
    }
    fn compute(&self, now: DateTime<Utc>) -> ActivityState {
        if self.locked {
            ActivityState::Locked
        } else {
            let dt = (now - self.last_signal_time).num_seconds();
            if dt <= self.active_grace_secs {
                ActivityState::Active
            } else if dt <= self.idle_threshold_secs {
                ActivityState::Passive
            } else {
                ActivityState::Inactive
            }
        }
    }
}

impl StateEngine for DefaultStateEngine {
    fn current(&self) -> ActivityState {
        self.current
    }

    fn on_signal(&mut self, event: &EventEnvelope, now: DateTime<Utc>) -> Option<StateTransition> {
        match event.kind {
            EventKind::Signal(SignalKind::LockStart) => {
                if self.locked {
                    return None;
                }
                let from = self.current;
                self.locked = true;
                self.current = ActivityState::Locked;
                return Some(StateTransition {
                    from,
                    to: self.current,
                });
            }
            EventKind::Signal(SignalKind::LockEnd) => {
                if !self.locked {
                    return None;
                }
                self.locked = false;
                let from = self.current;
                let to = self.compute(now);
                self.current = to;
                return Some(StateTransition { from, to });
            }
            EventKind::Signal(_) => {
                if self.locked {
                    return None;
                }
                self.last_signal_time = now;
                let from = self.current;
                let to = self.compute(now);
                if to != from {
                    self.current = to;
                    return Some(StateTransition { from, to });
                }
            }
            _ => {}
        }
        None
    }

    fn on_tick(&mut self, now: DateTime<Utc>) -> Option<StateTransition> {
        if self.locked {
            return None;
        }
        let from = self.current;
        let to = self.compute(now);
        if to != from {
            self.current = to;
            Some(StateTransition { from, to })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    fn default_split_policy_detects_title_and_window_changes() {
        let ts = Utc.with_ymd_and_hms(2024, 4, 22, 12, 0, 0).unwrap();
        let app_changed = EventEnvelope {
            id: 1,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::AppChanged),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };
        let title_changed = EventEnvelope {
            id: 2,
            timestamp: ts,
            source: EventSource::Derived,
            kind: EventKind::Hint(HintKind::TitleChanged),
            payload: EventPayload::Title {
                window_id: 10,
                title: "new".into(),
            },
            derived: true,
            polling: false,
            sensitive: false,
        };

        let policy = DefaultSplitPolicy;
        assert_eq!(
            policy.split_reason(&app_changed),
            Some(SplitReason::AppWindowChanged)
        );
        assert_eq!(
            policy.split_reason(&title_changed),
            Some(SplitReason::TitleChanged)
        );
    }

    #[test]
    fn state_engine_transitions_between_activity_levels() {
        let start = Utc.with_ymd_and_hms(2024, 4, 22, 12, 30, 0).unwrap();
        let mut engine = DefaultStateEngine::new(start, 30, 120);

        let signal = EventEnvelope {
            id: 5,
            timestamp: start,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::KeyboardInput),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let tr = engine
            .on_signal(&signal, start)
            .expect("first signal should promote to active");
        assert_eq!(tr.from, ActivityState::Inactive);
        assert_eq!(tr.to, ActivityState::Active);

        let tick_time = start + Duration::seconds(90);
        let tr_passive = engine
            .on_tick(tick_time)
            .expect("should degrade to passive");
        assert_eq!(tr_passive.from, ActivityState::Active);
        assert_eq!(tr_passive.to, ActivityState::Passive);

        let tr_inactive = engine
            .on_tick(start + Duration::seconds(300))
            .expect("should degrade to inactive");
        assert_eq!(tr_inactive.from, ActivityState::Passive);
        assert_eq!(tr_inactive.to, ActivityState::Inactive);

        // Lock signal forces immediate transition.
        let lock_start = EventEnvelope {
            id: 6,
            timestamp: start + Duration::seconds(301),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::LockStart),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let tr_locked = engine
            .on_signal(&lock_start, lock_start.timestamp)
            .expect("lock start should emit transition");
        assert_eq!(tr_locked.from, ActivityState::Inactive);
        assert_eq!(tr_locked.to, ActivityState::Locked);
        assert!(
            engine
                .on_tick(lock_start.timestamp + Duration::seconds(10))
                .is_none()
        );

        let lock_end = EventEnvelope {
            id: 7,
            timestamp: lock_start.timestamp + Duration::seconds(20),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::LockEnd),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };
        let tr_unlock = engine
            .on_signal(&lock_end, lock_end.timestamp)
            .expect("lock end restores activity");
        assert_eq!(tr_unlock.from, ActivityState::Locked);
        assert_eq!(tr_unlock.to, ActivityState::Inactive);
    }
}
