use crate::daemon::compressor::CompressionEngine;
use crate::daemon::events::RawEvent;
use crate::daemon::events::model::{EventKind, SignalKind};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::storage::StorageCommand;
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use kronical_core::compression::CompactEvent;
use log::{debug, error, info};
use std::collections::HashSet;

use super::types::CompressionCommand;

pub struct CompressionStageConfig {
    pub receiver: Receiver<CompressionCommand>,
    pub storage_tx: Sender<StorageCommand>,
    pub focus_interner_max_strings: usize,
}

pub fn spawn_compression_stage(
    threads: &ThreadRegistry,
    config: CompressionStageConfig,
) -> Result<ThreadHandle> {
    let CompressionStageConfig {
        receiver,
        storage_tx,
        focus_interner_max_strings,
    } = config;

    let threads = threads.clone();
    threads.spawn("pipeline-compression", move || {
        info!("Pipeline compression stage started");
        let mut engine = CompressionEngine::with_focus_cap(focus_interner_max_strings);
        while let Ok(cmd) = receiver.recv() {
            match cmd {
                CompressionCommand::Process { batch, envelopes } => {
                    if batch.events.is_empty() {
                        continue;
                    }
                    debug!(
                        "Compression stage processing {} raw events",
                        batch.events.len()
                    );
                    match engine.compress_events(batch.events) {
                        Ok((processed_events, compact_events)) => {
                            forward_raw_events(&storage_tx, &envelopes, processed_events);
                            forward_compact_events(&storage_tx, compact_events);
                        }
                        Err(e) => error!("Failed to compress events: {}", e),
                    }
                }
                CompressionCommand::Shutdown => {
                    info!("Compression stage received shutdown");
                    break;
                }
            }
        }
        info!("Pipeline compression stage exiting");
    })
}

fn forward_raw_events(
    storage_tx: &Sender<StorageCommand>,
    envelopes: &[crate::daemon::events::model::EventEnvelope],
    processed_events: Vec<RawEvent>,
) {
    if processed_events.is_empty() {
        return;
    }

    let mut suppress: HashSet<u64> = HashSet::new();
    let mut locked = false;
    for env in envelopes {
        if let EventKind::Signal(kind) = &env.kind {
            match kind {
                SignalKind::LockStart => locked = true,
                SignalKind::LockEnd => locked = false,
                SignalKind::KeyboardInput | SignalKind::MouseInput if locked => {
                    suppress.insert(env.id);
                }
                _ => {}
            }
        }
    }

    for event in processed_events.into_iter() {
        let should_store = match &event {
            RawEvent::KeyboardInput { event_id, .. } | RawEvent::MouseInput { event_id, .. } => {
                !suppress.contains(event_id)
            }
            RawEvent::WindowFocusChange { .. } => true,
        };
        if should_store {
            if let Err(e) = storage_tx.send(StorageCommand::RawEvent(event)) {
                error!("Failed to enqueue raw event for storage: {}", e);
                break;
            }
        }
    }
}

fn forward_compact_events(storage_tx: &Sender<StorageCommand>, compact_events: Vec<CompactEvent>) {
    if compact_events.is_empty() {
        return;
    }
    if let Err(e) = storage_tx.send(StorageCommand::CompactEvents(compact_events)) {
        error!("Failed to enqueue compact events: {}", e);
    }
}
