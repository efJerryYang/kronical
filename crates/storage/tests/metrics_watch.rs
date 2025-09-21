use chrono::{TimeZone, Utc};
use kronical_storage::{
    dec_backlog,
    inc_backlog,
    set_last_flush,
    storage_metrics_watch,
};
use std::sync::{Mutex, OnceLock};

fn with_metrics_lock<T>(f: impl FnOnce() -> T) -> T {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("metrics lock poisoned");
    let out = f();
    drop(guard);
    out
}

#[test]
fn metrics_channel_reflects_backlog_and_flush_updates() {
    with_metrics_lock(|| {
        let rx = storage_metrics_watch();
        let baseline = { rx.borrow().clone() };

        inc_backlog();
        let after_inc = { rx.borrow().clone() };
        assert_eq!(after_inc.backlog_count, baseline.backlog_count + 1);

        dec_backlog();
        let after_dec = { rx.borrow().clone() };
        assert_eq!(after_dec.backlog_count, baseline.backlog_count);

        let flush_time = Utc.with_ymd_and_hms(2024, 4, 22, 12, 0, 0).unwrap();
        set_last_flush(flush_time);
        let after_flush = { rx.borrow().clone() };
        assert_eq!(after_flush.last_flush_at, Some(flush_time));

        if let Some(prev) = baseline.last_flush_at {
            set_last_flush(prev);
        } else {
            // Reset to the "no flush yet" sentinel.
            let reset = Utc.timestamp_opt(0, 0).unwrap();
            set_last_flush(reset);
        }
    });
}

#[test]
fn new_subscribers_observe_latest_metrics_snapshot() {
    with_metrics_lock(|| {
        let rx = storage_metrics_watch();
        let baseline = { rx.borrow().clone() };

        inc_backlog();
        let after_inc = { rx.borrow().clone() };
        assert_eq!(after_inc.backlog_count, baseline.backlog_count + 1);

        let subscriber = storage_metrics_watch();
        let snapshot = { subscriber.borrow().clone() };
        assert_eq!(snapshot.backlog_count, after_inc.backlog_count);
        assert_eq!(snapshot.last_flush_at, after_inc.last_flush_at);

        dec_backlog();

        if let Some(prev) = baseline.last_flush_at {
            set_last_flush(prev);
        } else {
            let reset = Utc.timestamp_opt(0, 0).unwrap();
            set_last_flush(reset);
        }
    });
}
