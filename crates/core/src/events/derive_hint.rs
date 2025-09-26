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
    use crate::events::{
        KeyboardEventData, MouseButton, MouseEventData, MouseEventKind, MousePosition,
    };
    use chrono::{Duration, TimeZone, Utc};

    fn keyboard_event(id: u64, ts: DateTime<Utc>) -> EventEnvelope {
        EventEnvelope {
            id,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::KeyboardInput),
            payload: EventPayload::Keyboard(KeyboardEventData {
                key_code: Some(40),
                key_char: None,
                modifiers: vec![],
            }),
            derived: false,
            polling: false,
            sensitive: false,
        }
    }

    fn mouse_event(id: u64, ts: DateTime<Utc>) -> EventEnvelope {
        EventEnvelope {
            id,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::MouseInput),
            payload: EventPayload::Mouse(MouseEventData {
                position: MousePosition { x: 0, y: 0 },
                button: Some(MouseButton::Primary),
                click_count: Some(1),
                event_type: Some(MouseEventKind::Clicked),
                wheel_amount: None,
                wheel_rotation: None,
                wheel_axis: None,
                wheel_scroll_type: None,
            }),
            derived: false,
            polling: false,
            sensitive: false,
        }
    }

    fn activity_hint(state_env: EventEnvelope) -> (ActivityState, ActivityState) {
        if let EventPayload::State { from, to } = state_env.payload {
            (from, to)
        } else {
            panic!("expected state payload");
        }
    }

    #[test]
    fn emits_state_changes_for_activity_and_timeout() {
        let start = Utc.with_ymd_and_hms(2024, 4, 22, 10, 0, 0).unwrap();
        let mut deriver = StateDeriver::new(start, 30, 120);

        let first = deriver
            .on_signal(&keyboard_event(1, start))
            .expect("first signal should transition to active");
        assert!(first.derived);
        let (from, to) = activity_hint(first);
        assert_eq!(from, ActivityState::Inactive);
        assert_eq!(to, ActivityState::Active);

        // A second signal within the active grace interval should not emit another hint.
        let second = deriver.on_signal(&keyboard_event(2, start + Duration::seconds(5)));
        assert!(second.is_none());

        // Advance beyond active grace but before idle threshold via tick.
        let tick = deriver
            .on_tick(start + Duration::seconds(45))
            .expect("tick should downgrade to passive");
        let (from, to) = activity_hint(tick);
        assert_eq!(from, ActivityState::Active);
        assert_eq!(to, ActivityState::Passive);

        // Another tick past idle threshold moves to inactive.
        let second_tick = deriver
            .on_tick(start + Duration::seconds(200))
            .expect("tick should downgrade to inactive");
        let (from, to) = activity_hint(second_tick);
        assert_eq!(from, ActivityState::Passive);
        assert_eq!(to, ActivityState::Inactive);
    }

    #[test]
    fn handles_lock_cycle_with_state_hints() {
        let start = Utc.with_ymd_and_hms(2024, 4, 22, 11, 0, 0).unwrap();
        let mut deriver = StateDeriver::new(start, 60, 300);

        // Begin with activity to bump into Active.
        deriver.on_signal(&keyboard_event(10, start));

        let lock_start = EventEnvelope {
            id: 20,
            timestamp: start + Duration::seconds(1),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::LockStart),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let lock_hint = deriver
            .on_signal(&lock_start)
            .expect("lock start should emit state change");
        let (_, to) = activity_hint(lock_hint);
        assert_eq!(to, ActivityState::Locked);

        let lock_end = EventEnvelope {
            id: 21,
            timestamp: start + Duration::seconds(10),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::LockEnd),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let unlock_hint = deriver
            .on_signal(&lock_end)
            .expect("lock end should emit state change");
        let (from, to) = activity_hint(unlock_hint);
        assert_eq!(from, ActivityState::Locked);
        assert_eq!(to, ActivityState::Active);

        // In absence of further signals, a later tick should drop back to passive.
        let tick = deriver
            .on_tick(start + Duration::seconds(100))
            .expect("should degrade after unlock");
        let (from, to) = activity_hint(tick);
        assert_eq!(from, ActivityState::Active);
        assert_eq!(to, ActivityState::Passive);
    }

    #[test]
    fn ignores_activity_signals_while_locked() {
        let start = Utc.with_ymd_and_hms(2024, 4, 22, 12, 0, 0).unwrap();
        let mut deriver = StateDeriver::new(start, 30, 120);
        let lock_start = EventEnvelope {
            id: 50,
            timestamp: start,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::LockStart),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };
        deriver.on_signal(&lock_start);

        let mouse = mouse_event(51, start + Duration::seconds(10));
        assert!(deriver.on_signal(&mouse).is_none());
    }

    #[test]
    fn on_tick_returns_none_while_locked() {
        let start = Utc.with_ymd_and_hms(2024, 4, 22, 12, 30, 0).unwrap();
        let mut deriver = StateDeriver::new(start, 30, 120);
        let lock_start = EventEnvelope {
            id: 60,
            timestamp: start,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::LockStart),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };
        deriver.on_signal(&lock_start);

        assert!(deriver.on_tick(start + Duration::seconds(10)).is_none());
    }
}
