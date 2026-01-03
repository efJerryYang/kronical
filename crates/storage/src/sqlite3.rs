use crate::{StorageBackend, StorageCommand, dec_backlog, inc_backlog, set_last_flush};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossbeam_channel::{self, Receiver, Sender};
use kronical_common::threading::{ThreadHandle, ThreadRegistry};
use rusqlite::{Connection, params};
use serde_json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kronical_core::compression::CompactEvent;
use kronical_core::events::RawEvent;
use kronical_core::events::model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use kronical_core::records::{ActivityRecord, ActivityState};
use kronical_core::snapshot;

fn parse_state_label(label: &str) -> ActivityState {
    match label {
        "Active" => ActivityState::Active,
        "Passive" => ActivityState::Passive,
        "Inactive" => ActivityState::Inactive,
        "Locked" => ActivityState::Locked,
        other => {
            eprintln!("Unknown state in DB: {}", other);
            ActivityState::Inactive
        }
    }
}

// Uses shared StorageCommand from crate::storage

pub struct SqliteStorage {
    db_path: PathBuf,
    sender: Sender<StorageCommand>,
    writer_thread: Option<ThreadHandle>,
}

impl SqliteStorage {
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

        let (sender, receiver) = crossbeam_channel::unbounded();

        let db_path_clone = db_path.clone();
        let bus_for_writer = Arc::clone(&snapshot_bus);
        let writer_thread = threads.spawn("sqlite-writer", move || {
            Self::background_writer(db_path_clone, receiver, bus_for_writer);
        })?;

        Ok(Self {
            db_path,
            sender,
            writer_thread: Some(writer_thread),
        })
    }

    fn init_db(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -4000)?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS raw_events (
                id INTEGER PRIMARY KEY,
                timestamp TEXT NOT NULL,
                event_type TEXT NOT NULL,
                data TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_raw_events_timestamp ON raw_events(timestamp);
            CREATE TABLE IF NOT EXISTS raw_envelopes (
                id INTEGER PRIMARY KEY,
                event_id INTEGER,
                timestamp TEXT NOT NULL,
                source TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT,
                derived INTEGER NOT NULL,
                polling INTEGER NOT NULL,
                sensitive INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_raw_envelopes_timestamp ON raw_envelopes(timestamp);
            CREATE TABLE IF NOT EXISTS activity_records (
                id INTEGER PRIMARY KEY,
                start_time TEXT NOT NULL,
                end_time TEXT,
                state TEXT NOT NULL,
                focus_info TEXT,
                run_id TEXT
            );
            CREATE TABLE IF NOT EXISTS compact_events (
                start_time TEXT NOT NULL,
                end_time TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT,
                raw_event_ref TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_compact_events_start_time ON compact_events(start_time);
            CREATE INDEX IF NOT EXISTS idx_activity_records_start_time ON activity_records(start_time);
            CREATE TABLE IF NOT EXISTS recent_transitions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at TEXT NOT NULL,
                from_state TEXT NOT NULL,
                to_state TEXT NOT NULL,
                by_signal TEXT,
                run_id TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_recent_transitions_at ON recent_transitions(at);"
        )?;
        let _ = conn.execute("ALTER TABLE recent_transitions ADD COLUMN run_id TEXT", []);
        let _ = conn.execute("ALTER TABLE activity_records ADD COLUMN run_id TEXT", []);
        Ok(())
    }

    fn background_writer(
        db_path: PathBuf,
        receiver: Receiver<StorageCommand>,
        snapshot_bus: Arc<snapshot::SnapshotBus>,
    ) {
        let mut conn = Connection::open(&db_path).expect("Failed to open DB in writer thread");
        let mut tx = conn.transaction().expect("Failed to start transaction");
        let mut count = 0;

        for command in receiver.iter() {
            let result = match command {
                StorageCommand::RawEvent(event) => {
                    let data = serde_json::to_string(&event).unwrap();
                    tx.execute(
                        "INSERT INTO raw_events (timestamp, event_type, data) VALUES (?, ?, ?)",
                        params![
                            event.timestamp().to_rfc3339(),
                            event.event_type().to_string(),
                            data
                        ],
                    )
                }
                StorageCommand::Record(record) => {
                    let focus_info_json = record
                        .focus_info
                        .as_ref()
                        .map(|fi| serde_json::to_string(fi).unwrap());
                    tx.execute(
                        "INSERT INTO activity_records (start_time, end_time, state, focus_info, run_id) VALUES (?, ?, ?, ?, ?)",
                        params![
                            record.start_time.to_rfc3339(),
                            record.end_time.map(|t| t.to_rfc3339()),
                            format!("{:?}", record.state),
                            focus_info_json,
                            record.run_id,
                        ],
                    )
                }
                StorageCommand::Envelope(env) => {
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
                    tx.execute(
                        "INSERT INTO raw_envelopes (event_id, timestamp, source, kind, payload, derived, polling, sensitive) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                        params![
                            env.id as i64,
                            env.timestamp.to_rfc3339(),
                            source,
                            kind,
                            payload_json,
                            if env.derived { 1 } else { 0 },
                            if env.polling { 1 } else { 0 },
                            if env.sensitive { 1 } else { 0 },
                        ],
                    )
                }
                StorageCommand::CompactEvents(events) => {
                    let mut last_res: rusqlite::Result<usize> = Ok(0);
                    for ce in events.into_iter() {
                        let (start_time, end_time, kind_s, payload_js, raw_ref_js) = match &ce {
                            CompactEvent::Scroll(s) => (
                                s.start_time.to_rfc3339(),
                                s.end_time.to_rfc3339(),
                                "compact:scroll".to_string(),
                                serde_json::to_string(&s).ok(),
                                s.raw_event_ids
                                    .as_ref()
                                    .and_then(|link| serde_json::to_string(link).ok()),
                            ),
                            CompactEvent::MouseTrajectory(t) => (
                                t.start_time.to_rfc3339(),
                                t.end_time.to_rfc3339(),
                                "compact:mouse_traj".to_string(),
                                serde_json::to_string(&t).ok(),
                                t.raw_event_ids
                                    .as_ref()
                                    .and_then(|link| serde_json::to_string(link).ok()),
                            ),
                            CompactEvent::Keyboard(k) => (
                                k.start_time.to_rfc3339(),
                                k.end_time.to_rfc3339(),
                                "compact:keyboard".to_string(),
                                serde_json::to_string(&k).ok(),
                                k.raw_event_ids
                                    .as_ref()
                                    .and_then(|link| serde_json::to_string(link).ok()),
                            ),
                            CompactEvent::Focus(f) => (
                                f.timestamp.to_rfc3339(),
                                f.timestamp.to_rfc3339(),
                                "compact:focus".to_string(),
                                serde_json::to_string(&f).ok(),
                                f.raw_event_ids
                                    .as_ref()
                                    .and_then(|link| serde_json::to_string(link).ok()),
                            ),
                        };
                        last_res = tx.execute(
                            "INSERT INTO compact_events (start_time, end_time, kind, payload, raw_event_ref) VALUES (?, ?, ?, ?, ?)",
                            params![start_time, end_time, kind_s, payload_js, raw_ref_js],
                        );
                        if last_res.is_err() {
                            break;
                        }
                    }
                    last_res
                }
                StorageCommand::Transition(transition) => {
                    tx.execute("INSERT INTO recent_transitions (at, from_state, to_state, by_signal, run_id) VALUES (?, ?, ?, ?, ?)",
                        params![
                            transition.at.to_rfc3339(),
                            format!("{:?}", transition.from),
                            format!("{:?}", transition.to),
                            transition.by_signal.clone(),
                            transition.run_id.clone(),
                        ],
                    )
                }
                StorageCommand::Shutdown => break,
            };

            if let Err(e) = result {
                eprintln!("Failed to write to DB: {}", e);
                snapshot_bus.push_health(format!("db write error: {}", e));
            }

            count += 1;
            if count % 100 == 0 {
                if let Err(e) = tx.commit() {
                    eprintln!("Failed to commit transaction: {}", e);
                    snapshot_bus.push_health(format!("db commit error: {}", e));
                }
                set_last_flush(Utc::now());
                tx = conn.transaction().expect("Failed to start new transaction");
            }
            dec_backlog();
        }
        if let Err(e) = tx.commit() {
            eprintln!("Final commit error: {}", e);
            snapshot_bus.push_health(format!("db final commit error: {}", e));
        }
        set_last_flush(Utc::now());
    }
}

impl StorageBackend for SqliteStorage {
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

    fn add_transitions(&mut self, transitions: Vec<snapshot::Transition>) -> Result<()> {
        for transition in transitions {
            self.sender
                .send(StorageCommand::Transition(transition.clone()))
                .context("Failed to send transition to writer")?;
            inc_backlog();
        }
        Ok(())
    }

    fn fetch_records_since(
        &mut self,
        since: DateTime<Utc>,
        run_id: Option<&str>,
    ) -> Result<Vec<ActivityRecord>> {
        let conn = Connection::open(&self.db_path)?;
        // Include records that have not ended or ended after 'since'.
        let mut stmt = match run_id {
            Some(_) => conn.prepare(
                "SELECT id, start_time, end_time, state, focus_info, run_id
                 FROM activity_records
                 WHERE (end_time IS NULL OR end_time >= ?1)
                   AND run_id = ?2
                 ORDER BY start_time ASC",
            )?,
            None => conn.prepare(
                "SELECT id, start_time, end_time, state, focus_info, run_id
                 FROM activity_records
                 WHERE (end_time IS NULL OR end_time >= ?1)
                 ORDER BY start_time ASC",
            )?,
        };

        let mut rows = match run_id {
            Some(id) => stmt.query([since.to_rfc3339(), id.to_string()])?,
            None => stmt.query([since.to_rfc3339()])?,
        };
        let mut out: Vec<ActivityRecord> = Vec::new();

        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let start_s: String = row.get(1)?;
            let end_s: Option<String> = row.get(2)?;
            let state_s: String = row.get(3)?;
            let focus_json: Option<String> = row.get(4)?;
            let row_run_id: Option<String> = row.get(5)?;

            let start_time = DateTime::parse_from_rfc3339(&start_s)
                .map(|dt| dt.with_timezone(&Utc))
                .context("Invalid start_time in DB")?;
            let end_time = match end_s {
                Some(s) => Some(
                    DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&Utc))
                        .context("Invalid end_time in DB")?,
                ),
                None => None,
            };

            let state = parse_state_label(&state_s);

            let focus_info = match focus_json {
                Some(js) => serde_json::from_str(&js).ok(),
                None => None,
            };

            out.push(ActivityRecord {
                record_id: id as u64,
                run_id: row_run_id,
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
        let mut stmt = conn.prepare(
            "SELECT event_id, timestamp, source, kind, payload, derived, polling, sensitive
             FROM raw_envelopes
             WHERE timestamp >= ?1 AND timestamp <= ?2
             ORDER BY timestamp ASC",
        )?;
        let mut rows = stmt.query([since.to_rfc3339(), until.to_rfc3339()])?;
        let mut out: Vec<EventEnvelope> = Vec::new();
        while let Some(row) = rows.next()? {
            let event_id: Option<i64> = row.get(0)?;
            let ts_s: String = row.get(1)?;
            let source_s: String = row.get(2)?;
            let kind_s: String = row.get(3)?;
            let payload_s: Option<String> = row.get(4)?;
            let derived_i: i64 = row.get(5)?;
            let polling_i: i64 = row.get(6)?;
            let sensitive_i: i64 = row.get(7)?;

            let timestamp = DateTime::parse_from_rfc3339(&ts_s).map(|dt| dt.with_timezone(&Utc))?;
            let source = match source_s.as_str() {
                "hook" => EventSource::Hook,
                "derived" => EventSource::Derived,
                "polling" => EventSource::Polling,
                other => {
                    eprintln!("Unknown envelope source: {}", other);
                    EventSource::Hook
                }
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
                    // Focus, Lock or Title tuples
                    if kind_s.contains("lock") {
                        EventPayload::Lock { reason: js }
                    } else if kind_s.contains("title") {
                        // stored as tuple (window_id, title)
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
                derived: derived_i != 0,
                polling: polling_i != 0,
                sensitive: sensitive_i != 0,
            });
        }
        Ok(out)
    }

    fn fetch_recent_transitions(
        &mut self,
        run_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<snapshot::Transition>> {
        let conn = Connection::open(&self.db_path)?;
        let mut stmt = match run_id {
            Some(_) => conn.prepare(
                "SELECT at, from_state, to_state, by_signal, run_id FROM recent_transitions WHERE run_id = ? ORDER BY at DESC LIMIT ?",
            )?,
            None => conn.prepare(
                "SELECT at, from_state, to_state, by_signal, run_id FROM recent_transitions ORDER BY at DESC LIMIT ?",
            )?,
        };
        let mut rows = match run_id {
            Some(id) => stmt.query(params![id, limit as i64])?,
            None => stmt.query([limit as i64])?,
        };
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let at_s: String = row.get(0)?;
            let from_s: String = row.get(1)?;
            let to_s: String = row.get(2)?;
            let by_signal: Option<String> = row.get(3)?;
            let run_id: Option<String> = row.get(4)?;
            let at = DateTime::parse_from_rfc3339(&at_s).map(|dt| dt.with_timezone(&Utc))?;
            out.push(snapshot::Transition {
                from: parse_state_label(&from_s),
                to: parse_state_label(&to_s),
                at,
                by_signal,
                run_id,
            });
        }
        Ok(out)
    }
}

impl Drop for SqliteStorage {
    fn drop(&mut self) {
        let _ = self.sender.send(StorageCommand::Shutdown);
        if let Some(thread) = self.writer_thread.take() {
            if let Err(e) = thread.join() {
                eprintln!("Background writer thread panicked: {:?}", e);
            }
        }
    }
}
