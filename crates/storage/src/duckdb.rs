use crate::{StorageBackend, StorageCommand, dec_backlog, inc_backlog, set_last_flush};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use duckdb::{Connection, params};
use kronical_common::threading::{ThreadHandle, ThreadRegistry};
use serde_json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

use kronical_core::compression::CompactEvent;
use kronical_core::events::RawEvent;
use kronical_core::events::model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use kronical_core::records::{ActivityRecord, ActivityState};
use kronical_core::snapshot;

pub struct DuckDbStorage {
    db_path: PathBuf,
    sender: mpsc::Sender<StorageCommand>,
    writer_thread: Option<ThreadHandle>,
    memory_limit_mb: Option<u64>,
}

impl DuckDbStorage {
    pub fn new<P: AsRef<Path>>(
        db_path: P,
        snapshot_bus: Arc<snapshot::SnapshotBus>,
        threads: ThreadRegistry,
    ) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {:?}", parent))?;
        }

        let conn = Connection::open(&db_path)?;
        Self::init_db(&conn)?;

        let (sender, receiver) = mpsc::channel();

        let db_path_clone = db_path.clone();
        let bus_for_writer = Arc::clone(&snapshot_bus);
        let writer_thread = threads.spawn("duckdb-writer", move || {
            Self::background_writer(db_path_clone, receiver, bus_for_writer);
        })?;

        Ok(Self {
            db_path,
            sender,
            writer_thread: Some(writer_thread),
            memory_limit_mb: None,
        })
    }

    pub fn new_with_limit<P: AsRef<Path>>(
        db_path: P,
        limit_mb: u64,
        snapshot_bus: Arc<snapshot::SnapshotBus>,
        threads: ThreadRegistry,
    ) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {:?}", parent))?;
        }

        let conn = Connection::open(&db_path)?;
        // Apply memory limit for this connection.
        let _ = conn.execute_batch(&format!(
            "PRAGMA memory_limit='{}MB'; PRAGMA threads=2;",
            limit_mb
        ));
        Self::init_db(&conn)?;

        let (sender, receiver) = mpsc::channel();

        let db_path_clone = db_path.clone();
        let bus_for_writer = Arc::clone(&snapshot_bus);
        let writer_thread = threads.spawn("duckdb-writer", move || {
            let conn =
                Connection::open(&db_path_clone).expect("Failed to open DB in writer thread");
            // Apply memory limit for the writer connection as well.
            let _ = conn.execute_batch(&format!(
                "PRAGMA memory_limit='{}MB'; PRAGMA threads=2;",
                limit_mb
            ));
            // Hand off to a helper that assumes an initialized connection
            Self::background_writer_with_conn(conn, receiver, bus_for_writer);
        })?;

        Ok(Self {
            db_path,
            sender,
            writer_thread: Some(writer_thread),
            memory_limit_mb: Some(limit_mb),
        })
    }

    fn init_db(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            r"CREATE TABLE IF NOT EXISTS raw_events (
                id BIGINT,
                timestamp TIMESTAMPTZ NOT NULL,
                event_type TEXT NOT NULL,
                data TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_raw_events_timestamp ON raw_events(timestamp);
            CREATE TABLE IF NOT EXISTS raw_envelopes (
                id BIGINT,
                event_id BIGINT,
                timestamp TIMESTAMPTZ NOT NULL,
                source TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT,
                derived BOOLEAN NOT NULL,
                polling BOOLEAN NOT NULL,
                sensitive BOOLEAN NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_raw_envelopes_timestamp ON raw_envelopes(timestamp);
            CREATE TABLE IF NOT EXISTS activity_records (
                id BIGINT,
                start_time TIMESTAMPTZ NOT NULL,
                end_time TIMESTAMPTZ,
                state TEXT NOT NULL,
                focus_info TEXT
            );
            CREATE TABLE IF NOT EXISTS compact_events (
                start_time TIMESTAMPTZ NOT NULL,
                end_time TIMESTAMPTZ NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT,
                raw_event_ids TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_compact_events_start_time ON compact_events(start_time);
            CREATE INDEX IF NOT EXISTS idx_activity_records_start_time ON activity_records(start_time);
            ",
        )?;
        Ok(())
    }

    fn background_writer(
        db_path: PathBuf,
        receiver: mpsc::Receiver<StorageCommand>,
        snapshot_bus: Arc<snapshot::SnapshotBus>,
    ) {
        let conn = Connection::open(&db_path).expect("Failed to open DB in writer thread");
        Self::background_writer_with_conn(conn, receiver, snapshot_bus)
    }

    fn background_writer_with_conn(
        conn: Connection,
        receiver: mpsc::Receiver<StorageCommand>,
        snapshot_bus: Arc<snapshot::SnapshotBus>,
    ) {
        let mut count: usize = 0;
        conn.execute_batch("BEGIN TRANSACTION;")
            .expect("Failed to start transaction");

        loop {
            let result = match receiver.recv() {
                Ok(StorageCommand::RawEvent(event)) => {
                    let data = serde_json::to_string(&event).unwrap();
                    let mut stmt = conn
                        .prepare(
                            "INSERT INTO raw_events (timestamp, event_type, data) VALUES (?, ?, ?)",
                        )
                        .expect("prepare failed");
                    stmt.execute(params![
                        event.timestamp().clone(),
                        event.event_type().to_string(),
                        data
                    ])
                }
                Ok(StorageCommand::Record(record)) => {
                    let focus_info_json = record
                        .focus_info
                        .as_ref()
                        .map(|fi| serde_json::to_string(fi).unwrap());
                    let mut stmt = conn
                        .prepare("INSERT INTO activity_records (start_time, end_time, state, focus_info) VALUES (?, ?, ?, ?)")
                        .expect("prepare failed");
                    stmt.execute(params![
                        record.start_time,
                        record.end_time,
                        format!("{:?}", record.state),
                        focus_info_json
                    ])
                }
                Ok(StorageCommand::Envelope(env)) => {
                    let source = match env.source {
                        EventSource::Hook => "hook",
                        EventSource::Derived => "derived",
                        EventSource::Polling => "polling",
                    };
                    let kind = match &env.kind {
                        EventKind::Signal(sk) => match sk {
                            SignalKind::KeyboardInput => "signal:keyboard",
                            SignalKind::MouseInput => "signal:mouse",
                            SignalKind::AppChanged => "signal:app_changed",
                            SignalKind::WindowChanged => "signal:window_changed",
                            SignalKind::ActivityPulse => "signal:activity_pulse",
                            SignalKind::LockStart => "signal:lock_start",
                            SignalKind::LockEnd => "signal:lock_end",
                        },
                        EventKind::Hint(hk) => match hk {
                            HintKind::FocusChanged => "hint:focus_changed",
                            HintKind::StateChanged => "hint:state_changed",
                            HintKind::TitleChanged => "hint:title_changed",
                            HintKind::PositionChanged => "hint:position",
                            HintKind::SizeChanged => "hint:size",
                        },
                    };
                    let payload_json = match &env.payload {
                        EventPayload::Keyboard(k) => serde_json::to_string(k).ok(),
                        EventPayload::Mouse(m) => serde_json::to_string(m).ok(),
                        EventPayload::Focus(f) => serde_json::to_string(f).ok(),
                        EventPayload::Lock { reason } => serde_json::to_string(reason).ok(),
                        EventPayload::Title { window_id, title } => {
                            serde_json::to_string(&(window_id, title)).ok()
                        }
                        EventPayload::State { from, to } => {
                            let from_s = format!("{:?}", from);
                            let to_s = format!("{:?}", to);
                            serde_json::to_string(&(from_s, to_s)).ok()
                        }
                        EventPayload::None => None,
                    };
                    let mut stmt = conn
                        .prepare("INSERT INTO raw_envelopes (event_id, timestamp, source, kind, payload, derived, polling, sensitive) VALUES (?, ?, ?, ?, ?, ?, ?, ?)")
                        .expect("prepare failed");
                    stmt.execute(params![
                        env.id as i64,
                        env.timestamp,
                        source,
                        kind,
                        payload_json,
                        env.derived,
                        env.polling,
                        env.sensitive,
                    ])
                }
                Ok(StorageCommand::CompactEvents(events)) => {
                    for ce in events.into_iter() {
                        match ce {
                            CompactEvent::Scroll(s) => {
                                let _ = conn.execute(
                                    "INSERT INTO compact_events (start_time, end_time, kind, payload, raw_event_ids) VALUES (?, ?, ?, ?, ?)",
                                    params![
                                        s.start_time,
                                        s.end_time,
                                        "compact:scroll",
                                        serde_json::to_string(&s).ok(),
                                        serde_json::to_string(&s.raw_event_ids).unwrap_or("[]".to_string()),
                                    ],
                                );
                            }
                            CompactEvent::MouseTrajectory(t) => {
                                let _ = conn.execute(
                                    "INSERT INTO compact_events (start_time, end_time, kind, payload, raw_event_ids) VALUES (?, ?, ?, ?, ?)",
                                    params![
                                        t.start_time,
                                        t.end_time,
                                        "compact:mouse_traj",
                                        serde_json::to_string(&t).ok(),
                                        serde_json::to_string(&t.raw_event_ids).unwrap_or("[]".to_string()),
                                    ],
                                );
                            }
                            CompactEvent::Keyboard(k) => {
                                let _ = conn.execute(
                                    "INSERT INTO compact_events (start_time, end_time, kind, payload, raw_event_ids) VALUES (?, ?, ?, ?, ?)",
                                    params![
                                        k.start_time,
                                        k.end_time,
                                        "compact:keyboard",
                                        serde_json::to_string(&k).ok(),
                                        serde_json::to_string(&k.raw_event_ids).unwrap_or("[]".to_string()),
                                    ],
                                );
                            }
                            CompactEvent::Focus(f) => {
                                let _ = conn.execute(
                                    "INSERT INTO compact_events (start_time, end_time, kind, payload, raw_event_ids) VALUES (?, ?, ?, ?, ?)",
                                    params![
                                        f.timestamp,
                                        f.timestamp,
                                        "compact:focus",
                                        serde_json::to_string(&f).ok(),
                                        serde_json::to_string(&vec![f.event_id]).unwrap_or("[]".to_string()),
                                    ],
                                );
                            }
                        }
                    }
                    Ok(1)
                }
                Ok(StorageCommand::Shutdown) => break,
                Err(_) => break,
            };

            if let Err(e) = result {
                eprintln!("Failed to write to DuckDB: {}", e);
                // If a statement fails inside a transaction, DuckDB aborts the transaction.
                // Roll back and start a new transaction to clear the aborted state.
                let _ = conn.execute_batch("ROLLBACK; BEGIN TRANSACTION;");
                snapshot_bus.push_health(format!("duckdb write error: {}", e));
            }

            count += 1;
            if count % 100 == 0 {
                if let Err(e) = conn.execute_batch("COMMIT; BEGIN TRANSACTION;") {
                    eprintln!("Failed to commit DuckDB transaction: {}", e);
                    snapshot_bus.push_health(format!("duckdb commit error: {}", e));
                }
                set_last_flush(Utc::now());
            }
            dec_backlog();
        }

        if let Err(e) = conn.execute_batch("COMMIT;") {
            eprintln!("Final DuckDB commit error: {}", e);
            snapshot_bus.push_health(format!("duckdb final commit error: {}", e));
        }
        set_last_flush(Utc::now());
    }
}

impl StorageBackend for DuckDbStorage {
    fn add_events(&mut self, events: Vec<RawEvent>) -> Result<()> {
        for event in events {
            self.sender
                .send(StorageCommand::RawEvent(event))
                .context("Failed to send raw event to writer")?;
            inc_backlog();
        }
        Ok(())
    }

    fn add_records(&mut self, new_records: Vec<ActivityRecord>) -> Result<()> {
        for record in new_records {
            self.sender
                .send(StorageCommand::Record(record.clone()))
                .context("Failed to send record to writer")?;
            inc_backlog();
        }
        Ok(())
    }

    fn add_compact_events(&mut self, events: Vec<CompactEvent>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.sender
            .send(StorageCommand::CompactEvents(events))
            .context("Failed to send compact events to writer")?;
        inc_backlog();
        Ok(())
    }

    fn add_envelopes(&mut self, events: Vec<EventEnvelope>) -> Result<()> {
        for env in events {
            self.sender
                .send(StorageCommand::Envelope(env))
                .context("Failed to send envelope to writer")?;
            inc_backlog();
        }
        Ok(())
    }

    fn fetch_records_since(&mut self, since: DateTime<Utc>) -> Result<Vec<ActivityRecord>> {
        let conn = Connection::open(&self.db_path)?;
        if let Some(mb) = self.memory_limit_mb {
            let _ = conn.execute_batch(&format!(
                "PRAGMA memory_limit='{}MB'; PRAGMA threads=2;",
                mb
            ));
        }
        let mut stmt = conn.prepare(
            "SELECT id, start_time, end_time, state, focus_info
             FROM activity_records
             WHERE (end_time IS NULL OR end_time >= ?)
             ORDER BY start_time ASC",
        )?;

        let mut rows = stmt.query([since])?;
        let mut out: Vec<ActivityRecord> = Vec::new();

        while let Some(row) = rows.next()? {
            let id: Option<i64> = row.get(0)?;
            let start_time: DateTime<Utc> = row.get(1)?;
            let end_time: Option<DateTime<Utc>> = row.get(2)?;
            let state_s: String = row.get(3)?;
            let focus_json: Option<String> = row.get(4)?;

            let state = match state_s.as_str() {
                "Active" => ActivityState::Active,
                "Passive" => ActivityState::Passive,
                "Inactive" => ActivityState::Inactive,
                "Locked" => ActivityState::Locked,
                _ => ActivityState::Inactive,
            };

            let focus_info = match focus_json {
                Some(js) => serde_json::from_str(&js).ok(),
                None => None,
            };

            out.push(ActivityRecord {
                record_id: id.unwrap_or_default() as u64,
                start_time,
                end_time,
                state,
                focus_info,
                event_count: 0,
                triggering_events: Vec::new(),
            });
        }

        Ok(out)
    }

    fn fetch_envelopes_between(
        &mut self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<EventEnvelope>> {
        let conn = Connection::open(&self.db_path)?;
        if let Some(mb) = self.memory_limit_mb {
            let _ = conn.execute_batch(&format!(
                "PRAGMA memory_limit='{}MB'; PRAGMA threads=2;",
                mb
            ));
        }
        let mut stmt = conn.prepare(
            "SELECT event_id, timestamp, source, kind, payload, derived, polling, sensitive
             FROM raw_envelopes
             WHERE timestamp >= ? AND timestamp <= ?
             ORDER BY timestamp ASC",
        )?;
        let mut rows = stmt.query(params![since, until])?;
        let mut out: Vec<EventEnvelope> = Vec::new();
        while let Some(row) = rows.next()? {
            let event_id: Option<i64> = row.get(0)?;
            let timestamp: DateTime<Utc> = row.get(1)?;
            let source_s: String = row.get(2)?;
            let kind_s: String = row.get(3)?;
            let payload_s: Option<String> = row.get(4)?;
            let derived_b: bool = row.get(5)?;
            let polling_b: bool = row.get(6)?;
            let sensitive_b: bool = row.get(7)?;

            let source = match source_s.as_str() {
                "hook" => EventSource::Hook,
                "derived" => EventSource::Derived,
                "polling" => EventSource::Polling,
                _ => EventSource::Hook,
            };
            let kind = if kind_s.starts_with("signal:") {
                let k = &kind_s[7..];
                let sk = match k {
                    "keyboard" => SignalKind::KeyboardInput,
                    "mouse" => SignalKind::MouseInput,
                    "app_changed" => SignalKind::AppChanged,
                    "window_changed" => SignalKind::WindowChanged,
                    "activity_pulse" => SignalKind::ActivityPulse,
                    "lock_start" => SignalKind::LockStart,
                    "lock_end" => SignalKind::LockEnd,
                    _ => SignalKind::AppChanged,
                };
                EventKind::Signal(sk)
            } else if kind_s.starts_with("hint:") {
                let k = &kind_s[5..];
                let hk = match k {
                    "focus_changed" => HintKind::FocusChanged,
                    "state_changed" => HintKind::StateChanged,
                    "title_changed" => HintKind::TitleChanged,
                    "position" => HintKind::PositionChanged,
                    "size" => HintKind::SizeChanged,
                    _ => HintKind::TitleChanged,
                };
                EventKind::Hint(hk)
            } else {
                EventKind::Signal(SignalKind::AppChanged)
            };
            let payload = match (kind.clone(), payload_s) {
                (EventKind::Signal(SignalKind::KeyboardInput), Some(js)) => {
                    serde_json::from_str(&js)
                        .map(EventPayload::Keyboard)
                        .unwrap_or(EventPayload::None)
                }
                (EventKind::Signal(SignalKind::MouseInput), Some(js)) => serde_json::from_str(&js)
                    .map(EventPayload::Mouse)
                    .unwrap_or(EventPayload::None),
                (EventKind::Signal(_), Some(js)) | (EventKind::Hint(_), Some(js)) => {
                    if kind_s.contains("lock") {
                        EventPayload::Lock { reason: js }
                    } else if kind_s.contains("title") {
                        match serde_json::from_str::<(u32, String)>(&js) {
                            Ok((wid, title)) => EventPayload::Title {
                                window_id: wid,
                                title,
                            },
                            Err(_) => EventPayload::None,
                        }
                    } else if kind_s.contains("state_changed") {
                        match serde_json::from_str::<(String, String)>(&js) {
                            Ok((from_s, to_s)) => {
                                let parse = |s: &str| match s {
                                    "Active" => ActivityState::Active,
                                    "Passive" => ActivityState::Passive,
                                    "Inactive" => ActivityState::Inactive,
                                    "Locked" => ActivityState::Locked,
                                    _ => ActivityState::Inactive,
                                };
                                EventPayload::State {
                                    from: parse(&from_s),
                                    to: parse(&to_s),
                                }
                            }
                            Err(_) => EventPayload::None,
                        }
                    } else {
                        serde_json::from_str(&js)
                            .map(EventPayload::Focus)
                            .unwrap_or(EventPayload::None)
                    }
                }
                _ => EventPayload::None,
            };

            out.push(EventEnvelope {
                id: event_id.unwrap_or(0) as u64,
                timestamp,
                source,
                kind,
                payload,
                derived: derived_b,
                polling: polling_b,
                sensitive: sensitive_b,
            });
        }
        Ok(out)
    }
}

impl Drop for DuckDbStorage {
    fn drop(&mut self) {
        let _ = self.sender.send(StorageCommand::Shutdown);
        if let Some(thread) = self.writer_thread.take() {
            if let Err(e) = thread.join() {
                eprintln!("Background writer thread panicked: {:?}", e);
            }
        }
    }
}
