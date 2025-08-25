use crate::compression::CompactEvent;
use crate::events::RawEvent;
use crate::records::ActivityRecord;
use anyhow::Result;

pub trait StorageBackend: Send + Sync {
    fn add_events(&mut self, events: Vec<RawEvent>) -> Result<()>;
    fn add_compact_events(&mut self, events: Vec<CompactEvent>) -> Result<()>;
    fn add_records(&mut self, records: Vec<ActivityRecord>) -> Result<()>;
}
