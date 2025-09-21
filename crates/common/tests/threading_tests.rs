use std::sync::mpsc;
use std::time::Duration;

use kronical_common::threading::{ThreadRegistry, ThreadStatus};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn wait_ready(rx: &mpsc::Receiver<()>) {
    rx.recv_timeout(Duration::from_secs(1))
        .expect("thread ready");
}

#[test]
fn reserved_slots_show_as_reserved_until_spawned() -> TestResult {
    let registry = ThreadRegistry::with_slots(["alpha", "beta"]);

    let snapshot = registry.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert!(snapshot.iter().all(|s| s.status == ThreadStatus::Reserved));

    Ok(())
}

#[test]
fn spawn_uses_reserved_slot_and_updates_status() -> TestResult {
    let registry = ThreadRegistry::with_slots(["alpha", "beta"]);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let handle = registry.spawn("alpha", move || {
        ready_tx.send(()).ok();
        let _ = stop_rx.recv();
    })?;

    wait_ready(&ready_rx);

    let snapshot = registry.snapshot();
    let alpha_entry = snapshot
        .into_iter()
        .find(|s| s.name == "alpha")
        .expect("alpha slot present");
    assert_eq!(alpha_entry.status, ThreadStatus::Running);
    assert_eq!(registry.active_count(), 1);

    stop_tx.send(()).ok();
    handle.join().expect("join alpha");

    let snapshot = registry.snapshot();
    let alpha_entry = snapshot
        .into_iter()
        .find(|s| s.name == "alpha")
        .expect("alpha slot present");
    assert_eq!(alpha_entry.status, ThreadStatus::Joined);
    assert_eq!(registry.active_count(), 0);

    Ok(())
}

#[test]
fn joining_updates_status() -> TestResult {
    let registry = ThreadRegistry::new();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let handle = registry.spawn("worker", move || {
        ready_tx.send(()).ok();
        let _ = stop_rx.recv();
    })?;

    wait_ready(&ready_rx);
    assert_eq!(registry.active_count(), 1);

    stop_tx.send(()).ok();
    handle.join().expect("join worker");

    let snapshot = registry.snapshot();
    let worker = snapshot
        .into_iter()
        .find(|s| s.name == "worker")
        .expect("worker entry");
    assert_eq!(worker.status, ThreadStatus::Joined);
    assert_eq!(registry.active_count(), 0);

    Ok(())
}

#[test]
fn drop_without_join_marks_detached() -> TestResult {
    let registry = ThreadRegistry::new();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    {
        let _handle = registry.spawn("detached", move || {
            ready_tx.send(()).ok();
            let _ = stop_rx.recv();
        })?;
        wait_ready(&ready_rx);
    }

    let snapshot = registry.snapshot();
    let detached = snapshot
        .into_iter()
        .find(|s| s.name == "detached")
        .expect("detached entry");
    assert_eq!(detached.status, ThreadStatus::Detached);
    assert_eq!(registry.active_count(), 0);

    stop_tx.send(()).ok();

    Ok(())
}

#[test]
fn unreserved_spawn_gets_new_slot() -> TestResult {
    let registry = ThreadRegistry::with_slots(["alpha"]);
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = registry.spawn("gamma", move || {
        ready_tx.send(()).ok();
    })?;

    wait_ready(&ready_rx);
    handle.join().expect("join gamma");

    let snapshot = registry.snapshot();
    let gamma = snapshot
        .into_iter()
        .find(|s| s.name == "gamma")
        .expect("gamma entry");
    assert_eq!(gamma.status, ThreadStatus::Joined);

    Ok(())
}

#[test]
fn panicked_thread_records_status() -> TestResult {
    let registry = ThreadRegistry::new();
    let handle = registry.spawn("panicker", || panic!("boom"))?;
    assert!(handle.join().is_err());

    let snapshot = registry.snapshot();
    let entry = snapshot
        .into_iter()
        .find(|s| s.name == "panicker")
        .expect("panicker entry");
    assert_eq!(entry.status, ThreadStatus::Panicked);

    Ok(())
}

#[test]
fn reserve_slots_deduplicates_entries() -> TestResult {
    let registry = ThreadRegistry::with_slots(["alpha", "beta"]);
    // Attempt to reserve duplicates and a new slot.
    registry.reserve_slots(["alpha", "gamma", "gamma"]);

    let snapshot = registry.snapshot();
    assert_eq!(
        snapshot.len(),
        3,
        "registry should only contain unique slots"
    );
    let alpha_count = snapshot.iter().filter(|s| s.name == "alpha").count();
    assert_eq!(alpha_count, 1, "duplicate reserve should be ignored");

    Ok(())
}

#[test]
fn active_thread_names_reflect_running_threads() -> TestResult {
    let registry = ThreadRegistry::new();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let handle = registry.spawn("active", move || {
        ready_tx.send(()).ok();
        let _ = stop_rx.recv();
    })?;

    wait_ready(&ready_rx);
    let active = registry.active_thread_names();
    assert_eq!(active, vec!["active".to_string()]);

    stop_tx.send(()).ok();
    handle.join().expect("join active");

    assert!(registry.active_thread_names().is_empty());

    Ok(())
}

#[test]
fn thread_handle_name_exposes_label() -> TestResult {
    let registry = ThreadRegistry::new();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (stop_tx, stop_rx) = mpsc::channel();
    let handle = registry.spawn("named", move || {
        ready_tx.send(()).ok();
        let _ = stop_rx.recv();
    })?;

    wait_ready(&ready_rx);
    assert_eq!(handle.name(), "named");

    stop_tx.send(()).ok();
    handle.join().expect("join named");

    Ok(())
}
