use crate::events::{MousePosition, WheelAxis};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type StringId = u16;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactScrollSequence {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub direction: ScrollDirection,
    pub total_amount: i32,
    pub total_rotation: i32,
    pub scroll_count: u32,
    pub position: MousePosition,
    pub raw_event_ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ScrollDirection {
    VerticalUp,
    VerticalDown,
    HorizontalLeft,
    HorizontalRight,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactMouseTrajectory {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub event_type: MouseTrajectoryType,
    pub start_position: MousePosition,
    pub end_position: MousePosition,
    pub simplified_path: Vec<MousePosition>,
    pub total_distance: f32,
    pub max_velocity: f32,
    pub raw_event_ids: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MouseTrajectoryType {
    Movement,
    Drag,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactKeyboardActivity {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub keystrokes: u32,
    pub keys_per_minute: f32,
    pub density_per_sec: f32,
    pub raw_event_ids: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactFocusEvent {
    pub timestamp: DateTime<Utc>,
    pub app_name_id: StringId,
    pub window_title_id: StringId,
    pub pid: i32,
    pub window_position: Option<MousePosition>,
    pub event_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompactEvent {
    Scroll(CompactScrollSequence),
    MouseTrajectory(CompactMouseTrajectory),
    Keyboard(CompactKeyboardActivity),
    Focus(CompactFocusEvent),
}

impl CompactEvent {
    pub fn memory_size(&self) -> usize {
        match self {
            CompactEvent::Scroll(_) => std::mem::size_of::<CompactScrollSequence>(),
            CompactEvent::MouseTrajectory(t) => {
                std::mem::size_of::<CompactMouseTrajectory>()
                    + (t.simplified_path.len() * std::mem::size_of::<MousePosition>())
            }
            CompactEvent::Keyboard(_) => std::mem::size_of::<CompactKeyboardActivity>(),
            CompactEvent::Focus(_) => std::mem::size_of::<CompactFocusEvent>(),
        }
    }
}

pub fn infer_scroll_direction(amount: i32, axis: WheelAxis) -> ScrollDirection {
    match axis {
        WheelAxis::Vertical => {
            if amount > 0 {
                ScrollDirection::VerticalUp
            } else {
                ScrollDirection::VerticalDown
            }
        }
        WheelAxis::Horizontal => {
            if amount > 0 {
                ScrollDirection::HorizontalRight
            } else {
                ScrollDirection::HorizontalLeft
            }
        }
    }
}
