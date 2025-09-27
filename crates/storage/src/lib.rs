use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use tokio::sync::watch;

use kronical_core::compression::CompactEvent;
use kronical_core::events::RawEvent;
use kronical_core::events::model::EventEnvelope;
use kronical_core::records::ActivityRecord;
use kronical_core::snapshot;

pub trait StorageBackend: Send + Sync {
    fn add_events(&mut self, events: Vec<RawEvent>) -> Result<()>;
    fn add_compact_events(&mut self, events: Vec<CompactEvent>) -> Result<()>;
    fn add_envelopes(&mut self, events: Vec<EventEnvelope>) -> Result<()>;
    fn add_records(&mut self, records: Vec<ActivityRecord>) -> Result<()>;
    fn add_transitions(&mut self, transitions: Vec<snapshot::Transition>) -> Result<()>;
    fn fetch_records_since(&mut self, since: DateTime<Utc>) -> Result<Vec<ActivityRecord>>;
    fn fetch_envelopes_between(
        &mut self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<EventEnvelope>>;
    fn fetch_recent_transitions(&mut self, limit: usize) -> Result<Vec<snapshot::Transition>>;
}

static STORAGE_BACKLOG: AtomicU64 = AtomicU64::new(0);
static LAST_FLUSH_AT_EPOCH: AtomicI64 = AtomicI64::new(0);

static METRICS_CH: OnceCell<(
    watch::Sender<StorageMetrics>,
    watch::Receiver<StorageMetrics>,
)> = OnceCell::new();

fn init_metrics_channel() -> &'static (
    watch::Sender<StorageMetrics>,
    watch::Receiver<StorageMetrics>,
) {
    METRICS_CH.get_or_init(|| {
        let initial = storage_metrics_snapshot();
        watch::channel(initial)
    })
}

pub fn inc_backlog() {
    let _ = STORAGE_BACKLOG.fetch_add(1, Ordering::Relaxed);
    publish_metrics();
}

pub fn dec_backlog() {
    let _ = STORAGE_BACKLOG.fetch_sub(1, Ordering::Relaxed);
    publish_metrics();
}

pub fn set_last_flush(t: DateTime<Utc>) {
    let secs = t.timestamp();
    LAST_FLUSH_AT_EPOCH.store(secs, Ordering::Relaxed);
    publish_metrics();
}

#[derive(Clone, Debug)]
pub struct StorageMetrics {
    pub backlog_count: u64,
    pub last_flush_at: Option<DateTime<Utc>>,
}

fn storage_metrics_snapshot() -> StorageMetrics {
    let secs = LAST_FLUSH_AT_EPOCH.load(Ordering::Relaxed);
    let last = if secs > 0 {
        Utc.timestamp_opt(secs, 0).single()
    } else {
        None
    };
    StorageMetrics {
        backlog_count: STORAGE_BACKLOG.load(Ordering::Relaxed),
        last_flush_at: last,
    }
}

fn publish_metrics() {
    let (tx, _rx) = init_metrics_channel();
    let _ = tx.send(storage_metrics_snapshot());
}

pub fn storage_metrics_watch() -> watch::Receiver<StorageMetrics> {
    let (_tx, rx) = init_metrics_channel();
    rx.clone()
}

#[derive(Clone, Debug)]
pub enum StorageCommand {
    RawEvent(RawEvent),
    Record(ActivityRecord),
    Envelope(EventEnvelope),
    CompactEvents(Vec<CompactEvent>),
    Transition(snapshot::Transition),
    Shutdown,
}

pub mod duckdb;
pub mod sqlite3;
pub mod system_metrics;

#[cfg(test)]
mod tests {
    use super::duckdb::DuckDbStorage;
    use super::sqlite3::SqliteStorage;
    use super::*;
    use ::duckdb::{Connection as DuckDbConnection, params as duckdb_params};
    use chrono::{Duration, TimeZone, Utc as ChronoUtc};
    use once_cell::sync::Lazy;
    use rusqlite::Connection as SqliteConnection;
    use std::sync::{Arc, Mutex};
    use std::{thread, time::Duration as StdDuration};

    use kronical_common::threading::ThreadRegistry;

    use kronical_core::compression::{
        CompactEvent, CompactFocusEvent, CompactKeyboardActivity, CompactMouseTrajectory,
        CompactScrollSequence, DEFAULT_RAW_EVENT_TABLE, MouseTrajectoryType, RawEventPointer,
        ScrollDirection, StringId,
    };
    use kronical_core::events::model::{
        EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
    };
    use kronical_core::events::{KeyboardEventData, MousePosition, RawEvent, WindowFocusInfo};
    use kronical_core::records::{ActivityRecord, ActivityState};
    use kronical_core::snapshot;
    use tempfile::tempdir;

    static TEST_GUARD: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    #[derive(Clone)]
    struct SampleData {
        base_ts: chrono::DateTime<chrono::Utc>,
        focus_info: WindowFocusInfo,
        raw_event: RawEvent,
        keyboard_env: EventEnvelope,
        state_env: EventEnvelope,
        record: ActivityRecord,
        compact_events: Vec<CompactEvent>,
    }

    fn with_clean_state<F: FnOnce(watch::Receiver<StorageMetrics>)>(f: F) {
        let _lock = TEST_GUARD.lock().expect("storage test mutex poisoned");
        STORAGE_BACKLOG.store(0, Ordering::Relaxed);
        LAST_FLUSH_AT_EPOCH.store(0, Ordering::Relaxed);
        publish_metrics();
        let rx = storage_metrics_watch();
        f(rx);
        STORAGE_BACKLOG.store(0, Ordering::Relaxed);
        LAST_FLUSH_AT_EPOCH.store(0, Ordering::Relaxed);
        publish_metrics();
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if predicate() {
                return true;
            }
            thread::sleep(StdDuration::from_millis(10));
        }
        false
    }

    fn build_sample_data() -> SampleData {
        let base_ts = ChronoUtc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap();
        let focus_info = WindowFocusInfo {
            pid: 42,
            process_start_time: 123_456,
            app_name: Arc::new("Chronicle".to_string()),
            window_title: Arc::new("Coverage".to_string()),
            window_id: 777,
            window_instance_start: base_ts,
            window_position: Some(MousePosition { x: 10, y: 20 }),
            window_size: Some((1280, 720)),
        };

        let raw_event = RawEvent::KeyboardInput {
            timestamp: base_ts,
            event_id: 1,
            data: KeyboardEventData {
                key_code: Some(13),
                key_char: Some('\n'),
                modifiers: vec!["cmd".into()],
            },
        };

        let keyboard_env = EventEnvelope {
            id: 10,
            timestamp: base_ts + Duration::seconds(1),
            source: EventSource::Hook,
            kind: EventKind::Signal(SignalKind::KeyboardInput),
            payload: EventPayload::Keyboard(KeyboardEventData {
                key_code: Some(42),
                key_char: Some('*'),
                modifiers: vec!["shift".into()],
            }),
            derived: false,
            polling: false,
            sensitive: false,
        };

        let state_env = EventEnvelope {
            id: 11,
            timestamp: base_ts + Duration::seconds(2),
            source: EventSource::Derived,
            kind: EventKind::Hint(HintKind::StateChanged),
            payload: EventPayload::State {
                from: ActivityState::Inactive,
                to: ActivityState::Active,
            },
            derived: true,
            polling: false,
            sensitive: false,
        };

        let record = ActivityRecord {
            record_id: 5,
            start_time: base_ts,
            end_time: Some(base_ts + Duration::seconds(5)),
            state: ActivityState::Active,
            focus_info: Some(focus_info.clone()),
            event_count: 3,
            triggering_events: vec![1, 2, 3],
        };

        let scroll = CompactEvent::Scroll(CompactScrollSequence {
            start_time: base_ts,
            end_time: base_ts + Duration::seconds(1),
            direction: ScrollDirection::VerticalDown,
            total_amount: -10,
            total_rotation: -10,
            scroll_count: 2,
            position: MousePosition { x: 0, y: 0 },
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![1]),
        });

        let keyboard_compact = CompactEvent::Keyboard(CompactKeyboardActivity {
            start_time: base_ts,
            end_time: base_ts + Duration::seconds(1),
            keystrokes: 4,
            keys_per_minute: 240.0,
            density_per_sec: 4.0,
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![1]),
        });

        let trajectory = CompactEvent::MouseTrajectory(CompactMouseTrajectory {
            start_time: base_ts + Duration::seconds(2),
            end_time: base_ts + Duration::seconds(3),
            event_type: MouseTrajectoryType::Movement,
            start_position: MousePosition { x: 10, y: 10 },
            end_position: MousePosition { x: 20, y: 20 },
            simplified_path: vec![
                MousePosition { x: 10, y: 10 },
                MousePosition { x: 20, y: 20 },
            ],
            total_distance: 28.0,
            max_velocity: 14.0,
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![2, 3]),
        });

        let focus_compact = CompactEvent::Focus(CompactFocusEvent {
            timestamp: base_ts + Duration::seconds(4),
            app_name_id: 1 as StringId,
            window_title_id: 2 as StringId,
            pid: 42,
            window_position: Some(MousePosition { x: 50, y: 60 }),
            raw_event_ids: RawEventPointer::new(DEFAULT_RAW_EVENT_TABLE, vec![99]),
        });

        SampleData {
            base_ts,
            focus_info,
            raw_event,
            keyboard_env,
            state_env,
            record,
            compact_events: vec![scroll, keyboard_compact, trajectory, focus_compact],
        }
    }

    fn enqueue_sample_data<B: StorageBackend>(storage: &mut B, sample: &SampleData) -> Result<()> {
        storage.add_events(vec![sample.raw_event.clone()])?;
        storage.add_envelopes(vec![sample.keyboard_env.clone(), sample.state_env.clone()])?;
        storage.add_records(vec![sample.record.clone()])?;
        storage.add_compact_events(sample.compact_events.clone())?;
        Ok(())
    }

    #[test]
    fn metrics_watch_tracks_backlog_changes() {
        with_clean_state(|rx| {
            assert_eq!(rx.borrow().backlog_count, 0);

            inc_backlog();
            assert_eq!(rx.borrow().backlog_count, 1);

            inc_backlog();
            assert_eq!(rx.borrow().backlog_count, 2);

            dec_backlog();
            assert_eq!(rx.borrow().backlog_count, 1);
        });
    }

    #[test]
    fn metrics_watch_updates_last_flush_timestamp() {
        with_clean_state(|rx| {
            assert!(rx.borrow().last_flush_at.is_none());

            let timestamp = Utc.with_ymd_and_hms(2024, 5, 1, 12, 30, 45).unwrap();
            set_last_flush(timestamp);
            assert_eq!(rx.borrow().last_flush_at, Some(timestamp));

            LAST_FLUSH_AT_EPOCH.store(0, Ordering::Relaxed);
            publish_metrics();
            assert!(rx.borrow().last_flush_at.is_none());
        });
    }

    #[test]
    fn storage_metrics_watch_clones_receive_updates() {
        with_clean_state(|rx1| {
            let rx2 = storage_metrics_watch();

            inc_backlog();
            assert_eq!(rx1.borrow().backlog_count, 1);
            assert_eq!(rx2.borrow().backlog_count, 1);

            dec_backlog();
            assert_eq!(rx1.borrow().backlog_count, 0);
            assert_eq!(rx2.borrow().backlog_count, 0);
        });
    }

    #[test]
    fn duckdb_storage_persists_and_fetches_data() {
        with_clean_state(|rx| {
            let dir = tempdir().expect("tempdir");
            let db_path = dir.path().join("store.duckdb");
            let bus = Arc::new(snapshot::SnapshotBus::new());
            let sample = build_sample_data();

            let mut storage = DuckDbStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("duckdb init");

            assert_eq!(rx.borrow().backlog_count, 0);
            storage
                .add_compact_events(Vec::new())
                .expect("empty compact events do not enqueue");
            assert_eq!(rx.borrow().backlog_count, 0);

            enqueue_sample_data(&mut storage, &sample).expect("sample data enqueued");

            assert!(
                wait_until(|| rx.borrow().backlog_count == 0),
                "writer drained backlog"
            );

            drop(storage); // ensure commit + shutdown

            assert!(wait_until(|| rx.borrow().last_flush_at.is_some()));
            assert!(bus.current_health().is_empty());

            let conn = DuckDbConnection::open(&db_path).expect("open duckdb");
            let raw_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM raw_events", [], |row| row.get(0))
                .expect("raw_events count");
            assert_eq!(raw_count, 1);

            let env_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM raw_envelopes", [], |row| row.get(0))
                .expect("raw_envelopes count");
            assert_eq!(env_count, 2);

            let record_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM activity_records", [], |row| {
                    row.get(0)
                })
                .expect("activity_records count");
            assert_eq!(record_count, 1);

            let compact_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM compact_events", [], |row| row.get(0))
                .expect("compact_events count");
            assert_eq!(compact_count, 4);

            drop(conn);

            let mut reader = DuckDbStorage::new_with_limit(
                &db_path,
                32,
                Arc::new(snapshot::SnapshotBus::new()),
                ThreadRegistry::new(),
            )
            .expect("duckdb reader");

            let since = sample.base_ts - Duration::seconds(10);
            let records = reader
                .fetch_records_since(since)
                .expect("record fetch succeeds");
            assert_eq!(records.len(), 1);
            let fetched = &records[0];
            assert_eq!(fetched.state, sample.record.state);
            assert!(fetched.end_time.is_some());
            let fetched_focus = fetched.focus_info.as_ref().expect("focus info fetched");
            assert_eq!(fetched_focus.window_id, sample.focus_info.window_id);
            assert_eq!(&*fetched_focus.app_name, &*sample.focus_info.app_name);
            assert_eq!(fetched.event_count, 0);
            assert!(fetched.triggering_events.is_empty());

            let envelopes = reader
                .fetch_envelopes_between(since, sample.base_ts + Duration::seconds(10))
                .expect("envelope fetch succeeds");
            assert_eq!(envelopes.len(), 2);

            let mut saw_keyboard = false;
            let mut saw_state = false;
            for env in envelopes {
                match env.kind {
                    EventKind::Signal(SignalKind::KeyboardInput) => {
                        if let EventPayload::Keyboard(data) = env.payload {
                            assert_eq!(data.key_code, Some(42));
                            saw_keyboard = true;
                        }
                    }
                    EventKind::Hint(HintKind::StateChanged) => {
                        if let EventPayload::State { from, to } = env.payload {
                            assert_eq!(from, ActivityState::Inactive);
                            assert_eq!(to, ActivityState::Active);
                            saw_state = true;
                        }
                    }
                    _ => {}
                }
            }
            assert!(saw_keyboard && saw_state);

            drop(reader);
        });
    }

    #[test]
    fn sqlite_storage_persists_and_fetches_data() {
        with_clean_state(|rx| {
            let dir = tempdir().expect("tempdir");
            let db_path = dir.path().join("store.sqlite3");
            let bus = Arc::new(snapshot::SnapshotBus::new());
            let sample = build_sample_data();

            let mut storage = SqliteStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("sqlite init");
            assert_eq!(rx.borrow().backlog_count, 0);
            storage
                .add_compact_events(Vec::new())
                .expect("empty compact events do not enqueue");
            assert_eq!(rx.borrow().backlog_count, 0);
            enqueue_sample_data(&mut storage, &sample).expect("sample data enqueued");

            assert!(wait_until(|| rx.borrow().backlog_count == 0));

            drop(storage);

            assert!(wait_until(|| rx.borrow().last_flush_at.is_some()));
            assert!(bus.current_health().is_empty());

            let conn = SqliteConnection::open(&db_path).expect("open sqlite");
            let raw_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM raw_events", [], |row| row.get(0))
                .expect("raw_events count");
            assert_eq!(raw_count, 1);

            let env_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM raw_envelopes", [], |row| row.get(0))
                .expect("raw_envelopes count");
            assert_eq!(env_count, 2);

            let record_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM activity_records", [], |row| {
                    row.get(0)
                })
                .expect("activity_records count");
            assert_eq!(record_count, 1);

            let compact_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM compact_events", [], |row| row.get(0))
                .expect("compact_events count");
            assert_eq!(compact_count, 4);

            drop(conn);

            let mut reader = SqliteStorage::new(
                &db_path,
                Arc::new(snapshot::SnapshotBus::new()),
                ThreadRegistry::new(),
            )
            .expect("sqlite reader");

            let since = sample.base_ts - Duration::seconds(10);
            let records = reader
                .fetch_records_since(since)
                .expect("record fetch succeeds");
            assert_eq!(records.len(), 1);
            let fetched = &records[0];
            assert_eq!(fetched.state, sample.record.state);
            assert!(fetched.end_time.is_some());
            let fetched_focus = fetched.focus_info.as_ref().expect("focus info fetched");
            assert_eq!(fetched_focus.window_id, sample.focus_info.window_id);
            assert_eq!(&*fetched_focus.app_name, &*sample.focus_info.app_name);
            assert_eq!(fetched.event_count, 0);
            assert!(fetched.triggering_events.is_empty());

            let envelopes = reader
                .fetch_envelopes_between(since, sample.base_ts + Duration::seconds(10))
                .expect("envelope fetch succeeds");
            assert_eq!(envelopes.len(), 2);

            let mut saw_keyboard = false;
            let mut saw_state = false;
            for env in envelopes {
                match env.kind {
                    EventKind::Signal(SignalKind::KeyboardInput) => {
                        if let EventPayload::Keyboard(data) = env.payload {
                            assert_eq!(data.key_code, Some(42));
                            saw_keyboard = true;
                        }
                    }
                    EventKind::Hint(HintKind::StateChanged) => {
                        if let EventPayload::State { from, to } = env.payload {
                            assert_eq!(from, ActivityState::Inactive);
                            assert_eq!(to, ActivityState::Active);
                            saw_state = true;
                        }
                    }
                    _ => {}
                }
            }
            assert!(saw_keyboard && saw_state);

            drop(reader);
        });
    }

    #[test]
    fn duckdb_writer_commits_every_hundred_commands() {
        with_clean_state(|rx| {
            let dir = tempdir().expect("tempdir");
            let db_path = dir.path().join("batch.duckdb");
            let bus = Arc::new(snapshot::SnapshotBus::new());
            let sample = build_sample_data();

            let mut storage = DuckDbStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("duckdb init");

            for i in 0..100 {
                let mut raw = sample.raw_event.clone();
                if let RawEvent::KeyboardInput {
                    ref mut timestamp,
                    ref mut event_id,
                    ..
                } = raw
                {
                    *timestamp = sample.base_ts + Duration::milliseconds(i as i64);
                    *event_id = 1_000 + i as u64;
                }
                storage.add_events(vec![raw]).expect("raw event enqueued");
            }

            assert!(
                wait_until(|| rx.borrow().backlog_count == 0),
                "writer should drain backlog after batch commit",
            );
            assert!(
                wait_until(|| rx.borrow().last_flush_at.is_some()),
                "batch commit should update last flush timestamp",
            );
            assert!(bus.current_health().is_empty());

            drop(storage);
        });
    }

    #[test]
    fn sqlite_writer_commits_every_hundred_commands() {
        with_clean_state(|rx| {
            let dir = tempdir().expect("tempdir");
            let db_path = dir.path().join("batch.sqlite3");
            let bus = Arc::new(snapshot::SnapshotBus::new());
            let sample = build_sample_data();

            let mut storage = SqliteStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("sqlite init");

            for i in 0..100 {
                let mut raw = sample.raw_event.clone();
                if let RawEvent::KeyboardInput {
                    ref mut timestamp,
                    ref mut event_id,
                    ..
                } = raw
                {
                    *timestamp = sample.base_ts + Duration::milliseconds(i as i64);
                    *event_id = 2_000 + i as u64;
                }
                storage.add_events(vec![raw]).expect("raw event enqueued");
            }

            assert!(
                wait_until(|| rx.borrow().backlog_count == 0),
                "writer should drain backlog after batch commit",
            );
            assert!(
                wait_until(|| rx.borrow().last_flush_at.is_some()),
                "batch commit should update last flush timestamp",
            );
            assert!(bus.current_health().is_empty());

            drop(storage);
        });
    }

    #[test]
    fn duckdb_fetch_envelopes_decode_variant_payloads() {
        with_clean_state(|rx| {
            let dir = tempdir().expect("tempdir");
            let db_path = dir.path().join("variants.duckdb");
            let bus = Arc::new(snapshot::SnapshotBus::new());
            let sample = build_sample_data();

            let mut storage = DuckDbStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("duckdb init");

            let focus_env = EventEnvelope {
                id: 50,
                timestamp: sample.base_ts + Duration::seconds(3),
                source: EventSource::Hook,
                kind: EventKind::Signal(SignalKind::AppChanged),
                payload: EventPayload::Focus(sample.focus_info.clone()),
                derived: false,
                polling: false,
                sensitive: true,
            };

            let lock_env = EventEnvelope {
                id: 51,
                timestamp: sample.base_ts + Duration::seconds(4),
                source: EventSource::Derived,
                kind: EventKind::Signal(SignalKind::LockStart),
                payload: EventPayload::Lock {
                    reason: "manual lock".to_string(),
                },
                derived: true,
                polling: false,
                sensitive: false,
            };

            let title_env = EventEnvelope {
                id: 52,
                timestamp: sample.base_ts + Duration::seconds(5),
                source: EventSource::Polling,
                kind: EventKind::Hint(HintKind::TitleChanged),
                payload: EventPayload::Title {
                    window_id: 999,
                    title: "New Title".to_string(),
                },
                derived: true,
                polling: true,
                sensitive: false,
            };

            storage
                .add_envelopes(vec![focus_env.clone(), lock_env.clone(), title_env.clone()])
                .expect("envelopes enqueue");

            assert!(wait_until(|| rx.borrow().backlog_count == 0));
            drop(storage);

            let conn = DuckDbConnection::open(&db_path).expect("open duckdb");
            conn.execute(
                "INSERT INTO raw_envelopes (event_id, timestamp, source, kind, payload, derived, polling, sensitive) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                duckdb_params![
                    Option::<i64>::None,
                    sample.base_ts + Duration::seconds(6),
                    "derived",
                    "hint:focus_changed",
                    Option::<&str>::None,
                    false,
                    false,
                    false,
                ],
            )
            .expect("insert manual row");
            drop(conn);

            let mut reader = DuckDbStorage::new_with_limit(
                &db_path,
                32,
                Arc::new(snapshot::SnapshotBus::new()),
                ThreadRegistry::new(),
            )
            .expect("duckdb reader");

            let envelopes = reader
                .fetch_envelopes_between(sample.base_ts, sample.base_ts + Duration::seconds(10))
                .expect("fetch envelopes");
            assert_eq!(envelopes.len(), 4);

            let mut saw_focus = false;
            let mut saw_lock = false;
            let mut saw_title = false;
            let mut saw_empty = false;

            for env in envelopes {
                match env.kind {
                    EventKind::Signal(SignalKind::AppChanged) => {
                        if let EventPayload::Focus(focus) = env.payload {
                            assert_eq!(focus.window_id, sample.focus_info.window_id);
                            assert!(env.sensitive);
                            saw_focus = true;
                        }
                    }
                    EventKind::Signal(SignalKind::LockStart) => {
                        if let EventPayload::Lock { reason } = env.payload {
                            assert_eq!(reason, "\"manual lock\"");
                            assert!(env.derived);
                            saw_lock = true;
                        }
                    }
                    EventKind::Hint(HintKind::TitleChanged) => {
                        if let EventPayload::Title { window_id, title } = env.payload {
                            assert_eq!(window_id, 999);
                            assert_eq!(title, "New Title");
                            assert!(env.polling);
                            saw_title = true;
                        }
                    }
                    EventKind::Hint(HintKind::FocusChanged) => {
                        assert_eq!(env.id, 0);
                        match env.payload {
                            EventPayload::None => {
                                saw_empty = true;
                            }
                            other => panic!("expected empty payload, got {:?}", other),
                        }
                    }
                    other => panic!("unexpected envelope kind: {:?}", other),
                }
            }

            assert!(saw_focus && saw_lock && saw_title && saw_empty);

            drop(reader);
        });
    }

    #[test]
    fn duckdb_fetch_records_unknown_state_defaults_to_inactive() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("unknown_state.duckdb");
        let bus = Arc::new(snapshot::SnapshotBus::new());
        let sample = build_sample_data();

        {
            let _storage = DuckDbStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("duckdb init");
        }

        let conn = DuckDbConnection::open(&db_path).expect("open duckdb");
        conn.execute(
            "INSERT INTO activity_records (id, start_time, end_time, state, focus_info) VALUES (?, ?, ?, ?, ?)",
            duckdb_params![
                900_i64,
                sample.base_ts,
                sample.base_ts + Duration::seconds(2),
                "Mystery",
                serde_json::to_string(&sample.focus_info).expect("focus json"),
            ],
        )
        .expect("insert manual record");

        let mut reader = DuckDbStorage::new_with_limit(
            &db_path,
            32,
            Arc::new(snapshot::SnapshotBus::new()),
            ThreadRegistry::new(),
        )
        .expect("duckdb reader");
        let records = reader
            .fetch_records_since(sample.base_ts - Duration::seconds(5))
            .expect("fetch records");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.state, ActivityState::Inactive);
        assert_eq!(record.record_id, 900);
        assert!(record.end_time.is_some());
        drop(reader);
    }

    #[test]
    fn sqlite_fetch_envelopes_decode_variant_payloads() {
        with_clean_state(|rx| {
            let dir = tempdir().expect("tempdir");
            let db_path = dir.path().join("variants.sqlite3");
            let bus = Arc::new(snapshot::SnapshotBus::new());
            let sample = build_sample_data();

            let mut storage = SqliteStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("sqlite init");

            let focus_env = EventEnvelope {
                id: 150,
                timestamp: sample.base_ts + Duration::seconds(3),
                source: EventSource::Hook,
                kind: EventKind::Signal(SignalKind::WindowChanged),
                payload: EventPayload::Focus(sample.focus_info.clone()),
                derived: false,
                polling: false,
                sensitive: true,
            };

            let lock_env = EventEnvelope {
                id: 151,
                timestamp: sample.base_ts + Duration::seconds(4),
                source: EventSource::Derived,
                kind: EventKind::Signal(SignalKind::LockEnd),
                payload: EventPayload::Lock {
                    reason: "unlock".to_string(),
                },
                derived: true,
                polling: false,
                sensitive: false,
            };

            let title_env = EventEnvelope {
                id: 152,
                timestamp: sample.base_ts + Duration::seconds(5),
                source: EventSource::Polling,
                kind: EventKind::Hint(HintKind::TitleChanged),
                payload: EventPayload::Title {
                    window_id: 123,
                    title: "Panel".to_string(),
                },
                derived: true,
                polling: true,
                sensitive: false,
            };

            storage
                .add_envelopes(vec![focus_env.clone(), lock_env.clone(), title_env.clone()])
                .expect("envelopes enqueue");

            assert!(wait_until(|| rx.borrow().backlog_count == 0));
            drop(storage);

            let conn = SqliteConnection::open(&db_path).expect("open sqlite");
            conn.execute(
                "INSERT INTO raw_envelopes (event_id, timestamp, source, kind, payload, derived, polling, sensitive) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    Option::<i64>::None,
                    (sample.base_ts + Duration::seconds(6)).to_rfc3339(),
                    "derived",
                    "hint:focus_changed",
                    Option::<String>::None,
                    0,
                    0,
                    0,
                ],
            )
            .expect("insert manual row");
            drop(conn);

            let mut reader = SqliteStorage::new(
                &db_path,
                Arc::new(snapshot::SnapshotBus::new()),
                ThreadRegistry::new(),
            )
            .expect("sqlite reader");
            let envelopes = reader
                .fetch_envelopes_between(sample.base_ts, sample.base_ts + Duration::seconds(10))
                .expect("fetch envelopes");
            assert_eq!(envelopes.len(), 4);

            let mut saw_focus = false;
            let mut saw_lock = false;
            let mut saw_title = false;
            let mut saw_empty = false;

            for env in envelopes {
                match env.kind {
                    EventKind::Signal(SignalKind::WindowChanged) => {
                        if let EventPayload::Focus(focus) = env.payload {
                            assert_eq!(focus.window_id, sample.focus_info.window_id);
                            assert!(env.sensitive);
                            saw_focus = true;
                        }
                    }
                    EventKind::Signal(SignalKind::LockEnd) => {
                        if let EventPayload::Lock { reason } = env.payload {
                            assert_eq!(reason, "\"unlock\"");
                            saw_lock = true;
                        }
                    }
                    EventKind::Hint(HintKind::TitleChanged) => {
                        if let EventPayload::Title { window_id, title } = env.payload {
                            assert_eq!(window_id, 123);
                            assert_eq!(title, "Panel");
                            saw_title = true;
                        }
                    }
                    EventKind::Hint(HintKind::FocusChanged) => {
                        assert_eq!(env.id, 0);
                        assert!(matches!(env.payload, EventPayload::None));
                        saw_empty = true;
                    }
                    other => panic!("unexpected kind: {:?}", other),
                }
            }

            assert!(saw_focus && saw_lock && saw_title && saw_empty);

            drop(reader);
        });
    }

    #[test]
    fn sqlite_fetch_records_unknown_state_defaults_to_inactive() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("unknown_state.sqlite3");
        let bus = Arc::new(snapshot::SnapshotBus::new());
        let sample = build_sample_data();

        {
            let _storage = SqliteStorage::new(&db_path, Arc::clone(&bus), ThreadRegistry::new())
                .expect("sqlite init");
        }

        let conn = SqliteConnection::open(&db_path).expect("open sqlite");
        conn.execute(
            "INSERT INTO activity_records (id, start_time, end_time, state, focus_info) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![
                501_i64,
                sample.base_ts.to_rfc3339(),
                (sample.base_ts + Duration::seconds(5)).to_rfc3339(),
                "Unknown",
                serde_json::to_string(&sample.focus_info).expect("focus json"),
            ],
        )
        .expect("insert manual record");

        let mut reader = SqliteStorage::new(
            &db_path,
            Arc::new(snapshot::SnapshotBus::new()),
            ThreadRegistry::new(),
        )
        .expect("sqlite reader");
        let records = reader
            .fetch_records_since(sample.base_ts - Duration::seconds(1))
            .expect("fetch records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, ActivityState::Inactive);
        assert_eq!(records[0].record_id, 501);
        drop(reader);
    }
}
