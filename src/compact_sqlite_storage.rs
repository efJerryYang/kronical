use crate::compression::{CompactEvent, CompactFocusEvent, CompactKeyboardEvent, CompactMouseTrajectory, CompactScrollSequence, StringInterner};
use crate::storage_backend::StorageBackend;
use crate::events::RawEvent;
use crate::records::{ActivityRecord, ActivityState};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use log::error;

pub enum StorageCommand {
    Event(CompactEvent),
    Record(ActivityRecord),
    Shutdown,
}

pub struct CompactSqliteStorage {
    db_path: PathBuf,
    writer_thread: Option<thread::JoinHandle<()>>,
    sender: mpsc::Sender<StorageCommand>,
    string_interner: Arc<Mutex<StringInterner>>,
}

impl CompactSqliteStorage {
    pub fn new<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
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
            writer_thread: Some(writer_thread),
            sender,
            string_interner: Arc::new(Mutex::new(StringInterner::new())),
        })
    }

    fn init_db(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS string_interner (
                id INTEGER PRIMARY KEY,
                value TEXT NOT NULL UNIQUE
             );
             CREATE TABLE IF NOT EXISTS focus_events (
                timestamp TEXT NOT NULL,
                app_name_id INTEGER NOT NULL,
                window_title_id INTEGER NOT NULL,
                pid INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS scroll_events (
                timestamp TEXT NOT NULL,
                data BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mouse_trajectory_events (
                timestamp TEXT NOT NULL,
                data BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS keyboard_events (
                timestamp TEXT NOT NULL,
                data BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS activity_records (
                start_time TEXT NOT NULL,
                end_time TEXT,
                state TEXT NOT NULL,
                focus_info BLOB
             );"
        )?;
        Ok(())
    }

    fn get_connection(&self) -> Result<Connection> {
        Connection::open(&self.db_path).context("Failed to open database connection")
    }

    fn background_writer(db_path: PathBuf, receiver: mpsc::Receiver<StorageCommand>) {
        let mut conn = Connection::open(db_path).expect("Failed to open DB in writer thread");
        let mut tx = conn.transaction().expect("Failed to start transaction");
        let mut count = 0;

        for command in receiver.iter() {
            let result = match command {
                StorageCommand::Event(event) => match event {
                    CompactEvent::Focus(e) => tx.execute("INSERT INTO focus_events (timestamp, app_name_id, window_title_id, pid) VALUES (?, ?, ?, ?)", params![e.timestamp.to_rfc3339(), e.app_name_id, e.window_title_id, e.pid]),
                    CompactEvent::Scroll(e) => tx.execute("INSERT INTO scroll_events (timestamp, data) VALUES (?, ?)", params![e.start_time.to_rfc3339(), bincode::serialize(&e).unwrap()]),
                    CompactEvent::MouseTrajectory(e) => tx.execute("INSERT INTO mouse_trajectory_events (timestamp, data) VALUES (?, ?)", params![e.start_time.to_rfc3339(), bincode::serialize(&e).unwrap()]),
                    CompactEvent::Keyboard(e) => tx.execute("INSERT INTO keyboard_events (timestamp, data) VALUES (?, ?)", params![e.timestamp.to_rfc3339(), bincode::serialize(&e).unwrap()]),
                },
                StorageCommand::Record(record) => {
                    let focus_info_blob = record.focus_info.as_ref().map(|fi| bincode::serialize(fi).unwrap());
                    tx.execute("INSERT INTO activity_records (start_time, end_time, state, focus_info) VALUES (?, ?, ?, ?)", params![record.start_time.to_rfc3339(), record.end_time.map(|t| t.to_rfc3339()), format!("{:?}", record.state), focus_info_blob])
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

impl StorageBackend for CompactSqliteStorage {
    fn add_events(&mut self, _events: Vec<RawEvent>) -> Result<()> {
        Ok(())
    }

    fn add_compact_events(&mut self, events: Vec<CompactEvent>) -> Result<()> {
        for event in events {
            self.sender.send(StorageCommand::Event(event)).context("Failed to send compact event to writer")?;
        }
        Ok(())
    }
    
    fn add_records(&mut self, records: Vec<ActivityRecord>) -> Result<()> {
        for record in records {
            self.sender.send(StorageCommand::Record(record)).context("Failed to send record to writer")?;
        }
        Ok(())
    }
}

impl Drop for CompactSqliteStorage {
    fn drop(&mut self) {
        let _ = self.sender.send(StorageCommand::Shutdown);
        if let Some(thread) = self.writer_thread.take() {
            thread.join().expect("Background writer thread panicked");
        }
    }
}
