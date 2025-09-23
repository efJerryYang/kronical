use chrono::{DateTime, Utc};

use crate::events::model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use crate::events::{RawEvent, WindowFocusInfo};

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
                                    window_id: current.window_id,
                                    title: (*current.window_title).clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        KeyboardEventData, MouseButton, MouseEventData, MouseEventKind, MousePosition,
    };
    use chrono::{TimeZone, Utc};
    use std::sync::Arc;

    fn focus_info(app: &str, window_id: u32, title: &str) -> WindowFocusInfo {
        WindowFocusInfo {
            pid: 100,
            process_start_time: 200,
            app_name: Arc::new(app.to_string()),
            window_title: Arc::new(title.to_string()),
            window_id,
            window_instance_start: Utc.with_ymd_and_hms(2024, 4, 22, 14, 0, 0).unwrap(),
            window_position: Some(MousePosition { x: 10, y: 20 }),
            window_size: Some((1280, 720)),
        }
    }

    fn focus_event(event_id: u64, ts: i64, info: WindowFocusInfo) -> RawEvent {
        RawEvent::WindowFocusChange {
            timestamp: Utc.timestamp_opt(ts, 0).unwrap(),
            event_id,
            focus_info: info,
        }
    }

    #[test]
    fn maps_keyboard_and_mouse_events_to_signals() {
        let mut adapter = EventAdapter::new();
        let ts = Utc.with_ymd_and_hms(2024, 4, 22, 13, 0, 0).unwrap();
        let raw = vec![
            RawEvent::KeyboardInput {
                timestamp: ts,
                event_id: 1,
                data: KeyboardEventData {
                    key_code: Some(4),
                    key_char: Some('a'),
                    modifiers: vec!["shift".into()],
                },
            },
            RawEvent::MouseInput {
                timestamp: ts,
                event_id: 2,
                data: MouseEventData {
                    position: MousePosition { x: 1, y: 2 },
                    button: Some(MouseButton::Primary),
                    click_count: Some(1),
                    event_type: Some(MouseEventKind::Clicked),
                    wheel_amount: None,
                    wheel_rotation: None,
                    wheel_axis: None,
                },
            },
        ];

        let out = adapter.adapt_batch(&raw);
        assert_eq!(out.len(), 2);
        assert!(matches!(
            out[0].kind,
            EventKind::Signal(SignalKind::KeyboardInput)
        ));
        assert!(matches!(
            out[1].kind,
            EventKind::Signal(SignalKind::MouseInput)
        ));
    }

    #[test]
    fn first_focus_event_emits_app_change_and_activity_pulse() {
        let mut adapter = EventAdapter::new();
        let raw = vec![focus_event(10, 1_000, focus_info("Terminal", 1, "shell"))];

        let out = adapter.adapt_batch(&raw);
        assert_eq!(out.len(), 3);
        assert!(matches!(
            out[0].kind,
            EventKind::Signal(SignalKind::AppChanged)
        ));
        assert!(matches!(
            out[1].kind,
            EventKind::Hint(HintKind::FocusChanged)
        ));
        assert!(matches!(
            out[2].kind,
            EventKind::Signal(SignalKind::ActivityPulse)
        ));
    }

    #[test]
    fn app_switch_detects_pid_or_app_name_change() {
        let mut adapter = EventAdapter::new();
        let first = focus_event(11, 2_000, focus_info("Terminal", 1, "shell"));
        let _ = adapter.adapt_batch(&vec![first]);

        let mut second_info = focus_info("Safari", 2, "Docs");
        second_info.pid = 101;
        let second = focus_event(12, 2_001, second_info);
        let out = adapter.adapt_batch(&vec![second]);

        assert_eq!(out.len(), 3);
        assert!(matches!(
            out[0].kind,
            EventKind::Signal(SignalKind::AppChanged)
        ));
        assert!(matches!(
            out[1].kind,
            EventKind::Hint(HintKind::FocusChanged)
        ));
        assert!(matches!(
            out[2].kind,
            EventKind::Signal(SignalKind::ActivityPulse)
        ));
    }

    #[test]
    fn window_switch_within_same_app_emits_window_changed() {
        let mut adapter = EventAdapter::new();
        let _ = adapter.adapt_batch(&vec![focus_event(
            20,
            3_000,
            focus_info("Terminal", 1, "tab1"),
        )]);
        let second = focus_event(21, 3_001, focus_info("Terminal", 2, "tab2"));
        let out = adapter.adapt_batch(&vec![second]);

        assert_eq!(out.len(), 3);
        assert!(matches!(
            out[0].kind,
            EventKind::Signal(SignalKind::WindowChanged)
        ));
        assert!(matches!(
            out[1].kind,
            EventKind::Hint(HintKind::FocusChanged)
        ));
        assert!(matches!(
            out[2].kind,
            EventKind::Signal(SignalKind::ActivityPulse)
        ));
    }

    #[test]
    fn title_change_emits_polling_hint_only() {
        let mut adapter = EventAdapter::new();
        let _ = adapter.adapt_batch(&vec![focus_event(
            30,
            4_000,
            focus_info("Terminal", 1, "tab1"),
        )]);

        let info = focus_info("Terminal", 1, "tab2");
        let out = adapter.adapt_batch(&vec![focus_event(31, 4_001, info.clone())]);

        assert_eq!(out.len(), 1);
        let evt = &out[0];
        assert!(matches!(evt.kind, EventKind::Hint(HintKind::TitleChanged)));
        assert_eq!(evt.source, EventSource::Polling);
        assert!(evt.polling);
        if let EventPayload::Title { window_id, title } = &evt.payload {
            assert_eq!(*window_id, info.window_id);
            assert_eq!(title, info.window_title.as_ref());
        } else {
            panic!("expected title payload");
        }
    }

    #[test]
    fn identical_focus_updates_emit_nothing() {
        let mut adapter = EventAdapter::new();
        let focus = focus_info("Terminal", 1, "tab1");
        let _ = adapter.adapt_batch(&vec![focus_event(40, 5_000, focus.clone())]);
        let out = adapter.adapt_batch(&vec![focus_event(41, 5_001, focus)]);
        assert!(out.is_empty());
    }
}
