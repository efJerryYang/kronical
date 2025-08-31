use crate::daemon::event_model::{EventEnvelope, EventKind, EventPayload, EventSource, SignalKind};

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
            let mut emitted_lock = false;
            if let EventKind::Signal(kind) = &e.kind {
                match kind {
                    SignalKind::AppChanged | SignalKind::WindowChanged => {
                        // Only derive lock boundaries around focus changes
                        // Determine app name via Focus payload if present
                        let is_login = match &e.payload {
                            EventPayload::Focus(fi) => {
                                fi.app_name.eq_ignore_ascii_case("loginwindow")
                            }
                            _ => false,
                        };
                        if is_login && !self.locked {
                            // Emit LockStart before the original signal
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
                            emitted_lock = true;
                        } else if self.locked && !is_login {
                            // Emit LockEnd before leaving loginwindow
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
                            emitted_lock = true;
                        }
                    }
                    _ => {}
                }
            }
            // push the original after any derived lock boundary
            out.push(e.clone());
            if emitted_lock {
                // nothing else
            }
        }
        out
    }
}
