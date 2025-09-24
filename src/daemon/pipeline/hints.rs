use crate::daemon::events::model::EventEnvelope;
use crate::daemon::records::{ActivityState, RecordBuilder};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::storage::StorageCommand;
use anyhow::Result;
use log::{error, info};
use std::sync::mpsc;

use super::types::SnapshotMessage;

pub fn spawn_hints_stage(
    threads: &ThreadRegistry,
    hints_rx: mpsc::Receiver<EventEnvelope>,
    storage_tx: mpsc::Sender<StorageCommand>,
    snapshot_tx: mpsc::Sender<SnapshotMessage>,
) -> Result<ThreadHandle> {
    let threads = threads.clone();
    threads.spawn("pipeline-hints", move || {
        info!("Pipeline hints stage started");
        let mut record_builder = RecordBuilder::new(ActivityState::Inactive);

        while let Ok(env) = hints_rx.recv() {
            if let Err(e) = storage_tx.send(StorageCommand::Envelope(env.clone())) {
                error!("Failed to enqueue hint envelope for storage: {}", e);
            }
            if let Some(record) = record_builder.on_hint(&env) {
                if let Err(e) = storage_tx.send(StorageCommand::Record(record.clone())) {
                    error!("Failed to enqueue activity record: {}", e);
                }
                let _ = snapshot_tx.send(SnapshotMessage::Record(record));
            }
        }

        if let Some(final_record) = record_builder.finalize_all() {
            if let Err(e) = storage_tx.send(StorageCommand::Record(final_record.clone())) {
                error!("Failed to enqueue final activity record: {}", e);
            }
            let _ = snapshot_tx.send(SnapshotMessage::Record(final_record));
        }

        let _ = snapshot_tx.send(SnapshotMessage::HintsComplete);
        info!("Pipeline hints stage exiting");
    })
}
