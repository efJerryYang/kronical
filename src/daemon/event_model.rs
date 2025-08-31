use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::daemon::events::{KeyboardEventData, MouseEventData, WindowFocusInfo};
use crate::daemon::records::ActivityState;

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
    LockStart,
    LockEnd,
}

// Hints split records or enrich metadata, but do not affect state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum HintKind {
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
    Lock { reason: String },
    Title { window_id: String, title: String },
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

impl EventEnvelope {
    pub fn is_signal(&self) -> bool {
        matches!(self.kind, EventKind::Signal(_))
    }
    pub fn is_hint(&self) -> bool {
        matches!(self.kind, EventKind::Hint(_))
    }
}

// A producer provides events from some source (hook, poller, or derived)
pub trait EventProducer {
    fn poll(&mut self) -> Vec<EventEnvelope>;
}

// A deriver consumes upstream envelopes and may produce derived envelopes (e.g., LockStart/End)
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
pub trait SplitPolicy {
    fn split_reason(&self, event: &EventEnvelope) -> Option<SplitReason>;
}

pub struct DefaultSplitPolicy;
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
            ActivityState::Inactive // Locked handled via explicit Lock state transitions outside compute
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
    fn current(&self) -> ActivityState { self.current }

    fn on_signal(&mut self, event: &EventEnvelope, now: DateTime<Utc>) -> Option<StateTransition> {
        match event.kind {
            EventKind::Signal(SignalKind::LockStart) => {
                self.locked = true;
                let from = self.current;
                self.current = ActivityState::Inactive; // we’ll map this to Locked in the record layer
                return Some(StateTransition { from, to: self.current });
            }
            EventKind::Signal(SignalKind::LockEnd) => {
                self.locked = false;
                // Don’t update last_signal_time here; next app change will
                let from = self.current;
                let to = self.compute(now);
                self.current = to;
                return Some(StateTransition { from, to });
            }
            EventKind::Signal(_) if !self.locked => {
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
        if self.locked { return None; }
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

