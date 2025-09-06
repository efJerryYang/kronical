use crate::daemon::compressor::CompactEvent;
use crate::daemon::events::RawEvent;
use crate::daemon::events::model::EventEnvelope;
use crate::daemon::records::ActivityRecord;
use anyhow::Result;
use chrono::TimeZone;
use chrono::{DateTime, Utc};
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use tokio::sync::watch;

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
// Epoch seconds of last flush; 0 means None
static LAST_FLUSH_AT_EPOCH: AtomicI64 = AtomicI64::new(0);

// Watch channel to broadcast storage metrics without locks
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
        // Safe because secs > 0; use non-deprecated API
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
    // Initialize channel on first use and broadcast current snapshot
    let (tx, _rx) = init_metrics_channel();
    let _ = tx.send(storage_metrics_snapshot());
}

// Optional: provide a watch receiver for live updates
pub fn storage_metrics_watch() -> watch::Receiver<StorageMetrics> {
    let (_tx, rx) = init_metrics_channel();
    rx.clone()
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
