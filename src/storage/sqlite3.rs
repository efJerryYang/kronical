use crate::daemon::events::RawEvent;
use crate::daemon::records::ActivityRecord;
use crate::storage::StorageBackend;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde_json;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;

pub enum StorageCommand {
    RawEvent(RawEvent),
    Record(ActivityRecord),
    Shutdown,
}

pub struct SqliteStorage {
    db_path: PathBuf,
    sender: mpsc::Sender<StorageCommand>,
    writer_thread: Option<thread::JoinHandle<()>>,
}

impl SqliteStorage {
    pub fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {:?}", parent))?;
        }

        let conn = Connection::open(&db_path)?;
        Self::init_db(&conn)?;

        let (sender, receiver) = mpsc::channel();

        let db_path_clone = db_path.clone();
        let writer_thread = thread::spawn(move || {
            Self::background_writer(db_path_clone, receiver);
        });

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
            CREATE TABLE IF NOT EXISTS activity_records (
                id INTEGER PRIMARY KEY,
                start_time TEXT NOT NULL,
                end_time TEXT,
                state TEXT NOT NULL,
                focus_info TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_activity_records_start_time ON activity_records(start_time);"
        )?;
        Ok(())
    }

    fn background_writer(db_path: PathBuf, receiver: mpsc::Receiver<StorageCommand>) {
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
                    tx.execute("INSERT INTO activity_records (start_time, end_time, state, focus_info) VALUES (?, ?, ?, ?)", params![record.start_time.to_rfc3339(), record.end_time.map(|t| t.to_rfc3339()), format!("{:?}", record.state), focus_info_json])
                }
                StorageCommand::Shutdown => break,
            };

            if let Err(e) = result {
                eprintln!("Failed to write to DB: {}", e);
            }

            count += 1;
            if count % 100 == 0 {
                if let Err(e) = tx.commit() {
                    eprintln!("Failed to commit transaction: {}", e);
                }
                tx = conn.transaction().expect("Failed to start new transaction");
            }
        }
        let _ = tx.commit();
    }
}

impl StorageBackend for SqliteStorage {
    fn add_events(&mut self, events: Vec<RawEvent>) -> Result<()> {
        for event in events {
            self.sender
                .send(StorageCommand::RawEvent(event))
                .context("Failed to send raw event to writer")?;
        }
        Ok(())
    }

    fn add_records(&mut self, new_records: Vec<ActivityRecord>) -> Result<()> {
        for record in new_records {
            self.sender
                .send(StorageCommand::Record(record.clone()))
                .context("Failed to send record to writer")?;
        }
        Ok(())
    }

    fn add_compact_events(
        &mut self,
        _events: Vec<crate::daemon::compression::CompactEvent>,
    ) -> Result<()> {
        Ok(())
    }

    fn fetch_records_since(&mut self, since: DateTime<Utc>) -> Result<Vec<ActivityRecord>> {
        let conn = Connection::open(&self.db_path)?;
        // Include records that have not ended or ended after 'since'.
        let mut stmt = conn.prepare(
            "SELECT id, start_time, end_time, state, focus_info
             FROM activity_records
             WHERE (end_time IS NULL OR end_time >= ?1)
             ORDER BY start_time ASC",
        )?;

        let mut rows = stmt.query([since.to_rfc3339()])?;
        let mut out: Vec<ActivityRecord> = Vec::new();

        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let start_s: String = row.get(1)?;
            let end_s: Option<String> = row.get(2)?;
            let state_s: String = row.get(3)?;
            let focus_json: Option<String> = row.get(4)?;

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

            let state = match state_s.as_str() {
                "Active" => crate::daemon::records::ActivityState::Active,
                "Passive" => crate::daemon::records::ActivityState::Passive,
                "Inactive" => crate::daemon::records::ActivityState::Inactive,
                other => {
                    eprintln!("Unknown state in DB: {}", other);
                    crate::daemon::records::ActivityState::Inactive
                }
            };

            let focus_info = match focus_json {
                Some(js) => serde_json::from_str(&js).ok(),
                None => None,
            };

            out.push(ActivityRecord {
                record_id: id as u64,
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
}

impl Drop for SqliteStorage {
    fn drop(&mut self) {
        let _ = self.sender.send(StorageCommand::Shutdown);
        if let Some(thread) = self.writer_thread.take() {
            thread.join().expect("Background writer thread panicked");
        }
    }
}
