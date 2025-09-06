use crate::events::model::{DefaultStateEngine, StateEngine};
use crate::events::model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use crate::records::ActivityState;
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
        if let EventKind::Signal(SignalKind::LockStart) = env.kind {
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
