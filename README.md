# Kronical

Kronical is a local activity-tracking service. The daemon (`kronid`) observes
system hooks, derives higher-level signals, and writes activity records that
other clients (current CLI, future Kronii UI agent) can consume over HTTP/gRPC.

This repository focuses on the service side: hook ingestion, signal-driven
state machine, compression, and storage backends. The UI layer is being built
separately and links to the public crates exposed here.

## Architecture Snapshot

- **Coordinator (`src/daemon/coordinator.rs`)** - runs on the macOS main thread
  and owns global hook handlers. It converts raw inputs into `KronicalEvent`
  messages and pushes them into the pipeline channels.
- **Pipeline (`src/daemon/pipeline/`)** - staged workers linked with
  `crossbeam_channel` queues: `ingest` batches hook output and emits ticks,
  `envelope` adapts events into envelopes, `hints` drives record generation,
  `compression` produces compact summaries, `snapshot_stage` publishes to the
  bus, and `storage` drains persistence commands. The layout keeps the
  signal-driven state machine deterministic without shared locks.
- **Core crate (`crates/core/`)** - domain types and logic: event envelopes,
  signal and hint derivation, record aggregation, compression, and the snapshot
  bus.
- **Common crate (`crates/common/`)** - shared helpers (config loader, path
  utilities, LRU caches, string interning).
- **Storage crate (`crates/storage/`)** - storage facade plus DuckDB and
  SQLite writer threads that drain persistence commands and publish health
  metrics via the snapshot bus.
- **Binaries (`src/bin/`)** - `kronid` wires everything together; `kronictl`
  offers CLI access for lifecycle and data inspection.

When onboarding, start with the coordinator to understand how hooks are
normalized. From there follow the channel hand-offs into `crates/core::events`
for derivation, `crates/core::records` for aggregation, and the storage crate to
see how data lands on disk.

## Repository Layout

- `crates/common/` - shared utilities.
- `crates/core/` - domain logic and snapshot bus.
- `crates/storage/` - storage facade plus integration tests under
  `crates/storage/tests/`.
- `src/daemon/` - runtime wiring: coordinator, pipeline, compressors, API
  surfaces, trackers.
- `src/bin/` - service binaries (`kronid`, `kronictl`).
- `docs/` - design notes (including Kronii orchestration TODOs).

## Getting Started

```bash
cargo build --release
./target/release/kronid         # launches the daemon
./target/release/kronictl help  # CLI overview
```

`kronid` requires macOS Accessibility + Screen Recording permissions to capture
input events and window titles. Without them the daemon will exit early or
return empty focus data.

### Useful `kronictl` commands

- `kronictl start|stop|status|restart` - daemon lifecycle.
- `kronictl snapshot [--pretty]` - fetch the latest snapshot via HTTP.
- `kronictl watch [--pretty]` - follow snapshot updates (SSE).
- `kronictl monitor` - interactive terminal UI (press `q` to quit).
- `kronictl tracker show [--watch]` - inspect system-tracker metrics when the
  tracker is enabled in config.

## State Machine & Signals

The event pipeline produces a deterministic state machine driven by signals:

- States: `Active`, `Passive`, `Inactive`, `Locked`.
- Signals: keyboard/mouse input, focus/title changes, lock/unlock, periodic
  pulses.
- Transitions respect `active_grace_secs` (default 30s) and
  `idle_threshold_secs` (default 300s) to move between states. While the system
  is locked we keep emitting lock/unlock signals but suppress raw input writes.

Hints derived from signals (`FocusChanged`, `TitleChanged`, `StateChanged`,
`ActivityPulse`, ...) drive record generation and downstream snapshots.

## Configuration

User configuration lives at `~/.kronical/config.toml`. Every field can be
overridden with a `KRONICAL_` environment variable (for example,
`KRONICAL_TRACKER_ENABLED=true`). Relevant knobs include workspace paths,
retention, state thresholds, tracker cadence, and cache sizes. See
`crates/common/src/config.rs` for the full schema.

## Storage Backends

DuckDB is the default backend; SQLite remains available for lightweight setups.
Each backend runs in its own thread, draining persistence commands, committing
in batches, and reporting backlog metrics via the snapshot bus. The shared
logical schema covers `raw_events`, `raw_envelopes`, `activity_records`,
`compact_events`, and `recent_transitions`. Raw event identifiers embedded in
compact records use `kronical_core::compression::RawEventPointer`, which stores
the source table name so debug captures can live alongside the default
`raw_events` table without dangling references. Integration tests under
`crates/storage/tests/` validate command execution, health reporting, and data
round trips.

## Compression

`crates/core::compression` converts persisted raw events into compact summary
records. The engine maintains alignment by recording contributing raw event IDs
alongside structured payloads for scroll bursts, mouse trajectories, keyboard
activity, and focus transitions, using optional `RawEventPointer` metadata when
those debug records are retained.

## Development Workflow

- Prefer crate-scoped test runs (`cargo test -p kronical-core`,
  `cargo test -p kronical-storage`, ...) to keep failures focused.
- Tests must exercise observable behavior (state transitions, channel hand-offs,
  storage writes) rather than tautological assertions.
- `make coverage` runs `cargo llvm-cov` with colorised output, writes
  `coverage/summary.txt`, and generates the HTML report under
  `coverage/html/`.
- Use `FEATURE_FLAGS="--features hotpath"` during `make` invocations to enable
  optional profiling hooks, keeping runs single-threaded when necessary.

## Permissions Checklist (macOS)

- Accessibility - required for global input hooks.
- Screen Recording - required for window title capture.

Grant both permissions to `kronid` (or the terminal hosting it) before starting
the daemon.

## Support & Next Steps

Active work targets the service pipeline and storage coverage. Kronii (the UI
agent) will depend on the crates surfaced here once the API stabilises. Track
ongoing refactor plans in `docs/todo`.
