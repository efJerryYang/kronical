use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MousePosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardEventData {
    pub key_code: Option<u16>,
    pub key_char: Option<char>,
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MouseEventData {
    pub position: MousePosition,
    pub button: Option<MouseButton>,
    pub click_count: Option<u16>,
    // Mouse movement/drag/click classification
    pub event_type: Option<MouseEventKind>,
    // Wheel-specific fields captured from uiohook Wheel events
    // amount is the magnitude of the scroll (platform-specific units)
    pub wheel_amount: Option<i32>,
    // rotation is the signed step direction (+1 / -1)
    pub wheel_rotation: Option<i32>,
    // axis of wheel movement (Vertical or Horizontal)
    pub wheel_axis: Option<WheelAxis>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Primary,
    Secondary,
    Middle,
    Button4,
    Button5,
    Other(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MouseEventKind {
    Moved,
    Pressed,
    Released,
    Clicked,
    Dragged,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum WheelAxis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowFocusInfo {
    pub pid: i32,
    pub process_start_time: u64,
    pub app_name: Arc<String>,
    pub window_title: Arc<String>,
    pub window_id: u32,
    pub window_instance_start: DateTime<Utc>,
    pub window_position: Option<MousePosition>,
    pub window_size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RawEvent {
    KeyboardInput {
        timestamp: DateTime<Utc>,
        event_id: u64,
        data: KeyboardEventData,
    },
    MouseInput {
        timestamp: DateTime<Utc>,
        event_id: u64,
        data: MouseEventData,
    },
    WindowFocusChange {
        timestamp: DateTime<Utc>,
        event_id: u64,
        focus_info: WindowFocusInfo,
    },
}

impl RawEvent {
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            RawEvent::KeyboardInput { timestamp, .. } => *timestamp,
            RawEvent::MouseInput { timestamp, .. } => *timestamp,
            RawEvent::WindowFocusChange { timestamp, .. } => *timestamp,
        }
    }

    #[allow(dead_code)]
    pub fn event_id(&self) -> u64 {
        match self {
            RawEvent::KeyboardInput { event_id, .. } => *event_id,
            RawEvent::MouseInput { event_id, .. } => *event_id,
            RawEvent::WindowFocusChange { event_id, .. } => *event_id,
        }
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            RawEvent::KeyboardInput { .. } => "keyboard",
            RawEvent::MouseInput { .. } => "mouse",
            RawEvent::WindowFocusChange { .. } => "window_focus_change",
        }
    }
}

pub mod adapter;
pub mod derive_hint;
pub mod derive_signal;
pub mod hints;
pub mod model;
pub mod signals;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::model::{EventEnvelope, EventKind, EventPayload, EventSource, HintKind};
    use crate::events::signals::SignalKind;
    use chrono::{TimeZone, Utc};
    use std::sync::Arc;

    #[test]
    fn raw_event_helpers_return_expected_metadata() {
        let ts = Utc.with_ymd_and_hms(2024, 4, 22, 8, 0, 0).unwrap();
        let keyboard = RawEvent::KeyboardInput {
            timestamp: ts,
            event_id: 10,
            data: KeyboardEventData {
                key_code: Some(42),
                key_char: Some('k'),
                modifiers: vec!["shift".into()],
            },
        };
        assert_eq!(keyboard.timestamp(), ts);
        assert_eq!(keyboard.event_id(), 10);
        assert_eq!(keyboard.event_type(), "keyboard");

        let mouse = RawEvent::MouseInput {
            timestamp: ts,
            event_id: 11,
            data: MouseEventData {
                position: MousePosition { x: 1, y: 2 },
                button: Some(MouseButton::Primary),
                click_count: Some(1),
                event_type: Some(MouseEventKind::Clicked),
                wheel_amount: None,
                wheel_rotation: None,
                wheel_axis: None,
            },
        };
        assert_eq!(mouse.timestamp(), ts);
        assert_eq!(mouse.event_id(), 11);
        assert_eq!(mouse.event_type(), "mouse");

        let focus = RawEvent::WindowFocusChange {
            timestamp: ts,
            event_id: 12,
            focus_info: WindowFocusInfo {
                pid: 1,
                process_start_time: 2,
                app_name: Arc::new("Terminal".into()),
                window_title: Arc::new("shell".into()),
                window_id: 3,
                window_instance_start: ts,
                window_position: None,
                window_size: None,
            },
        };
        assert_eq!(focus.timestamp(), ts);
        assert_eq!(focus.event_id(), 12);
        assert_eq!(focus.event_type(), "window_focus_change");
    }

    #[test]
    fn envelope_helpers_distinguish_signal_and_hint() {
        let ts = Utc.with_ymd_and_hms(2024, 4, 22, 9, 0, 0).unwrap();
        let signal = EventEnvelope {
            id: 1,
            timestamp: ts,
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::KeyboardInput),
            payload: EventPayload::Keyboard(KeyboardEventData {
                key_code: Some(1),
                key_char: None,
                modifiers: vec![],
            }),
            derived: false,
            polling: false,
            sensitive: false,
        };
        assert!(signal.is_signal());
        assert!(!signal.is_hint());

        let hint = EventEnvelope {
            id: 2,
            timestamp: ts,
            source: EventSource::Derived,
            kind: EventKind::Hint(HintKind::TitleChanged),
            payload: EventPayload::Title {
                window_id: 9,
                title: "new title".into(),
            },
            derived: true,
            polling: false,
            sensitive: false,
        };
        assert!(hint.is_hint());
        assert!(!hint.is_signal());
    }
}
