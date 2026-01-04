use crate::daemon::compressor::CompressionEngine;
use crate::daemon::events::RawEvent;
use crate::daemon::events::model::{EventKind, SignalKind};
use crate::daemon::runtime::{ThreadHandle, ThreadRegistry};
use crate::storage::StorageCommand;
use crate::util::logging::{debug, error, info};
use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use kronical_core::compression::CompactEvent;
use std::collections::HashSet;
use std::time::Duration;

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
        let mut rollup = RollupStats::new(Duration::from_secs(60));
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
                            rollup.add_batch(reason, &raw_stats, &compact_stats);
                            rollup.maybe_flush();
                            debug!(
                                "Compression batch stats (reason={:?}): Raw (focus/keyboard/mouse): {}/{}/{}; Compact (focus/keyboard/scroll/move): {}/{}/{}/{}",
                                reason,
                                raw_stats.focus,
                                raw_stats.keyboard,
                                raw_stats.mouse,
                                compact_stats.focus,
                                compact_stats.keyboard,
                                compact_stats.scroll,
                                compact_stats.mouse_traj,
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
                    rollup.flush();
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
}

struct RollupStats {
    interval: Duration,
    next_flush: std::time::Instant,
    batches: u64,
    raw: RawEventStats,
    compact: CompactEventStats,
    by_reason: std::collections::HashMap<super::types::FlushReason, u64>,
}

impl RollupStats {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_flush: std::time::Instant::now() + interval,
            batches: 0,
            raw: RawEventStats::default(),
            compact: CompactEventStats::default(),
            by_reason: std::collections::HashMap::new(),
        }
    }

    fn add_batch(
        &mut self,
        reason: super::types::FlushReason,
        raw: &RawEventStats,
        compact: &CompactEventStats,
    ) {
        self.batches += 1;
        self.raw.total += raw.total;
        self.raw.keyboard += raw.keyboard;
        self.raw.mouse += raw.mouse;
        self.raw.focus += raw.focus;
        self.compact.total += compact.total;
        self.compact.scroll += compact.scroll;
        self.compact.mouse_traj += compact.mouse_traj;
        self.compact.keyboard += compact.keyboard;
        self.compact.focus += compact.focus;
        *self.by_reason.entry(reason).or_insert(0) += 1;
    }

    fn maybe_flush(&mut self) {
        if std::time::Instant::now() >= self.next_flush {
            self.flush();
            self.next_flush = std::time::Instant::now() + self.interval;
        }
    }

    fn flush(&mut self) {
        if self.batches == 0 {
            return;
        }
        let timeout = self
            .by_reason
            .get(&super::types::FlushReason::Timeout)
            .copied()
            .unwrap_or(0);
        let shutdown = self
            .by_reason
            .get(&super::types::FlushReason::Shutdown)
            .copied()
            .unwrap_or(0);
        info!(
            "Compression rollup ({} batches, timeout={}, shutdown={}): Raw (focus/keyboard/mouse): {}/{}/{}; Compact (focus/keyboard/scroll/move): {}/{}/{}/{}",
            self.batches,
            timeout,
            shutdown,
            self.raw.focus,
            self.raw.keyboard,
            self.raw.mouse,
            self.compact.focus,
            self.compact.keyboard,
            self.compact.scroll,
            self.compact.mouse_traj,
        );
        self.batches = 0;
        self.raw = RawEventStats::default();
        self.compact = CompactEventStats::default();
        self.by_reason.clear();
    }
}
