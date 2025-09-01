use chrono::Utc;
use kronical::daemon::event_adapter::EventAdapter;
use kronical::daemon::event_model::{EventKind, HintKind};
use kronical::daemon::events::{RawEvent, WindowFocusInfo};

#[test]
fn title_only_change_emits_only_hint() {
    let mut adapter = EventAdapter::new();
    let t0 = Utc::now();
    let base = WindowFocusInfo {
        pid: 999,
        process_start_time: 1,
        app_name: "Safari".into(),
        window_title: "A".into(),
        window_id: "w1".into(),
        window_instance_start: t0,
        window_position: None,
        window_size: None,
    };

    // First observation sets baseline (emits multiple items, legacy + new model)
    let first = RawEvent::WindowFocusChange {
        timestamp: t0,
        event_id: 1,
        focus_info: base.clone(),
    };
    let _ = adapter.adapt_batch(&vec![first]);

    // Second observation: same app/window, different title
    let mut second_fi = base.clone();
    second_fi.window_title = "B".into();
    let second = RawEvent::WindowFocusChange {
        timestamp: t0 + chrono::Duration::seconds(1),
        event_id: 2,
        focus_info: second_fi,
    };
    let out = adapter.adapt_batch(&vec![second]);

    // Ensure all outputs are hints and exactly TitleChanged
    assert!(!out.is_empty());
    for e in &out {
        match &e.kind {
            EventKind::Hint(HintKind::TitleChanged) => {}
            _ => panic!(
                "unexpected envelope kind from title-only change: {:?}",
                e.kind
            ),
        }
    }
}
