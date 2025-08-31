use chrono::{DateTime, Utc};

use crate::daemon::event_model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use crate::daemon::events::{RawEvent, WindowFocusInfo};

pub struct EventAdapter {
    last_focus: Option<WindowFocusInfo>,
}

impl EventAdapter {
    pub fn new() -> Self {
        Self { last_focus: None }
    }

    pub fn adapt_batch(&mut self, raw: &Vec<RawEvent>) -> Vec<EventEnvelope> {
        let mut out = Vec::with_capacity(raw.len());
        for e in raw.iter() {
            match e {
                RawEvent::KeyboardInput {
                    timestamp,
                    event_id,
                    data,
                } => {
                    out.push(EventEnvelope {
                        id: *event_id,
                        timestamp: *timestamp,
                        source: EventSource::Hook,
                        kind: EventKind::Signal(SignalKind::KeyboardInput),
                        payload: EventPayload::Keyboard(data.clone()),
                        derived: false,
                        polling: false,
                        sensitive: false,
                    });
                }
                RawEvent::MouseInput {
                    timestamp,
                    event_id,
                    data,
                } => {
                    out.push(EventEnvelope {
                        id: *event_id,
                        timestamp: *timestamp,
                        source: EventSource::Hook,
                        kind: EventKind::Signal(SignalKind::MouseInput),
                        payload: EventPayload::Mouse(data.clone()),
                        derived: false,
                        polling: false,
                        sensitive: false,
                    });
                }
                RawEvent::WindowFocusChange {
                    timestamp,
                    event_id,
                    focus_info,
                } => {
                    let now: DateTime<Utc> = *timestamp;
                    let current = focus_info.clone();
                    let mut emitted = false;
                    if let Some(last) = &self.last_focus {
                        if last.pid != current.pid
                            || last.process_start_time != current.process_start_time
                            || last.app_name != current.app_name
                        {
                            out.push(EventEnvelope {
                                id: *event_id,
                                timestamp: now,
                                source: EventSource::Hook,
                                kind: EventKind::Signal(SignalKind::AppChanged),
                                payload: EventPayload::Focus(current.clone()),
                                derived: false,
                                polling: false,
                                sensitive: false,
                            });
                            // New model: emit FocusChanged hint and ActivityPulse signal
                            out.push(EventEnvelope {
                                id: *event_id,
                                timestamp: now,
                                source: EventSource::Hook,
                                kind: EventKind::Hint(HintKind::FocusChanged),
                                payload: EventPayload::Focus(current.clone()),
                                derived: false,
                                polling: false,
                                sensitive: false,
                            });
                            out.push(EventEnvelope {
                                id: *event_id,
                                timestamp: now,
                                source: EventSource::Hook,
                                kind: EventKind::Signal(SignalKind::ActivityPulse),
                                payload: EventPayload::None,
                                derived: false,
                                polling: false,
                                sensitive: false,
                            });
                            emitted = true;
                        } else if last.window_id != current.window_id {
                            out.push(EventEnvelope {
                                id: *event_id,
                                timestamp: now,
                                source: EventSource::Hook,
                                kind: EventKind::Signal(SignalKind::WindowChanged),
                                payload: EventPayload::Focus(current.clone()),
                                derived: false,
                                polling: false,
                                sensitive: false,
                            });
                            // New model
                            out.push(EventEnvelope {
                                id: *event_id,
                                timestamp: now,
                                source: EventSource::Hook,
                                kind: EventKind::Hint(HintKind::FocusChanged),
                                payload: EventPayload::Focus(current.clone()),
                                derived: false,
                                polling: false,
                                sensitive: false,
                            });
                            out.push(EventEnvelope {
                                id: *event_id,
                                timestamp: now,
                                source: EventSource::Hook,
                                kind: EventKind::Signal(SignalKind::ActivityPulse),
                                payload: EventPayload::None,
                                derived: false,
                                polling: false,
                                sensitive: false,
                            });
                            emitted = true;
                        } else if last.window_title != current.window_title {
                            out.push(EventEnvelope {
                                id: *event_id,
                                timestamp: now,
                                source: EventSource::Polling,
                                kind: EventKind::Hint(HintKind::TitleChanged),
                                payload: EventPayload::Title {
                                    window_id: current.window_id.clone(),
                                    title: current.window_title.clone(),
                                },
                                derived: false,
                                polling: true,
                                sensitive: false,
                            });
                            emitted = true;
                        }
                    } else {
                        // First observation — treat as AppChanged baseline
                        out.push(EventEnvelope {
                            id: *event_id,
                            timestamp: now,
                            source: EventSource::Hook,
                            kind: EventKind::Signal(SignalKind::AppChanged),
                            payload: EventPayload::Focus(current.clone()),
                            derived: false,
                            polling: false,
                            sensitive: false,
                        });
                        out.push(EventEnvelope {
                            id: *event_id,
                            timestamp: now,
                            source: EventSource::Hook,
                            kind: EventKind::Hint(HintKind::FocusChanged),
                            payload: EventPayload::Focus(current.clone()),
                            derived: false,
                            polling: false,
                            sensitive: false,
                        });
                        out.push(EventEnvelope {
                            id: *event_id,
                            timestamp: now,
                            source: EventSource::Hook,
                            kind: EventKind::Signal(SignalKind::ActivityPulse),
                            payload: EventPayload::None,
                            derived: false,
                            polling: false,
                            sensitive: false,
                        });
                        emitted = true;
                    }
                    self.last_focus = Some(current);
                    if !emitted {
                        // No-op update; keep nothing
                    }
                }
            }
        }
        out
    }
}
