use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use tokio::sync::watch;

use kronical_core::compression::CompactEvent;
use kronical_core::events::RawEvent;
use kronical_core::events::model::EventEnvelope;
use kronical_core::records::ActivityRecord;

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
    Shutdown,
}

pub mod duckdb;
pub mod sqlite3;
pub mod system_metrics;
