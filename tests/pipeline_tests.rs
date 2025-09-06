use chrono::{DateTime, Utc};
use kronical::daemon::events::adapter::EventAdapter;
use kronical::daemon::events::derive_hint::StateDeriver;
use kronical::daemon::events::derive_signal::LockDeriver;
use kronical::daemon::events::model::EventKind;
use kronical::daemon::events::{RawEvent, WindowFocusInfo};
use kronical::daemon::records::{ActivityState, RecordBuilder};
use std::sync::Arc;

fn mk_focus(pid: i32, app: &str, wid: u32, title: &str, ts: DateTime<Utc>) -> RawEvent {
    let focus_info = WindowFocusInfo {
        pid,
        process_start_time: 42,
        app_name: Arc::new(app.to_string()),
        window_title: Arc::new(title.to_string()),
        window_id: wid,
        window_instance_start: ts,
        window_position: None,
        window_size: None,
    };
    RawEvent::WindowFocusChange {
        timestamp: ts,
        event_id: ts.timestamp_millis() as u64,
        focus_info,
    }
}

fn mk_kb(ts: DateTime<Utc>) -> RawEvent {
    RawEvent::KeyboardInput {
        timestamp: ts,
        event_id: ts.timestamp_millis() as u64,
        data: kronical::daemon::events::KeyboardEventData {
            key_code: Some(42),
            key_char: None,
            modifiers: vec![],
        },
    }
}

fn mk_mouse(ts: DateTime<Utc>) -> RawEvent {
    RawEvent::MouseInput {
        timestamp: ts,
        event_id: ts.timestamp_millis() as u64,
        data: kronical::daemon::events::MouseEventData {
            position: kronical::daemon::events::MousePosition { x: 0, y: 0 },
            button: None,
            click_count: None,
            event_type: None,
            wheel_amount: None,
            wheel_rotation: None,
            wheel_axis: None,
        },
    }
}

fn process_batch(
    adapter: &mut EventAdapter,
    lock_deriver: &mut LockDeriver,
    state_deriver: &mut StateDeriver,
    builder: &mut RecordBuilder,
    batch: Vec<RawEvent>,
) -> Vec<kronical::daemon::records::ActivityRecord> {
    let mut completed = Vec::new();
    let envs = adapter.adapt_batch(&batch);
    let envs = lock_deriver.derive(&envs);
    let mut envs_sorted = envs.clone();
    envs_sorted.sort_by_key(|e| {
        let kind_order = match e.kind {
            EventKind::Hint(_) => 0,
            EventKind::Signal(_) => 1,
        };
        (e.timestamp, kind_order)
    });
    for e in envs_sorted {
        match e.kind {
            EventKind::Hint(_) => {
                if let Some(r) = builder.on_hint(&e) {
                    completed.push(r);
                }
            }
            EventKind::Signal(_) => {
                if let Some(h) = state_deriver.on_signal(&e) {
                    if let Some(r) = builder.on_hint(&h) {
                        completed.push(r);
                    }
                }
            }
        }
    }
    completed
}

#[test]
fn test_end_to_end_stream_counts() {
    let mut adapter = EventAdapter::new();
    let mut lock_deriver = LockDeriver::new();
    let now = Utc::now();
    let mut state_deriver = StateDeriver::new(now, 30, 300);
    let mut builder = RecordBuilder::new(ActivityState::Inactive);

    let mut batch: Vec<RawEvent> = Vec::new();
    // Initial focus
    batch.push(mk_focus(100, "Safari", 1, "A", now));

    // 200 input events over 20s; title changes every 50 events; window switch at 150
    let mut expected_focus_splits = 1; // initial focus
    let mut expected_title_splits = 0;
    for i in 1..=200 {
        let ts = now + chrono::Duration::milliseconds((i * 100) as i64);
        if i % 2 == 0 {
            batch.push(mk_kb(ts));
        } else {
            batch.push(mk_mouse(ts));
        }
        if i % 50 == 0 {
            // title change
            batch.push(mk_focus(100, "Safari", 1, &format!("A-{}", i), ts));
            expected_title_splits += 1;
        }
        if i == 150 {
            // window switch
            batch.push(mk_focus(100, "Safari", 2, "B", ts));
            expected_focus_splits += 1;
        }
    }

    let mut records = process_batch(
        &mut adapter,
        &mut lock_deriver,
        &mut state_deriver,
        &mut builder,
        batch,
    );

    // Drive timeouts: Active->Passive and Passive->Inactive
    if let Some(h) = state_deriver.on_tick(now + chrono::Duration::seconds(40)) {
        if let Some(r) = builder.on_hint(&h) {
            records.push(r);
        }
    }
    if let Some(h) = state_deriver.on_tick(now + chrono::Duration::seconds(400)) {
        if let Some(r) = builder.on_hint(&h) {
            records.push(r);
        }
    }

    // Expect at least focus + title splits
    assert!(records.len() >= (expected_focus_splits + expected_title_splits) as usize);
    // First completion should be Inactive (focus before state), last state should be Inactive
    assert_eq!(records.first().unwrap().state, ActivityState::Inactive);
    assert_eq!(builder.current_state(), ActivityState::Inactive);
}

#[test]
fn test_end_to_end_with_lock() {
    let mut adapter = EventAdapter::new();
    let mut lock_deriver = LockDeriver::new();
    let now = Utc::now();
    let mut state_deriver = StateDeriver::new(now, 30, 300);
    let mut builder = RecordBuilder::new(ActivityState::Inactive);

    // Focus to app
    let mut batch: Vec<RawEvent> = vec![mk_focus(200, "Code", 1, "Edit", now)];
    // Become active
    batch.push(mk_kb(now + chrono::Duration::seconds(1)));
    // Switch to loginwindow (lock)
    batch.push(mk_focus(
        1,
        "loginwindow",
        999,
        "",
        now + chrono::Duration::seconds(5),
    ));
    // Inputs during lock
    for i in 0..10 {
        let ts = now + chrono::Duration::seconds(6 + i);
        batch.push(mk_mouse(ts));
    }
    // Unlock back to app
    batch.push(mk_focus(
        200,
        "Code",
        1,
        "Edit",
        now + chrono::Duration::seconds(20),
    ));

    let records = process_batch(
        &mut adapter,
        &mut lock_deriver,
        &mut state_deriver,
        &mut builder,
        batch,
    );

    // Expect at least three splits: initial focus, to Locked, back to Active
    assert!(records.len() >= 3);
    // Current state should be Active or Passive depending on keyboard spacing; allow either
    let st = builder.current_state();
    assert!(st == ActivityState::Active || st == ActivityState::Passive);
}
