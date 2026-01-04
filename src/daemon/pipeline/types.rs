use crate::daemon::events::model::EventEnvelope;
use crate::daemon::events::{RawEvent, WindowFocusInfo};
use crate::daemon::records::{ActivityRecord, ActivityState};
use crate::daemon::snapshot;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlushReason {
    Timeout,
    Shutdown,
}

#[derive(Debug)]
pub struct RawBatch {
    pub events: Vec<RawEvent>,
    pub reason: FlushReason,
}

impl RawBatch {
    pub fn new(events: Vec<RawEvent>, reason: FlushReason) -> Self {
        Self { events, reason }
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[derive(Debug)]
pub enum DeriveCommand {
    Batch(RawBatch),
    Tick(DateTime<Utc>),
    Shutdown,
}

#[derive(Debug)]
pub enum CompressionCommand {
    Process {
        batch: RawBatch,
        envelopes: Vec<EventEnvelope>,
    },
    Shutdown,
}

#[derive(Debug, Default)]
pub struct SnapshotUpdate {
    pub hints_delta: u64,
    pub signals_delta: u64,
    pub focus: Option<WindowFocusInfo>,
    pub focus_title: Option<(u32, String)>,
    pub transition: Option<snapshot::Transition>,
    pub state: Option<ActivityState>,
}

impl SnapshotUpdate {
    pub fn is_empty(&self) -> bool {
        self.hints_delta == 0
            && self.signals_delta == 0
            && self.focus.is_none()
            && self.focus_title.is_none()
            && self.transition.is_none()
            && self.state.is_none()
    }
}

#[derive(Debug)]
pub enum SnapshotMessage {
    Update(SnapshotUpdate),
    Record(ActivityRecord),
    Publish,
    HintsComplete,
    Shutdown,
}
