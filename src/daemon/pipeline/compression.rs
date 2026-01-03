use crate::daemon::compressor::CompressionEngine;
use crate::daemon::events::RawEvent;
use crate::daemon::events::model::{EventKind, SignalKind};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::storage::StorageCommand;
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use kronical_core::compression::CompactEvent;
use crate::util::logging::{debug, error, info};
use std::collections::HashSet;

use super::types::CompressionCommand;

pub struct CompressionStageConfig {
    pub receiver: Receiver<CompressionCommand>,
    pub storage_tx: Sender<StorageCommand>,
    pub focus_interner_max_strings: usize,
    pub persist_raw_events: bool,
}

pub fn spawn_compression_stage(
    threads: &ThreadRegistry,
    config: CompressionStageConfig,
) -> Result<ThreadHandle> {
    let CompressionStageConfig {
        receiver,
        storage_tx,
        focus_interner_max_strings,
        persist_raw_events,
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
                    let reason = batch.reason;
                    let raw_stats = RawEventStats::from_events(&batch.events);
                    debug!(
                        "Compression stage processing {} raw events",
                        batch.events.len()
                    );
                    match engine.compress_events(batch.events) {
                        Ok((processed_events, mut compact_events)) => {
                            let compact_stats =
                                CompactEventStats::from_events(&compact_events);
                            info!(
                                "Compression batch stats (reason={:?}): raw_total={} raw_keyboard={} ({:.1}%) raw_mouse={} ({:.1}%) raw_focus={} ({:.1}%) compact_total={} compact_scroll={} ({:.1}%) compact_mouse_traj={} ({:.1}%) compact_keyboard={} ({:.1}%) compact_focus={} ({:.1}%)",
                                reason,
                                raw_stats.total,
                                raw_stats.keyboard,
                                raw_stats.pct(raw_stats.keyboard),
                                raw_stats.mouse,
                                raw_stats.pct(raw_stats.mouse),
                                raw_stats.focus,
                                raw_stats.pct(raw_stats.focus),
                                compact_stats.total,
                                compact_stats.scroll,
                                compact_stats.pct(compact_stats.scroll),
                                compact_stats.mouse_traj,
                                compact_stats.pct(compact_stats.mouse_traj),
                                compact_stats.keyboard,
                                compact_stats.pct(compact_stats.keyboard),
                                compact_stats.focus,
                                compact_stats.pct(compact_stats.focus),
                            );
                            if persist_raw_events {
                                forward_raw_events(&storage_tx, &envelopes, processed_events);
                            } else {
                                clear_raw_event_refs(&mut compact_events);
                            }
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

fn clear_raw_event_refs(compact_events: &mut [CompactEvent]) {
    for event in compact_events {
        match event {
            CompactEvent::Scroll(s) => s.raw_event_ids = None,
            CompactEvent::MouseTrajectory(t) => t.raw_event_ids = None,
            CompactEvent::Keyboard(k) => k.raw_event_ids = None,
            CompactEvent::Focus(f) => f.raw_event_ids = None,
        }
    }
}

#[derive(Debug, Default)]
struct RawEventStats {
    total: usize,
    keyboard: usize,
    mouse: usize,
    focus: usize,
}

impl RawEventStats {
    fn from_events(events: &[RawEvent]) -> Self {
        let mut stats = RawEventStats::default();
        for event in events {
            stats.total += 1;
            match event {
                RawEvent::KeyboardInput { .. } => stats.keyboard += 1,
                RawEvent::MouseInput { .. } => stats.mouse += 1,
                RawEvent::WindowFocusChange { .. } => stats.focus += 1,
            }
        }
        stats
    }

    fn pct(&self, count: usize) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (count as f32 / self.total as f32) * 100.0
        }
    }
}

#[derive(Debug, Default)]
struct CompactEventStats {
    total: usize,
    scroll: usize,
    mouse_traj: usize,
    keyboard: usize,
    focus: usize,
}

impl CompactEventStats {
    fn from_events(events: &[CompactEvent]) -> Self {
        let mut stats = CompactEventStats::default();
        for event in events {
            stats.total += 1;
            match event {
                CompactEvent::Scroll(_) => stats.scroll += 1,
                CompactEvent::MouseTrajectory(_) => stats.mouse_traj += 1,
                CompactEvent::Keyboard(_) => stats.keyboard += 1,
                CompactEvent::Focus(_) => stats.focus += 1,
            }
        }
        stats
    }

    fn pct(&self, count: usize) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (count as f32 / self.total as f32) * 100.0
        }
    }
}
