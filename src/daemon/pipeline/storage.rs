use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::storage::{StorageBackend, StorageCommand};
use anyhow::Result;
use crossbeam_channel::Receiver;
use log::{error, info};

pub fn spawn_storage_stage(
    threads: &ThreadRegistry,
    storage_rx: Receiver<StorageCommand>,
    mut storage: Box<dyn StorageBackend>,
) -> Result<ThreadHandle> {
    let threads = threads.clone();
    threads.spawn("pipeline-storage", move || {
        info!("Pipeline storage stage started");
        while let Ok(cmd) = storage_rx.recv() {
            match cmd {
                StorageCommand::RawEvent(event) => {
                    if let Err(e) = storage.add_events(vec![event]) {
                        error!("store add_events error: {}", e);
                    }
                }
                StorageCommand::Record(record) => {
                    if let Err(e) = storage.add_records(vec![record]) {
                        error!("store add_records error: {}", e);
                    }
                }
                StorageCommand::Envelope(env) => {
                    if let Err(e) = storage.add_envelopes(vec![env]) {
                        error!("store add_envelopes error: {}", e);
                    }
                }
                StorageCommand::CompactEvents(events) => {
                    if let Err(e) = storage.add_compact_events(events) {
                        error!("store add_compact_events error: {}", e);
                    }
                }
                StorageCommand::Transition(transition) => {
                    if let Err(e) = storage.add_transitions(vec![transition]) {
                        error!("store add_transitions error: {}", e);
                    }
                }
                StorageCommand::Shutdown => {
                    info!("Storage stage received shutdown");
                    break;
                }
            }
        }
        info!("Pipeline storage stage exiting");
    })
}
