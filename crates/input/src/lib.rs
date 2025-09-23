pub mod mouse {
    use kronical_core::events::{MouseButton, MouseEventKind};
    use uiohook_rs::hook::mouse::{
        MouseButton as HookMouseButton, MouseEventType as HookMouseEventType,
    };

    /// Convert a uiohook mouse button enum into the internal representation.
    pub fn button_from_hook(button: HookMouseButton) -> Option<MouseButton> {
        match button {
            HookMouseButton::NoButton => None,
            HookMouseButton::Button1 => Some(MouseButton::Primary),
            HookMouseButton::Button2 => Some(MouseButton::Secondary),
            HookMouseButton::Button3 => Some(MouseButton::Middle),
            HookMouseButton::Button4 => Some(MouseButton::Button4),
            HookMouseButton::Button5 => Some(MouseButton::Button5),
        }
    }

    /// Convert a uiohook mouse event type into the internal classification enum.
    pub fn event_kind_from_hook(event_type: HookMouseEventType) -> MouseEventKind {
        match event_type {
            HookMouseEventType::Moved => MouseEventKind::Moved,
            HookMouseEventType::Pressed => MouseEventKind::Pressed,
            HookMouseEventType::Released => MouseEventKind::Released,
            HookMouseEventType::Clicked => MouseEventKind::Clicked,
            HookMouseEventType::Dragged => MouseEventKind::Dragged,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn converts_mouse_buttons() {
            assert_eq!(button_from_hook(HookMouseButton::NoButton), None);
            assert_eq!(
                button_from_hook(HookMouseButton::Button1),
                Some(MouseButton::Primary)
            );
            assert_eq!(
                button_from_hook(HookMouseButton::Button2),
                Some(MouseButton::Secondary)
            );
            assert_eq!(
                button_from_hook(HookMouseButton::Button3),
                Some(MouseButton::Middle)
            );
            assert_eq!(
                button_from_hook(HookMouseButton::Button4),
                Some(MouseButton::Button4)
            );
            assert_eq!(
                button_from_hook(HookMouseButton::Button5),
                Some(MouseButton::Button5)
            );
        }

        #[test]
        fn converts_mouse_event_kind() {
            assert_eq!(
                event_kind_from_hook(HookMouseEventType::Moved),
                MouseEventKind::Moved
            );
            assert_eq!(
                event_kind_from_hook(HookMouseEventType::Pressed),
                MouseEventKind::Pressed
            );
            assert_eq!(
                event_kind_from_hook(HookMouseEventType::Released),
                MouseEventKind::Released
            );
            assert_eq!(
                event_kind_from_hook(HookMouseEventType::Clicked),
                MouseEventKind::Clicked
            );
            assert_eq!(
                event_kind_from_hook(HookMouseEventType::Dragged),
                MouseEventKind::Dragged
            );
        }
    }
}
