use crate::daemon::event_model::{DefaultStateEngine, StateEngine};
use crate::daemon::event_model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use crate::daemon::records::ActivityState;
use chrono::{DateTime, Utc};

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

pub struct StateDeriver {
    engine: DefaultStateEngine,
}

impl StateDeriver {
    pub fn new(now: DateTime<Utc>, active_grace_secs: i64, idle_threshold_secs: i64) -> Self {
        Self {
            engine: DefaultStateEngine::new(now, active_grace_secs, idle_threshold_secs),
        }
    }

    pub fn on_signal(&mut self, env: &EventEnvelope) -> Option<EventEnvelope> {
        let now = env.timestamp;
        // Explicit lock mapping to Locked state-change hints
        if let EventKind::Signal(SignalKind::LockStart) = env.kind {
            // Update engine's internal lock state
            let _ = self.engine.on_signal(env, now);
            let from = self.engine.current();
            return Some(EventEnvelope {
                id: env.id,
                timestamp: now,
                source: EventSource::Derived,
                kind: EventKind::Hint(HintKind::StateChanged),
                payload: EventPayload::State {
                    from,
                    to: ActivityState::Locked,
                },
                derived: true,
                polling: false,
                sensitive: false,
            });
        }
        if let EventKind::Signal(SignalKind::LockEnd) = env.kind {
            let from = ActivityState::Locked;
            // After unlock, compute based on last activity time
            if let Some(tr) = self.engine.on_signal(env, now) {
                return Some(EventEnvelope {
                    id: env.id,
                    timestamp: now,
                    source: EventSource::Derived,
                    kind: EventKind::Hint(HintKind::StateChanged),
                    payload: EventPayload::State { from, to: tr.to },
                    derived: true,
                    polling: false,
                    sensitive: false,
                });
            } else {
                return Some(EventEnvelope {
                    id: env.id,
                    timestamp: now,
                    source: EventSource::Derived,
                    kind: EventKind::Hint(HintKind::StateChanged),
                    payload: EventPayload::State {
                        from,
                        to: self.engine.current(),
                    },
                    derived: true,
                    polling: false,
                    sensitive: false,
                });
            }
        }

        if let Some(tr) = self.engine.on_signal(env, now) {
            Some(EventEnvelope {
                id: env.id,
                timestamp: now,
                source: EventSource::Derived,
                kind: EventKind::Hint(HintKind::StateChanged),
                payload: EventPayload::State {
                    from: tr.from,
                    to: tr.to,
                },
                derived: true,
                polling: false,
                sensitive: false,
            })
        } else {
            None
        }
    }

    pub fn on_tick(&mut self, now: DateTime<Utc>) -> Option<EventEnvelope> {
        if let Some(tr) = self.engine.on_tick(now) {
            Some(EventEnvelope {
                id: now.timestamp_millis() as u64,
                timestamp: now,
                source: EventSource::Derived,
                kind: EventKind::Hint(HintKind::StateChanged),
                payload: EventPayload::State {
                    from: tr.from,
                    to: tr.to,
                },
                derived: true,
                polling: false,
                sensitive: false,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::event_model::{EventEnvelope, EventKind, EventSource, SignalKind};

    fn mk_sig(kind: SignalKind, ts: DateTime<Utc>) -> EventEnvelope {
        EventEnvelope {
            id: 1,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Signal(kind),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        }
    }

    #[test]
    fn test_state_deriver_transitions_and_lock() {
        let t0 = Utc::now();
        let mut sd = StateDeriver::new(t0, 30, 300);

        // Keyboard -> Active
        let s = mk_sig(SignalKind::KeyboardInput, t0);
        let h = sd.on_signal(&s).expect("state hint");
        match h.payload {
            EventPayload::State { from, to } => {
                assert_eq!(from, crate::daemon::records::ActivityState::Inactive);
                assert_eq!(to, crate::daemon::records::ActivityState::Active);
            }
            _ => panic!("expected state payload"),
        }

        // Tick 31s -> Passive
        let h2 = sd
            .on_tick(t0 + chrono::Duration::seconds(31))
            .expect("tick to passive");
        match h2.payload {
            EventPayload::State { from, to } => {
                assert_eq!(from, crate::daemon::records::ActivityState::Active);
                assert_eq!(to, crate::daemon::records::ActivityState::Passive);
            }
            _ => panic!("expected state payload"),
        }

        // LockStart -> Locked
        let h3 = sd
            .on_signal(&mk_sig(
                SignalKind::LockStart,
                t0 + chrono::Duration::seconds(60),
            ))
            .expect("lock start hint");
        match h3.payload {
            EventPayload::State { from: _, to } => {
                assert_eq!(to, crate::daemon::records::ActivityState::Locked);
            }
            _ => panic!("expected state payload"),
        }

        // on_tick while locked -> no transition
        assert!(sd.on_tick(t0 + chrono::Duration::seconds(100)).is_none());

        // LockEnd at t0+130 -> back to computed state (Passive: 130s since keyboard)
        let h4 = sd
            .on_signal(&mk_sig(
                SignalKind::LockEnd,
                t0 + chrono::Duration::seconds(130),
            ))
            .expect("lock end hint");
        match h4.payload {
            EventPayload::State { from, to } => {
                assert_eq!(from, crate::daemon::records::ActivityState::Locked);
                assert_eq!(to, crate::daemon::records::ActivityState::Passive);
            }
            _ => panic!("expected state payload"),
        }
    }
}
