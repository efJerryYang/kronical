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
    pub button: Option<String>,
    pub click_count: Option<u16>,
    pub wheel_delta: Option<i16>,
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
