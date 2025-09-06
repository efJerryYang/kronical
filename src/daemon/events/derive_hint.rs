use crate::daemon::events::model::{DefaultStateEngine, StateEngine};
use crate::daemon::events::model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use crate::daemon::records::ActivityState;
use chrono::{DateTime, Utc};

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
    use crate::daemon::events::model::{EventEnvelope, EventKind, EventSource, SignalKind};

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
    }
}
