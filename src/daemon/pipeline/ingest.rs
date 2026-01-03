use crate::daemon::coordinator::KronicalEvent;
use crate::daemon::events::{KeyboardEventData, MouseEventData, MousePosition, RawEvent};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::util::logging::{debug, error, info, trace};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use crossbeam_channel::{Receiver, Sender};
use std::time::{Duration, Instant};

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
        let mut last_event_ms: u64 = 0;
        let mut event_seq: u64 = 0;
        let ticker = crossbeam_channel::tick(Duration::from_millis(50));
        let idle_tick_interval = Duration::from_millis(500);
        let mut last_idle_tick = Instant::now();

        loop {
            let mut tick_due: Option<chrono::DateTime<chrono::Utc>> = None;
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
                            let (now, event_id) =
                                next_event_id(&mut last_event_ms, &mut event_seq);
                            match convert_kronid_to_raw(event, now, event_id) {
                                Ok(raw) => {
                                    trace!(
                                        "Queued raw event {} (pending {})",
                                        raw.event_id(),
                                        pending.len() + 1
                                    );
                                    pending.push(raw);
                                    tick_due = Some(Utc::now());
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
                        if last_idle_tick.elapsed() >= idle_tick_interval {
                            tick_due = Some(Utc::now());
                        }
                    } else {
                        debug!("Ingest flushing {} pending raw events", pending.len());
                        let batch = RawBatch::new(std::mem::take(&mut pending), FlushReason::Timeout);
                        if derive_tx.send(DeriveCommand::Batch(batch)).is_err() {
                            break;
                        }
                        tick_due = Some(Utc::now());
                    }
                }
            }

            if let Some(now) = tick_due {
                if derive_tx.send(DeriveCommand::Tick(now)).is_err() {
                    break;
                }
                last_idle_tick = Instant::now();
            }
        }

        info!("Pipeline ingest thread exiting");
    })
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
fn convert_kronid_to_raw(
    event: KronicalEvent,
    now: DateTime<Utc>,
    event_id: u64,
) -> Result<RawEvent> {
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

const EVENT_ID_SEQ_BITS: u64 = 12;
const EVENT_ID_SEQ_MASK: u64 = (1 << EVENT_ID_SEQ_BITS) - 1;

fn next_event_id(last_event_ms: &mut u64, event_seq: &mut u64) -> (DateTime<Utc>, u64) {
    let mut now = Utc::now();
    let mut now_ms = now.timestamp_millis() as u64;

    if now_ms == *last_event_ms {
        if *event_seq >= EVENT_ID_SEQ_MASK {
            loop {
                now = Utc::now();
                now_ms = now.timestamp_millis() as u64;
                if now_ms != *last_event_ms {
                    break;
                }
            }
            *last_event_ms = now_ms;
            *event_seq = 0;
        } else {
            *event_seq += 1;
        }
    } else {
        *last_event_ms = now_ms;
        *event_seq = 0;
    }

    let event_id = (now_ms << EVENT_ID_SEQ_BITS) | *event_seq;
    (now, event_id)
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
