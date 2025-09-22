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
                    SignalKind::ActivityPulse => {
                        // Activity pulses are generated alongside focus changes; they should
                        // never be treated as evidence that the lock screen cleared.
                    }
                    _ => {
                        // Any other signal clears the loginwindow lock if present.
                        if self.locked {
                            out.push(EventEnvelope {
                                id: e.id,
                                timestamp: e.timestamp,
                                source: EventSource::Derived,
                                kind: EventKind::Signal(SignalKind::LockEnd),
                                payload: EventPayload::Lock {
                                    reason: "fallback".to_string(),
                                },
                                derived: true,
                                polling: false,
                                sensitive: false,
                            });
                            self.locked = false;
                        }
                    }
                }
            }
            out.push(e.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::model::HintKind;
    use crate::events::{KeyboardEventData, MousePosition, WindowFocusInfo};
    use chrono::{TimeZone, Utc};
    use std::sync::Arc;

    fn focus_event(id: u64, app: &str, kind: SignalKind) -> EventEnvelope {
        let ts = Utc
            .with_ymd_and_hms(2024, 4, 22, 9, 30, id as u32 % 60)
            .unwrap();
        let focus = WindowFocusInfo {
            pid: 1234,
            process_start_time: 99,
            app_name: Arc::new(app.to_string()),
            window_title: Arc::new("Login Screen".to_string()),
            window_id: 7,
            window_instance_start: ts,
            window_position: Some(MousePosition { x: 100, y: 200 }),
            window_size: Some((800, 600)),
        };
        EventEnvelope {
            id,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Signal(kind),
            payload: EventPayload::Focus(focus),
            derived: false,
            polling: false,
            sensitive: false,
        }
    }

    #[test]
    fn emits_lock_start_when_login_window_focused() {
        let mut deriver = LockDeriver::new();
        let login_event = focus_event(1, "LOGINWINDOW", SignalKind::AppChanged);

        let out = deriver.derive(&[login_event.clone()]);
        assert_eq!(out.len(), 2, "derived lock start plus original event");
        match &out[0].kind {
            EventKind::Signal(SignalKind::LockStart) => {
                assert!(out[0].derived);
                if let EventPayload::Lock { reason } = &out[0].payload {
                    assert_eq!(reason, "loginwindow");
                } else {
                    panic!("expected lock payload");
                }
            }
            other => panic!("unexpected first kind: {:?}", other),
        }
        assert_eq!(out[1].kind, login_event.kind);
        assert!(!out[1].derived);

        // A subsequent loginwindow focus should not re-emit LockStart while locked.
        let second = deriver.derive(&[login_event.clone()]);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].kind, login_event.kind);
        assert!(!second[0].derived);
    }

    #[test]
    fn emits_lock_end_when_focus_returns_to_normal_app() {
        let mut deriver = LockDeriver::new();
        let login_event = focus_event(5, "loginwindow", SignalKind::WindowChanged);
        let _ = deriver.derive(&[login_event]);

        let finder_event = focus_event(6, "Finder", SignalKind::WindowChanged);
        let out = deriver.derive(&[finder_event.clone()]);

        assert_eq!(out.len(), 2);
        match &out[0].kind {
            EventKind::Signal(SignalKind::LockEnd) => {
                if let EventPayload::Lock { reason } = &out[0].payload {
                    assert_eq!(reason, "unlock");
                } else {
                    panic!("expected unlock payload");
                }
            }
            other => panic!("expected lock end, got {:?}", other),
        }
        assert_eq!(out[1].kind, finder_event.kind);
    }

    #[test]
    fn other_signals_clear_lock_state() {
        let mut deriver = LockDeriver::new();
        let login_event = focus_event(7, "loginwindow", SignalKind::AppChanged);
        let _ = deriver.derive(&[login_event]);

        let keyboard_signal = EventEnvelope {
            id: 8,
            timestamp: Utc.with_ymd_and_hms(2024, 4, 22, 9, 31, 0).unwrap(),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::KeyboardInput),
            payload: EventPayload::Keyboard(KeyboardEventData {
                key_code: Some(4),
                key_char: Some('a'),
                modifiers: vec![],
            }),
            derived: false,
            polling: false,
            sensitive: false,
        };

        let out = deriver.derive(&[keyboard_signal.clone()]);
        assert_eq!(out.len(), 2);
        match &out[0] {
            EventEnvelope {
                kind: EventKind::Signal(SignalKind::LockEnd),
                payload: EventPayload::Lock { reason },
                ..
            } => assert_eq!(reason, "fallback"),
            other => panic!("expected fallback lock end, got {:?}", other),
        }
        assert_eq!(out[1].kind, keyboard_signal.kind);

        // Another non-login event should no longer emit LockEnd since lock is cleared.
        let follow_up = deriver.derive(&[keyboard_signal.clone()]);
        assert_eq!(follow_up.len(), 1);
    }

    #[test]
    fn activity_pulse_does_not_unlock_login_window() {
        let mut deriver = LockDeriver::new();
        let login_event = focus_event(9, "loginwindow", SignalKind::AppChanged);
        let _ = deriver.derive(&[login_event]);

        let activity_pulse = EventEnvelope {
            id: 10,
            timestamp: Utc.with_ymd_and_hms(2024, 4, 22, 9, 32, 0).unwrap(),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::ActivityPulse),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let out = deriver.derive(&[activity_pulse.clone()]);
        let derived_kinds: Vec<_> = out
            .iter()
            .filter(|env| env.derived)
            .map(|env| env.kind.clone())
            .collect();

        assert!(
            derived_kinds
                .iter()
                .all(|kind| *kind != EventKind::Signal(SignalKind::LockEnd)),
            "Activity pulse should not emit LockEnd while loginwindow is focused"
        );
        assert_eq!(out.last().unwrap().kind, activity_pulse.kind);
    }

    #[test]
    fn activity_pulse_keeps_state_engine_locked() {
        use crate::events::derive_hint::StateDeriver;
        use crate::records::ActivityState;

        let start = Utc.with_ymd_and_hms(2024, 4, 22, 9, 30, 0).unwrap();
        let mut lock_deriver = LockDeriver::new();
        let mut state_deriver = StateDeriver::new(start, 30, 120);

        let login_event = focus_event(30, "loginwindow", SignalKind::AppChanged);
        let derived_login = lock_deriver.derive(&[login_event]);

        let mut locked = false;
        for env in derived_login {
            if let EventKind::Signal(_) = env.kind {
                if let Some(state_hint) = state_deriver.on_signal(&env) {
                    if let EventPayload::State { to, .. } = state_hint.payload {
                        if to == ActivityState::Locked {
                            locked = true;
                        }
                    }
                }
            }
        }
        assert!(
            locked,
            "LockStart should push the state engine into Locked state"
        );

        let pulse_event = EventEnvelope {
            id: 31,
            timestamp: start + chrono::Duration::seconds(2),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::ActivityPulse),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let derived_pulse = lock_deriver.derive(&[pulse_event]);
        let mut unlock_hints = Vec::new();
        for env in derived_pulse {
            if let EventKind::Signal(_) = env.kind {
                if let Some(state_hint) = state_deriver.on_signal(&env) {
                    unlock_hints.push(state_hint);
                }
            }
        }

        assert!(
            unlock_hints.is_empty(),
            "Activity pulse should not unlock the engine while loginwindow remains focused"
        );
    }

    #[test]
    fn app_change_without_focus_payload_does_not_toggle_lock() {
        let mut deriver = LockDeriver::new();
        let ts = Utc.with_ymd_and_hms(2024, 4, 22, 9, 45, 0).unwrap();
        let env = EventEnvelope {
            id: 99,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::AppChanged),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let out = deriver.derive(&[env.clone()]);
        assert_eq!(out.len(), 1, "should only forward original event");
        let forwarded = &out[0];
        assert_eq!(forwarded.id, env.id);
        assert_eq!(forwarded.kind, env.kind);
        assert_eq!(forwarded.source, env.source);
        assert!(matches!(forwarded.payload, EventPayload::None));
        assert_eq!(forwarded.polling, env.polling);
        assert_eq!(forwarded.sensitive, env.sensitive);
        assert!(!forwarded.derived);
    }

    #[test]
    fn non_signal_events_pass_through_unmodified() {
        let mut deriver = LockDeriver::new();
        let ts = Utc.with_ymd_and_hms(2024, 4, 22, 9, 50, 0).unwrap();
        let env = EventEnvelope {
            id: 100,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Hint(HintKind::FocusChanged),
            payload: EventPayload::None,
            derived: false,
            polling: false,
            sensitive: false,
        };

        let out = deriver.derive(&[env.clone()]);
        assert_eq!(out.len(), 1);
        let forwarded = &out[0];
        assert_eq!(forwarded.id, env.id);
        assert_eq!(forwarded.kind, env.kind);
        assert_eq!(forwarded.source, env.source);
        assert!(matches!(forwarded.payload, EventPayload::None));
        assert_eq!(forwarded.polling, env.polling);
        assert_eq!(forwarded.sensitive, env.sensitive);
        assert!(!forwarded.derived);
    }
}
