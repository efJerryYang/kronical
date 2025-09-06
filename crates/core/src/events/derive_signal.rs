use crate::events::model::{EventEnvelope, EventKind, EventPayload, EventSource, SignalKind};

pub struct LockDeriver {
    locked: bool,
}

impl LockDeriver {
    pub fn new() -> Self {
        Self { locked: false }
    }

    pub fn derive(&mut self, input: &[EventEnvelope]) -> Vec<EventEnvelope> {
        let mut out = Vec::with_capacity(input.len());
        for e in input.iter() {
            if let EventKind::Signal(kind) = &e.kind {
                match kind {
                    SignalKind::AppChanged | SignalKind::WindowChanged => {
                        let is_login = match &e.payload {
                            EventPayload::Focus(fi) => {
                                fi.app_name.eq_ignore_ascii_case("loginwindow")
                            }
                            _ => false,
                        };
                        if is_login && !self.locked {
                            out.push(EventEnvelope {
                                id: e.id,
                                timestamp: e.timestamp,
                                source: EventSource::Derived,
                                kind: EventKind::Signal(SignalKind::LockStart),
                                payload: EventPayload::Lock {
                                    reason: "loginwindow".to_string(),
                                },
                                derived: true,
                                polling: false,
                                sensitive: false,
                            });
                            self.locked = true;
                        } else if self.locked && !is_login {
                            out.push(EventEnvelope {
                                id: e.id,
                                timestamp: e.timestamp,
                                source: EventSource::Derived,
                                kind: EventKind::Signal(SignalKind::LockEnd),
                                payload: EventPayload::Lock {
                                    reason: "unlock".to_string(),
                                },
                                derived: true,
                                polling: false,
                                sensitive: false,
                            });
                            self.locked = false;
                        }
                    }
                    _ => {}
                }
            }
            out.push(e.clone());
        }
        out
    }
}
