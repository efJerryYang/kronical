use crate::daemon::coordinator::KronicalEvent;
use crate::daemon::records::ActivityRecord;
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::daemon::snapshot;
use crate::storage::{StorageBackend, StorageCommand, storage_metrics_watch};
use anyhow::{Context, Result};
use chrono::Utc;
use log::{error, info};
use std::any::Any;
use std::sync::{Arc, mpsc};
use std::thread;

mod compression;
mod envelope;
mod hints;
mod ingest;
mod snapshot_stage;
mod storage;
mod types;

use compression::{CompressionStageConfig, spawn_compression_stage};
use envelope::{EnvelopeStageConfig, spawn_envelope_stage};
use hints::spawn_hints_stage;
use ingest::spawn_ingest_stage;
use snapshot_stage::{SnapshotStageConfig, spawn_snapshot_stage};
use storage::spawn_storage_stage;

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub retention_minutes: u64,
    pub active_grace_secs: u64,
    pub idle_threshold_secs: u64,
    pub ephemeral_max_duration_secs: u64,
    pub ephemeral_min_distinct_ids: usize,
    pub max_windows_per_app: usize,
    pub ephemeral_app_max_duration_secs: u64,
    pub ephemeral_app_min_distinct_procs: usize,
    pub focus_interner_max_strings: usize,
}

pub struct PipelineResources {
    pub storage: Box<dyn StorageBackend>,
    pub event_rx: mpsc::Receiver<KronicalEvent>,
    pub poll_handle: Arc<std::sync::atomic::AtomicU64>,
    pub snapshot_bus: Arc<snapshot::SnapshotBus>,
}

pub struct PipelineHandles {
    handles: Vec<ThreadHandle>,
}

impl PipelineHandles {
    pub fn join(self) -> thread::Result<()> {
        let mut first_err: Option<Box<dyn Any + Send + 'static>> = None;
        for handle in self.handles.into_iter().rev() {
            if let Err(e) = handle.join() {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        if let Some(err) = first_err {
            Err(err)
        } else {
            Ok(())
        }
    }
}

pub fn spawn_pipeline(
    config: PipelineConfig,
    resources: PipelineResources,
    threads: ThreadRegistry,
) -> Result<PipelineHandles> {
    let PipelineConfig {
        retention_minutes,
        active_grace_secs,
        idle_threshold_secs,
        ephemeral_max_duration_secs,
        ephemeral_min_distinct_ids,
        max_windows_per_app,
        ephemeral_app_max_duration_secs,
        ephemeral_app_min_distinct_procs,
        focus_interner_max_strings,
    } = config;

    let PipelineResources {
        mut storage,
        event_rx,
        poll_handle,
        snapshot_bus,
    } = resources;

    let initial_records = hydrate_recent_records(storage.as_mut(), retention_minutes);

    let (derive_tx, derive_rx) = mpsc::channel();
    let (hints_tx, hints_rx) = mpsc::channel();
    let (compression_tx, compression_rx) = mpsc::channel();
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    let (storage_tx, storage_rx) = mpsc::channel::<StorageCommand>();

    let mut handles = Vec::new();

    let storage_handle = spawn_storage_stage(&threads, storage_rx, storage)
        .context("spawn pipeline storage stage")?;
    handles.push(storage_handle);

    let hints_handle =
        spawn_hints_stage(&threads, hints_rx, storage_tx.clone(), snapshot_tx.clone())
            .context("spawn pipeline hints stage")?;
    handles.push(hints_handle);

    let compression_handle = spawn_compression_stage(
        &threads,
        CompressionStageConfig {
            receiver: compression_rx,
            storage_tx: storage_tx.clone(),
            focus_interner_max_strings,
        },
    )
    .context("spawn pipeline compression stage")?;
    handles.push(compression_handle);

    let cfg_summary = snapshot::ConfigSummary {
        active_grace_secs,
        idle_threshold_secs,
        retention_minutes,
        ephemeral_max_duration_secs,
        ephemeral_min_distinct_ids,
        ephemeral_app_max_duration_secs,
        ephemeral_app_min_distinct_procs,
    };

    let snapshot_handle = spawn_snapshot_stage(
        &threads,
        SnapshotStageConfig {
            receiver: snapshot_rx,
            snapshot_bus: Arc::clone(&snapshot_bus),
            storage_tx: storage_tx.clone(),
            poll_handle: Arc::clone(&poll_handle),
            cfg_summary: cfg_summary.clone(),
            retention_minutes,
            initial_records,
            ephemeral_max_duration_secs,
            ephemeral_min_distinct_ids,
            max_windows_per_app,
            ephemeral_app_max_duration_secs,
            ephemeral_app_min_distinct_procs,
            storage_metrics_rx: storage_metrics_watch(),
        },
    )
    .context("spawn pipeline snapshot stage")?;
    handles.push(snapshot_handle);

    let envelope_handle = spawn_envelope_stage(
        &threads,
        EnvelopeStageConfig {
            receiver: derive_rx,
            hints_tx: hints_tx.clone(),
            compression_tx: compression_tx.clone(),
            snapshot_tx: snapshot_tx.clone(),
            storage_tx: storage_tx.clone(),
            active_grace_secs,
            idle_threshold_secs,
        },
    )
    .context("spawn pipeline envelope stage")?;
    handles.push(envelope_handle);

    let ingest_handle =
        spawn_ingest_stage(&threads, event_rx, derive_tx).context("spawn pipeline ingest stage")?;
    handles.push(ingest_handle);

    Ok(PipelineHandles { handles })
}

pub use types::{CompressionCommand, DeriveCommand, FlushReason, SnapshotMessage, SnapshotUpdate};

fn hydrate_recent_records(
    storage: &mut dyn StorageBackend,
    retention_minutes: u64,
) -> Vec<ActivityRecord> {
    let retention = chrono::Duration::minutes(retention_minutes as i64);
    let since = Utc::now() - retention;
    match storage.fetch_records_since(since) {
        Ok(records) => {
            if !records.is_empty() {
                info!(
                    "Hydrated {} records from storage (since {})",
                    records.len(),
                    since
                );
            }
            records
        }
        Err(err) => {
            error!(
                "Failed to hydrate historical records from storage (since {}): {}",
                since, err
            );
            Vec::new()
        }
    }
}
