use crate::events::model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};

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
            let mut forward_original = true;
            if let EventKind::Signal(kind) = &e.kind {
                match kind {
                    SignalKind::AppChanged | SignalKind::WindowChanged => {
                        let is_login = matches!(
                            &e.payload,
                            EventPayload::Focus(fi) if fi.app_name.eq_ignore_ascii_case("loginwindow")
                        );
                        if is_login {
                            if !self.locked {
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
                            }
                            self.locked = true;
                            forward_original = false;
                        } else if self.locked {
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
                    SignalKind::LockStart => {
                        self.locked = true;
                    }
                    SignalKind::LockEnd => {
                        self.locked = false;
                    }
                    _ => {
                        if self.locked {
                            forward_original = false;
                        }
                    }
                }
            } else if self.locked {
                forward_original = matches!(&e.kind, EventKind::Hint(HintKind::FocusChanged));
            }

            if forward_original {
                out.push(e.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::model::HintKind;
    use crate::events::{KeyboardEventData, MouseEventData, MousePosition, WindowFocusInfo};
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
        assert_eq!(out.len(), 1, "only derived lock start emitted");
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

        // A subsequent loginwindow focus should not re-emit LockStart while locked.
        let second = deriver.derive(&[login_event.clone()]);
        assert!(second.is_empty());
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
    fn non_focus_signals_suppressed_while_locked() {
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
        assert!(out.is_empty(), "keyboard input suppressed while locked");

        let mouse_signal = EventEnvelope {
            id: 9,
            timestamp: Utc.with_ymd_and_hms(2024, 4, 22, 9, 31, 5).unwrap(),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::MouseInput),
            payload: EventPayload::Mouse(MouseEventData {
                position: MousePosition { x: 5, y: 5 },
                button: None,
                click_count: None,
                event_type: None,
                wheel_amount: None,
                wheel_rotation: None,
                wheel_axis: None,
                wheel_scroll_type: None,
            }),
            derived: false,
            polling: false,
            sensitive: false,
        };

        let mouse_out = deriver.derive(&[mouse_signal]);
        assert!(mouse_out.is_empty(), "mouse input suppressed while locked");

        let follow_up = deriver.derive(&[keyboard_signal.clone()]);
        assert!(follow_up.is_empty(), "lock remains until focus change");
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
    fn focus_hint_passes_and_title_hint_is_suppressed() {
        let mut deriver = LockDeriver::new();
        let login_event = focus_event(110, "loginwindow", SignalKind::AppChanged);
        let derived = deriver.derive(&[login_event.clone()]);
        assert_eq!(derived.len(), 1);

        let focus_hint = EventEnvelope {
            id: 111,
            timestamp: login_event.timestamp,
            source: EventSource::Hook,
            kind: EventKind::Hint(HintKind::FocusChanged),
            payload: login_event.payload.clone(),
            derived: false,
            polling: false,
            sensitive: false,
        };
        let hint_out = deriver.derive(&[focus_hint.clone()]);
        assert_eq!(hint_out.len(), 1);
        assert_eq!(hint_out[0].kind, focus_hint.kind);

        let title_hint = EventEnvelope {
            id: 112,
            timestamp: login_event.timestamp,
            source: EventSource::Polling,
            kind: EventKind::Hint(HintKind::TitleChanged),
            payload: EventPayload::Title {
                window_id: 7,
                title: "Login Screen".to_string(),
            },
            derived: false,
            polling: true,
            sensitive: false,
        };
        let title_out = deriver.derive(&[title_hint]);
        assert!(title_out.is_empty(), "title hints suppressed while locked");
    }
}
