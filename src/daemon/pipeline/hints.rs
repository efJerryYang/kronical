use crate::daemon::events::model::EventEnvelope;
use crate::daemon::records::{ActivityRecord, ActivityState, RecordBuilder};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::storage::StorageCommand;
use crate::util::logging::{error, info};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};

use super::types::SnapshotMessage;

pub fn spawn_hints_stage(
    threads: &ThreadRegistry,
    hints_rx: Receiver<EventEnvelope>,
    storage_tx: Sender<StorageCommand>,
    snapshot_tx: Sender<SnapshotMessage>,
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
                info!("Record finalized: {}", record_summary(&record));
                if let Err(e) = storage_tx.send(StorageCommand::Record(record.clone())) {
                    error!("Failed to enqueue activity record: {}", e);
                }
                let _ = snapshot_tx.send(SnapshotMessage::Record(record));
            }
        }

        if let Some(final_record) = record_builder.finalize_all() {
            info!("Final record finalized: {}", record_summary(&final_record));
            if let Err(e) = storage_tx.send(StorageCommand::Record(final_record.clone())) {
                error!("Failed to enqueue final activity record: {}", e);
            }
            let _ = snapshot_tx.send(SnapshotMessage::Record(final_record));
        }

        let _ = snapshot_tx.send(SnapshotMessage::HintsComplete);
        info!("Pipeline hints stage exiting");
    })
}

fn record_summary(record: &ActivityRecord) -> String {
    let focus = match &record.focus_info {
        Some(info) => format!(
            "app={} window={} pid={} wid={}",
            info.app_name, info.window_title, info.pid, info.window_id
        ),
        None => "focus=none".to_string(),
    };
    let end_time = record
        .end_time
        .map(|t| t.to_string())
        .unwrap_or_else(|| "open".to_string());
    format!(
        "id={} state={:?} start={} end={} events={} triggers={} {}",
        record.record_id,
        record.state,
        record.start_time,
        end_time,
        record.event_count,
        record.triggering_events.len(),
        focus
    )
}
