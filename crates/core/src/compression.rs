use crate::events::{MousePosition, WheelAxis};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type StringId = u16;

pub const DEFAULT_RAW_EVENT_TABLE: &str = "raw_events";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEventPointer {
    pub table: String,
    pub ids: Vec<u64>,
}

impl RawEventPointer {
    pub fn new(table: impl Into<String>, ids: Vec<u64>) -> Option<Self> {
        if ids.is_empty() {
            None
        } else {
            Some(Self {
                table: table.into(),
                ids,
            })
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactScrollSequence {
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub direction: ScrollDirection,
    pub total_amount: i32,
    pub total_rotation: i32,
    pub scroll_count: u32,
    pub position: MousePosition,
    pub raw_event_ids: Option<RawEventPointer>,
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
    pub raw_event_ids: Option<RawEventPointer>,
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
    pub raw_event_ids: Option<RawEventPointer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactFocusEvent {
    pub timestamp: DateTime<Utc>,
    pub app_name_id: StringId,
    pub window_title_id: StringId,
    pub pid: i32,
    pub window_position: Option<MousePosition>,
    pub raw_event_ids: Option<RawEventPointer>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn ts(sec: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 4, 22, 12, 0, sec).unwrap()
    }

    #[test]
    fn memory_size_accounts_for_dynamic_path_length() {
        let trajectory = CompactMouseTrajectory {
            start_time: ts(0),
            end_time: ts(1),
            event_type: MouseTrajectoryType::Movement,
            start_position: MousePosition { x: 0, y: 0 },
            end_position: MousePosition { x: 10, y: 10 },
            simplified_path: vec![MousePosition { x: 2, y: 2 }, MousePosition { x: 5, y: 5 }],
            total_distance: 14.1,
            max_velocity: 7.0,
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![1, 2, 3]),
        };

        let evt = CompactEvent::MouseTrajectory(trajectory.clone());
        let base = std::mem::size_of::<CompactMouseTrajectory>();
        let per_point = std::mem::size_of::<MousePosition>();
        assert_eq!(
            evt.memory_size(),
            base + trajectory.simplified_path.len() * per_point
        );

        let keyboard = CompactEvent::Keyboard(CompactKeyboardActivity {
            start_time: ts(0),
            end_time: ts(5),
            keystrokes: 10,
            keys_per_minute: 120.0,
            density_per_sec: 2.0,
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![10, 11]),
        });
        assert_eq!(
            keyboard.memory_size(),
            std::mem::size_of::<CompactKeyboardActivity>()
        );

        let focus = CompactEvent::Focus(CompactFocusEvent {
            timestamp: ts(2),
            app_name_id: 1,
            window_title_id: 2,
            pid: 4242,
            window_position: Some(MousePosition { x: 20, y: 30 }),
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![9]),
        });
        assert_eq!(
            focus.memory_size(),
            std::mem::size_of::<CompactFocusEvent>()
        );
    }

    #[test]
    fn scroll_memory_size_matches_struct_size() {
        let scroll = CompactScrollSequence {
            start_time: ts(0),
            end_time: ts(1),
            direction: ScrollDirection::VerticalDown,
            total_amount: -5,
            total_rotation: -5,
            scroll_count: 3,
            position: MousePosition { x: 10, y: 20 },
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![10, 11]),
        };
        let event = CompactEvent::Scroll(scroll.clone());
        assert_eq!(
            event.memory_size(),
            std::mem::size_of::<CompactScrollSequence>()
        );
    }

    #[test]
    fn infers_scroll_direction_for_axis_and_sign() {
        assert_eq!(
            infer_scroll_direction(4, WheelAxis::Vertical),
            ScrollDirection::VerticalUp
        );
        assert_eq!(
            infer_scroll_direction(-3, WheelAxis::Vertical),
            ScrollDirection::VerticalDown
        );
        assert_eq!(
            infer_scroll_direction(2, WheelAxis::Horizontal),
            ScrollDirection::HorizontalRight
        );
        assert_eq!(
            infer_scroll_direction(-1, WheelAxis::Horizontal),
            ScrollDirection::HorizontalLeft
        );
    }
}
