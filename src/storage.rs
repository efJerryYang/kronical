use crate::daemon::compression::CompactEvent;
use crate::daemon::events::RawEvent;
use crate::daemon::events::model::EventEnvelope;
use crate::daemon::records::ActivityRecord;
use anyhow::Result;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub trait StorageBackend: Send + Sync {
    fn add_events(&mut self, events: Vec<RawEvent>) -> Result<()>;
    fn add_compact_events(&mut self, events: Vec<CompactEvent>) -> Result<()>;
    fn add_envelopes(&mut self, events: Vec<EventEnvelope>) -> Result<()>;
    fn add_records(&mut self, records: Vec<ActivityRecord>) -> Result<()>;
    fn fetch_records_since(&mut self, since: DateTime<Utc>) -> Result<Vec<ActivityRecord>>;
    fn fetch_envelopes_between(
        &mut self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<EventEnvelope>>;
}

// Unified storage metrics (shared by all backends)
static STORAGE_BACKLOG: AtomicU64 = AtomicU64::new(0);
static LAST_FLUSH_AT: Lazy<RwLock<Option<DateTime<Utc>>>> = Lazy::new(|| RwLock::new(None));

pub fn inc_backlog() {
    let _ = STORAGE_BACKLOG.fetch_add(1, Ordering::Relaxed);
}
pub fn dec_backlog() {
    let _ = STORAGE_BACKLOG.fetch_sub(1, Ordering::Relaxed);
}
pub fn set_last_flush(t: DateTime<Utc>) {
    if let Ok(mut g) = LAST_FLUSH_AT.write() {
        *g = Some(t);
    }
}

#[derive(Clone, Debug)]
pub struct StorageMetrics {
    pub backlog_count: u64,
    pub last_flush_at: Option<DateTime<Utc>>,
}

pub fn storage_metrics() -> StorageMetrics {
    let last = LAST_FLUSH_AT.read().ok().and_then(|g| (*g).clone());
    StorageMetrics {
        backlog_count: STORAGE_BACKLOG.load(Ordering::Relaxed),
        last_flush_at: last,
    }
}

// Commands for background writer threads (shared)
#[derive(Clone, Debug)]
pub enum StorageCommand {
    RawEvent(RawEvent),
    Record(ActivityRecord),
    Envelope(EventEnvelope),
    Shutdown,
}

pub mod duckdb;
pub mod sqlite3;
