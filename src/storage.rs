use crate::daemon::compression::CompactEvent;
use crate::daemon::events::RawEvent;
use crate::daemon::events::model::EventEnvelope;
use crate::daemon::records::ActivityRecord;
use anyhow::Result;
use chrono::{DateTime, Utc};

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

pub mod sqlite3;
