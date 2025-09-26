use crate::daemon::coordinator::KronicalEvent;
use crate::daemon::events::{KeyboardEventData, MouseEventData, MousePosition, RawEvent};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use anyhow::{Result, anyhow};
use chrono::Utc;
use crossbeam_channel::{Receiver, Sender};
use log::{debug, error, info, trace};
use std::time::Duration;

use super::types::{DeriveCommand, FlushReason, RawBatch};

pub fn spawn_ingest_stage(
    threads: &ThreadRegistry,
    event_rx: Receiver<KronicalEvent>,
    derive_tx: Sender<DeriveCommand>,
) -> Result<ThreadHandle> {
    let threads = threads.clone();
    threads.spawn("pipeline-ingest", move || {
        info!("Pipeline ingest thread started");
        let mut pending: Vec<RawEvent> = Vec::new();
        let mut event_count = 0u64;
        let ticker = crossbeam_channel::tick(Duration::from_millis(50));

        loop {
            let mut should_tick = false;
            crossbeam_channel::select! {
                recv(event_rx) -> msg => {
                    match msg {
                        Ok(KronicalEvent::Shutdown) => {
                            info!("Ingest received shutdown signal");
                            if !pending.is_empty() {
                                let batch = RawBatch::new(std::mem::take(&mut pending), FlushReason::Shutdown);
                                if derive_tx.send(DeriveCommand::Batch(batch)).is_err() {
                                    break;
                                }
                            }
                            let _ = derive_tx.send(DeriveCommand::Shutdown);
                            break;
                        }
                        Ok(event) => {
                            event_count += 1;
                            debug!("Ingest received event #{}: {:?}", event_count, event);
                            match convert_kronid_to_raw(event) {
                                Ok(raw) => {
                                    trace!(
                                        "Queued raw event {} (pending {})",
                                        raw.event_id(),
                                        pending.len() + 1
                                    );
                                    pending.push(raw);
                                    should_tick = true;
                                }
                                Err(e) => error!("Failed to convert event to raw: {}", e),
                            }
                        }
                        Err(_) => {
                            info!("Event source disconnected; ingest stopping");
                            break;
                        }
                    }
                }
                recv(ticker) -> _ => {
                    if pending.is_empty() {
                        continue;
                    }
                    debug!("Ingest flushing {} pending raw events", pending.len());
                    let batch = RawBatch::new(std::mem::take(&mut pending), FlushReason::Timeout);
                    if derive_tx.send(DeriveCommand::Batch(batch)).is_err() {
                        break;
                    }
                    should_tick = true;
                }
            }

            if should_tick {
                let now = Utc::now();
                if derive_tx.send(DeriveCommand::Tick(now)).is_err() {
                    break;
                }
            }
        }

        info!("Pipeline ingest thread exiting");
    })
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn convert_kronid_to_raw(event: KronicalEvent) -> Result<RawEvent> {
    let now = Utc::now();
    let event_id = now.timestamp_millis() as u64;

    match event {
        KronicalEvent::KeyboardInput => Ok(RawEvent::KeyboardInput {
            timestamp: now,
            event_id,
            data: KeyboardEventData {
                key_code: None,
                key_char: None,
                modifiers: Vec::new(),
            },
        }),
        KronicalEvent::MouseInput {
            x,
            y,
            button,
            clicks,
            kind,
        } => Ok(RawEvent::MouseInput {
            timestamp: now,
            event_id,
            data: MouseEventData {
                position: MousePosition { x, y },
                button,
                click_count: if clicks > 0 { Some(clicks) } else { None },
                event_type: Some(kind),
                wheel_amount: None,
                wheel_rotation: None,
                wheel_axis: None,
                wheel_scroll_type: None,
            },
        }),
        KronicalEvent::MouseWheel {
            clicks,
            x,
            y,
            scroll_type,
            amount,
            rotation,
            axis,
        } => Ok(RawEvent::MouseInput {
            timestamp: now,
            event_id,
            data: MouseEventData {
                position: MousePosition { x, y },
                button: None,
                click_count: Some(clicks as u16),
                event_type: None,
                wheel_amount: Some(amount as i32),
                wheel_rotation: Some(rotation as i32),
                wheel_axis: axis,
                wheel_scroll_type: Some(scroll_type),
            },
        }),
        KronicalEvent::WindowFocusChange { focus_info } => Ok(RawEvent::WindowFocusChange {
            timestamp: now,
            event_id,
            focus_info,
        }),
        KronicalEvent::Shutdown => Err(anyhow!("Cannot convert Shutdown event to RawEvent")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pipeline::types::DeriveCommand;
    use std::time::Duration;

    #[test]
    fn ingest_flushes_pending_events_on_shutdown() {
        let registry = ThreadRegistry::new();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (derive_tx, derive_rx) = crossbeam_channel::unbounded();

        let handle = spawn_ingest_stage(&registry, event_rx, derive_tx).unwrap();

        event_tx.send(KronicalEvent::KeyboardInput).unwrap();
        let tick = derive_rx.recv_timeout(Duration::from_millis(200)).unwrap();
        match tick {
            DeriveCommand::Tick(_) => {}
            other => panic!("expected tick, got {other:?}"),
        }

        event_tx.send(KronicalEvent::Shutdown).unwrap();
        let batch = derive_rx.recv_timeout(Duration::from_millis(200)).unwrap();
        match batch {
            DeriveCommand::Batch(raw_batch) => {
                assert_eq!(raw_batch.events.len(), 1);
            }
            other => panic!("expected batch, got {other:?}"),
        }

        let shutdown = derive_rx.recv_timeout(Duration::from_millis(200)).unwrap();
        match shutdown {
            DeriveCommand::Shutdown => {}
            other => panic!("expected shutdown, got {other:?}"),
        }

        handle.join().unwrap();
    }
}
