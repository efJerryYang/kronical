# Architecture Notes

This document captures the current daemon pipeline, state machine, and transport
surface. It is intentionally service-focused and excludes the planned Kronii
agent work (tracked elsewhere).

## Pipeline Overview

- Coordinator (`src/daemon/coordinator.rs`) owns global hooks and emits
  `KronicalEvent`s.
- Pipeline stages (`src/daemon/pipeline/*.rs`) are connected by
  `crossbeam_channel` queues:
  - `ingest` batches raw input and emits periodic ticks.
  - `envelope` adapts events into envelopes and derives downstream commands.
  - `hints` owns `RecordBuilder` and emits finalized `ActivityRecord`s.
  - `compression` produces compact summaries from raw events.
  - `snapshot_stage` publishes snapshots and transitions to the `SnapshotBus`.
  - `storage` drains persistence commands for the configured backend.

Channel topology:
`ingest -> envelope -> {hints, compression, snapshot_stage, storage}`,
`hints -> {snapshot_stage, storage}`,
`compression -> storage`,
`snapshot_stage -> storage` (final shutdown).

## State Machine (Signal-Driven)

Signals are derived from raw inputs and ticks. The state machine transitions are
driven exclusively by `SignalKind` emissions; record generation is driven by
`HintKind` envelopes.

| From                     | Trigger                          | Guard/Condition                     | To        | Actions |
|--------------------------|----------------------------------|-------------------------------------|-----------|---------|
| Any                      | LockStart                        | —                                   | Locked    | Emit `StateChanged`, suppress raw input persistence while locked. |
| Locked                   | LockEnd                          | Determine post-unlock state         | Active/Passive/Inactive | Emit `StateChanged`, resume persistence. |
| Inactive/Passive         | Keyboard/Mouse/App/Window/Pulse  | —                                   | Active    | Emit `StateChanged`, emit focus/title hints as applicable. |
| Active                   | Tick                             | no input for `active_grace_secs`    | Passive   | Emit `StateChanged`. |
| Passive                  | Tick                             | no input for `idle_threshold_secs`  | Inactive  | Emit `StateChanged`. |

Hints derived from signals (`FocusChanged`, `TitleChanged`, `StateChanged`,
`ActivityPulse`, etc.) drive record building and storage updates.

## Snapshot Bus

`crates/core::snapshot::SnapshotBus` is the in-memory distribution point for
snapshots and transitions. Snapshot payloads include:

- current activity state, focus, cadence, counts
- `runId`
- recent transitions (buffered)
- aggregated app/window durations
- **full in-memory `records` list** (same run ID, retention window)

## API Surfaces

HTTP (Unix socket):

- `GET /v1/snapshot` -> JSON snapshot
- `GET /v1/stream` -> SSE stream of snapshots

gRPC (Unix socket):

- `Snapshot` -> snapshot payload
- `Watch` -> streaming snapshots
- `GetSystemMetrics` -> system tracker metrics (query via tracker thread)

## Storage & Hydration

- `activity_records`, `compact_events`, and `recent_transitions` are persisted.
- On startup, the pipeline hydrates records/transitions filtered by:
  - same `runId` (when present)
  - retention window (`retention_minutes`) for records
  - retention window for transitions (plus a cap)
