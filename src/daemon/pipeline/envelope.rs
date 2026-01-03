use crate::daemon::events::adapter::EventAdapter;
use crate::daemon::events::derive_hint::StateDeriver;
use crate::daemon::events::derive_signal::LockDeriver;
use crate::daemon::events::model::{
    EventEnvelope, EventKind, EventPayload, EventSource, HintKind, SignalKind,
};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::daemon::snapshot;
use crate::storage::StorageCommand;
use anyhow::Result;
use chrono::Utc;
use crossbeam_channel::{Receiver, Sender};
use crate::util::logging::{debug, error, info, trace};

use super::types::{CompressionCommand, DeriveCommand, SnapshotMessage, SnapshotUpdate};

pub struct EnvelopeStageConfig {
    pub receiver: Receiver<DeriveCommand>,
    pub hints_tx: Sender<EventEnvelope>,
    pub compression_tx: Sender<CompressionCommand>,
    pub snapshot_tx: Sender<SnapshotMessage>,
    pub storage_tx: Sender<StorageCommand>,
    pub active_grace_secs: u64,
    pub idle_threshold_secs: u64,
}

pub fn spawn_envelope_stage(
    threads: &ThreadRegistry,
    config: EnvelopeStageConfig,
) -> Result<ThreadHandle> {
    let EnvelopeStageConfig {
        receiver,
        hints_tx,
        compression_tx,
        snapshot_tx,
        storage_tx,
        active_grace_secs,
        idle_threshold_secs,
    } = config;

    let threads = threads.clone();
    threads.spawn("pipeline-envelope", move || {
        info!("Pipeline envelope stage started");
        let mut adapter = EventAdapter::new();
        let mut lock_deriver = LockDeriver::new();
        let mut state_deriver = StateDeriver::new(
            Utc::now(),
            active_grace_secs as i64,
            idle_threshold_secs as i64,
        );

        while let Ok(cmd) = receiver.recv() {
            match cmd {
                DeriveCommand::Batch(batch) => {
                    if batch.is_empty() {
                        continue;
                    }
                    debug!(
                        "Envelope stage processing {} raw events",
                        batch.events.len()
                    );
                    let envelopes = adapter.adapt_batch(&batch.events);
                    let envelopes_with_lock = lock_deriver.derive(&envelopes);
                    let mut update = SnapshotUpdate::default();
                    let mut send_failed = false;

                    for env in envelopes_with_lock.iter() {
                        trace!("Envelope stage handling envelope {:?}", env.kind);
                        match &env.kind {
                            EventKind::Hint(_) => {
                                update.hints_delta += 1;
                                apply_focus_update(&mut update, env);
                                apply_state_update_hint(&mut update, env, None);
                                if let Err(e) = hints_tx.send(env.clone()) {
                                    error!("Failed to send hint envelope: {}", e);
                                }
                                if update.transition.is_some()
                                    && !flush_snapshot_update(&snapshot_tx, &mut update)
                                {
                                    send_failed = true;
                                    break;
                                }
                            }
                            EventKind::Signal(signal_kind) => {
                                update.signals_delta += 1;
                                apply_focus_update(&mut update, env);
                                apply_state_update_signal(&mut update, env, signal_kind);
                                if let Err(e) =
                                    storage_tx.send(StorageCommand::Envelope(env.clone()))
                                {
                                    error!("Failed to enqueue signal envelope for storage: {}", e);
                                }
                                if update.transition.is_some()
                                    && !flush_snapshot_update(&snapshot_tx, &mut update)
                                {
                                    send_failed = true;
                                    break;
                                }
                                if let Some(hint) = state_deriver.on_signal(env) {
                                    update.hints_delta += 1;
                                    apply_focus_update(&mut update, &hint);
                                    apply_state_update_hint(&mut update, &hint, Some(signal_kind));
                                    if let Err(e) = hints_tx.send(hint.clone()) {
                                        error!("Failed to forward derived hint: {}", e);
                                    }
                                    if update.transition.is_some()
                                        && !flush_snapshot_update(&snapshot_tx, &mut update)
                                    {
                                        send_failed = true;
                                        break;
                                    }
                                }
                            }
                        }
                    }

                    if send_failed {
                        break;
                    }

                    let job = CompressionCommand::Process {
                        batch,
                        envelopes: envelopes_with_lock,
                    };
                    if compression_tx.send(job).is_err() {
                        error!("Compression stage channel closed; stopping envelope stage");
                        break;
                    }

                    if !update.is_empty() {
                        if snapshot_tx
                            .send(SnapshotMessage::Update(std::mem::take(&mut update)))
                            .is_err()
                        {
                            error!("Snapshot stage channel closed; stopping envelope stage");
                            break;
                        }
                    }
                    if snapshot_tx.send(SnapshotMessage::Publish).is_err() {
                        error!("Snapshot stage channel closed; stopping envelope stage");
                        break;
                    }
                }
                DeriveCommand::Tick(now) => {
                    if let Some(state_hint) = state_deriver.on_tick(now) {
                        debug!("Envelope stage emitting state hint after tick");
                        let mut update = SnapshotUpdate::default();
                        update.hints_delta += 1;
                        apply_focus_update(&mut update, &state_hint);
                        apply_state_update_hint(&mut update, &state_hint, None);
                        if let Err(e) = hints_tx.send(state_hint.clone()) {
                            error!("Failed to forward tick-derived hint: {}", e);
                        }
                        if update.transition.is_some()
                            && !flush_snapshot_update(&snapshot_tx, &mut update)
                        {
                            break;
                        }
                        if !update.is_empty() {
                            if snapshot_tx
                                .send(SnapshotMessage::Update(std::mem::take(&mut update)))
                                .is_err()
                            {
                                error!("Snapshot stage channel closed; stopping envelope stage");
                                break;
                            }
                        }
                        if snapshot_tx.send(SnapshotMessage::Publish).is_err() {
                            error!("Snapshot stage channel closed; stopping envelope stage");
                            break;
                        }
                    }
                }
                DeriveCommand::Shutdown => {
                    info!("Envelope stage received shutdown");
                    let _ = snapshot_tx.send(SnapshotMessage::Shutdown);
                    let _ = compression_tx.send(CompressionCommand::Shutdown);
                    break;
                }
            }
        }

        info!("Pipeline envelope stage exiting");
    })
}

fn apply_focus_update(update: &mut SnapshotUpdate, env: &EventEnvelope) {
    if let EventPayload::Focus(focus) = &env.payload {
        update.focus = Some(focus.clone());
    } else if let EventPayload::Title { window_id, title } = &env.payload {
        update.focus_title = Some((*window_id, title.clone()));
    }
}

fn apply_state_update_hint(
    update: &mut SnapshotUpdate,
    env: &EventEnvelope,
    signal_kind: Option<&SignalKind>,
) {
    if let EventPayload::State { from, to } = &env.payload {
        let transition = snapshot::Transition {
            from: *from,
            to: *to,
            at: env.timestamp,
            by_signal: transition_reason(env, signal_kind),
            run_id: None,
        };
        update.transition = Some(transition);
        update.state = Some(*to);
    }
}

fn apply_state_update_signal(
    update: &mut SnapshotUpdate,
    env: &EventEnvelope,
    signal_kind: &SignalKind,
) {
    if let EventPayload::State { from, to } = &env.payload {
        let transition = snapshot::Transition {
            from: *from,
            to: *to,
            at: env.timestamp,
            by_signal: transition_reason(env, Some(signal_kind)),
            run_id: None,
        };
        update.transition = Some(transition);
        update.state = Some(*to);
    }
}

fn transition_reason(env: &EventEnvelope, signal_kind: Option<&SignalKind>) -> Option<String> {
    if let Some(kind) = signal_kind {
        Some(format!("{kind:?}"))
    } else if matches!(env.kind, EventKind::Hint(HintKind::StateChanged))
        && matches!(env.source, EventSource::Derived)
    {
        Some("IdleTimeout".to_string())
    } else {
        None
    }
}

fn flush_snapshot_update(
    snapshot_tx: &Sender<SnapshotMessage>,
    update: &mut SnapshotUpdate,
) -> bool {
    if update.is_empty() {
        return true;
    }
    let payload = std::mem::take(update);
    if snapshot_tx.send(SnapshotMessage::Update(payload)).is_err() {
        error!("Snapshot stage channel closed; stopping envelope stage");
        false
    } else {
        true
    }
}
