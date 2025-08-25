use crate::{events::RawEvent, records::ActivityRecord, storage_backend::StorageBackend};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use size_of::SizeOf;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, SizeOf)]
pub struct EventsAndRecordsExport {
    pub events: Vec<RawEvent>,
    pub records: Vec<ActivityRecord>,
    pub last_updated: DateTime<Utc>,
}

pub struct JsonStorage {
    data_file: PathBuf,
    events: Vec<RawEvent>,
    records: Vec<ActivityRecord>,
    record_index: BTreeMap<DateTime<Utc>, usize>,
    event_index: BTreeMap<DateTime<Utc>, usize>,
}

impl StorageBackend for JsonStorage {
    fn add_events(&mut self, events: Vec<RawEvent>) -> Result<()> {
        JsonStorage::add_events(self, events)
    }

    fn add_records(&mut self, records: Vec<ActivityRecord>) -> Result<()> {
        JsonStorage::add_records(self, records)
    }

    fn add_compact_events(&mut self, _events: Vec<crate::compression::CompactEvent>) -> Result<()> {
        Ok(())
    }
}

impl JsonStorage {
    pub fn new<P: AsRef<Path>>(data_file: P) -> Result<Self> {
        let data_file = data_file.as_ref().to_path_buf();
        info!("Initializing DataStore with file: {:?}", data_file);

        if let Some(parent) = data_file.parent() {
            debug!("Creating directory: {:?}", parent);
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create data directory: {:?}", parent))?;
        }

        let mut store = Self {
            data_file,
            events: Vec::new(),
            records: Vec::new(),
            record_index: BTreeMap::new(),
            event_index: BTreeMap::new(),
        };

        store
            .load_data()
            .with_context(|| "Failed to load existing data during DataStore initialization")?;

        info!("DataStore initialized successfully");
        Ok(store)
    }

    pub fn add_events(&mut self, events: Vec<RawEvent>) -> Result<()> {
        for event in events {
            debug!(
                "Adding event #{}: {} at {}",
                event.event_id(),
                event.event_type(),
                event.timestamp()
            );
            let index = self.events.len();
            self.event_index.insert(event.timestamp(), index);
            self.events.push(event);
        }
        let result = self.save_data();
        if result.is_ok() {
            debug!(
                "Events saved successfully, total events: {}",
                self.events.len()
            );
        } else {
            error!("Failed to save events: {:?}", result);
        }
        result
    }

    pub fn add_records(&mut self, records: Vec<ActivityRecord>) -> Result<()> {
        for record in records {
            info!(
                "Adding record #{}: {:?} at {}",
                record.record_id,
                record.state,
                record.start_time
            );
            let index = self.records.len();
            self.record_index.insert(record.start_time, index);
            self.records.push(record);
        }
        let result = self.save_data();
        if result.is_ok() {
            info!(
                "Records saved successfully, total records: {}",
                self.records.len()
            );
        } else {
            error!("Failed to save records: {:?}", result);
        }
        result
    }

    pub fn get_events_in_range(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<&RawEvent> {
        self.event_index
            .range(start..=end)
            .map(|(_, &index)| &self.events[index])
            .filter(|event| event.timestamp() >= start && event.timestamp() <= end)
            .collect()
    }

    pub fn get_records_in_range(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Vec<&ActivityRecord> {
        self.record_index
            .range(start..=end)
            .map(|(_, &index)| &self.records[index])
            .filter(|record| {
                record.start_time >= start
                    && (record.end_time.is_none() || record.end_time.unwrap() <= end)
            })
            .collect()
    }

    pub fn get_all_events(&self) -> &[RawEvent] {
        &self.events
    }

    pub fn get_all_records(&self) -> &[ActivityRecord] {
        &self.records
    }

    pub fn get_recent_records(&self, limit: usize) -> Vec<ActivityRecord> {
        let len = self.records.len();
        if len <= limit {
            self.records.clone()
        } else {
            self.records[len - limit..].to_vec()
        }
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn get_records_collection_memory_usage_mb(&self) -> f32 {
        let total_records_size = self.records.size_of().total_bytes();
        let index_size = self.record_index.size_of().total_bytes();

        (total_records_size + index_size) as f32 / 1024.0 / 1024.0
    }

    pub fn get_events_collection_memory_usage_mb(&self) -> f32 {
        let total_events_size = self.events.size_of().total_bytes();
        let index_size = self.event_index.size_of().total_bytes();

        (total_events_size + index_size) as f32 / 1024.0 / 1024.0
    }

    pub fn export_json<P: AsRef<Path>>(&self, output_path: P) -> Result<()> {
        let data = EventsAndRecordsExport {
            events: self.events.clone(),
            records: self.records.clone(),
            last_updated: Utc::now(),
        };

        let file = File::create(output_path.as_ref())
            .with_context(|| format!("Failed to create export file: {:?}", output_path.as_ref()))?;

        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &data).context("Failed to serialize activity data")?;

        Ok(())
    }

    fn load_data(&mut self) -> Result<()> {
        if !self.data_file.exists() {
            info!(
                "Data file {:?} does not exist, starting with empty data",
                self.data_file
            );
            return Ok(());
        }

        info!("Loading data from file: {:?}", self.data_file);

        let file = File::open(&self.data_file)
            .with_context(|| format!("Failed to open data file: {:?}", self.data_file))?;

        let reader = BufReader::new(file);
        let data: EventsAndRecordsExport = serde_json::from_reader(reader)
            .with_context(|| {
                error!("JSON parsing failed for file: {:?}", self.data_file);
                error!("Expected structure: EventsAndRecordsExport with 'events', 'records', and 'last_updated' fields");
                format!("Failed to parse JSON data file: {:?}. The file format may be incompatible with the current version.", self.data_file)
            })?;

        info!(
            "Successfully loaded {} events and {} records from data file",
            data.events.len(),
            data.records.len()
        );

        self.events = data.events;
        self.records = data.records;
        self.rebuild_indices();

        Ok(())
    }

    fn save_data(&self) -> Result<()> {
        if let Some(parent) = self.data_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create data directory: {:?}", parent))?;
        }

        let data = EventsAndRecordsExport {
            events: self.events.clone(),
            records: self.records.clone(),
            last_updated: Utc::now(),
        };

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.data_file)
            .with_context(|| format!("Failed to create data file: {:?}", self.data_file))?;

        let writer = BufWriter::new(file);
        serde_json::to_writer_pretty(writer, &data)
            .with_context(|| format!("Failed to write data file: {:?}", self.data_file))?;

        Ok(())
    }

    fn rebuild_indices(&mut self) {
        self.event_index.clear();
        for (i, event) in self.events.iter().enumerate() {
            self.event_index.insert(event.timestamp(), i);
        }

        self.record_index.clear();
        for (i, record) in self.records.iter().enumerate() {
            self.record_index.insert(record.start_time, i);
        }
    }
}
