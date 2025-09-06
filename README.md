# Kronical

Activity tracking daemon (kronid) with dual-stream pattern, channel‑based workers, and explicit state transitions.

## State Machine

- States: `Active`, `Passive`, `Inactive`, `Locked`.
- Inputs: `KeyboardInput`, `MouseInput`, `AppChanged`, `WindowChanged`, `ActivityPulse`, `LockStart`, `LockEnd`.
- Parameters: `active_grace_secs` (default 30), `idle_threshold_secs` (default 300).

State transition table (driven by signals and timeouts):

- Any → `Locked` on `LockStart`
- `Locked` → computed(`Active`/`Passive`/`Inactive`) on `LockEnd` (based on last input time)
- `Inactive`/`Passive` → `Active` on any input signal (`KeyboardInput`/`MouseInput`/`AppChanged`/`WindowChanged`/`ActivityPulse`)
- `Active` → `Passive` when no input for `active_grace_secs` (on tick)
- `Passive` → `Inactive` when no input for `idle_threshold_secs` (on tick)

Follow‑up envelopes generated:

- `StateChanged` hint is derived for every state transition (including timer‑based ticks and lock/unlock boundaries).
- Focus changes produce: `AppChanged` or `WindowChanged` signal, then `FocusChanged` hint, then an `ActivityPulse` signal.
- Title changes produce `TitleChanged` hint (polling‑sourced).

Notes:

- While locked, raw keyboard/mouse input envelopes are suppressed from persistence; the lock/unlock signals and hints are still stored.
- Channels are used for coordination: raw events → envelopes (+derived lock signals) → state/hint derivation → record building → storage.

## Dual Streams Pattern

- Events Stream: raw input/focus envelopes (signals + hints)
- Records Stream: activity records segmented by hints (`FocusChanged`, `TitleChanged`, `StateChanged`)

## Commands

```bash
# Daemon control
kronictl start                    # Start kronid daemon
kronictl status                   # Check daemon status
kronictl stop                     # Stop daemon
kronictl restart                  # Restart daemon

# Monitoring and data access
kronictl snapshot                 # Get current snapshot
kronictl snapshot --pretty        # Get formatted snapshot
kronictl watch                    # Watch for changes
kronictl watch --pretty           # Watch with pretty output
kronictl monitor                  # Live TUI (press 'q' to quit)

# System tracking (when enabled in config)
kronictl tracker status           # Show tracker status
kronictl tracker show             # Show tracker data
kronictl tracker show --watch     # Follow tracker updates
```

## Permissions (macOS)

- **Accessibility**: Required for input hooks and window tracking
- **Screen Recording**: Required for window titles

Without these, kronid will abort at launch or show empty titles.

## Storage

- Backends: DuckDB (default) and SQLite3. Both use the same logical schema.
- Tables (SQLite3 DDL):
  - `raw_events(id INTEGER PRIMARY KEY, timestamp TEXT, event_type TEXT, data TEXT)`
  - `raw_envelopes(id INTEGER PRIMARY KEY, event_id INTEGER, timestamp TEXT, source TEXT, kind TEXT, payload TEXT, derived INTEGER, polling INTEGER, sensitive INTEGER)`
  - `activity_records(id INTEGER PRIMARY KEY, start_time TEXT, end_time TEXT NULL, state TEXT, focus_info TEXT NULL)`
  - `compact_events(start_time TEXT, end_time TEXT, kind TEXT, payload TEXT, raw_event_ids TEXT)`
- Indexes: `idx_raw_events_timestamp` on `raw_events(timestamp)`, `idx_raw_envelopes_timestamp` on `raw_envelopes(timestamp)`, `idx_activity_records_start_time` on `activity_records(start_time)`.

Payload formats (stored as JSON):

- `raw_events.data`: serialized `RawEvent` (`keyboard`, `mouse`, `window_focus_change`). For mouse wheel events, includes `wheel_amount`, `wheel_rotation`, `wheel_axis`.
- `raw_envelopes.payload`:
  - `Focus` and other structs: serialized JSON object
  - `TitleChanged`: JSON tuple `(window_id, title)`
  - `StateChanged`: JSON tuple `(from_state, to_state)` as strings
  - `Lock*`: JSON string `reason`
- `compact_events.payload`: serialized compact event struct (by kind) including summary metrics; `raw_event_ids` stores JSON array of contributing raw `event_id`s for alignment.

## Configuration

Configuration via `~/.kronical/config.toml`:

```toml
# Data storage location (default: ~/.kronical)
workspace_dir = "~/.kronical"

# Data retention period in minutes (default: 4320 = 72 hours)
retention_minutes = 4320

# State transition timeouts
active_grace_secs = 30              # Active to passive timeout
idle_threshold_secs = 300           # Passive to inactive timeout

# System metrics tracking
tracker_enabled = false             # Enable system tracking
tracker_interval_secs = 1.0         # Collection interval
tracker_batch_size = 60             # Batch size for storage
tracker_refresh_secs = 1.0          # UI refresh rate

# Performance tuning
ephemeral_max_duration_secs = 60    # Max ephemeral window duration
ephemeral_min_distinct_ids = 3      # Min distinct windows for persistence
max_windows_per_app = 30            # Max tracked windows per app
pid_cache_capacity = 1024           # Process ID cache size
title_cache_capacity = 512          # Window title cache size
focus_interner_max_strings = 4096   # String interning capacity
```

Environment variables supported with `KRONICAL_` prefix (e.g., `KRONICAL_TRACKER_ENABLED=true`).

## Compression

- Current status: compressed events are persisted to `compact_events` with alignment to source raw events.
- Engine: combines specialized compressors for scroll, mouse trajectories, keyboard, and focus.
- Outputs:
  - `Scroll`: CompactScrollSequence with `start/end`, direction, `total_amount`, `total_rotation`, `scroll_count`, position, and `raw_event_ids`.
  - `MouseTrajectory`: CompactMouseTrajectory for `Moved/Dragged` with `simplified_path` (Douglas–Peucker), `total_distance`, `max_velocity`, and `raw_event_ids`.
  - `Keyboard`: CompactKeyboardActivity aggregates bursts separated by a gap (default 30s). Stores `start/end`, `keystrokes`, `keys_per_minute`, `density_per_sec`, and `raw_event_ids`.
  - `Focus`: CompactFocusEvent with interned `app_name_id`, `window_title_id`, `pid`, optional position, single `event_id` (also stored in `raw_event_ids`).

Compare original vs compressed using DB data:

- The `raw_events` table contains full `RawEvent` JSON. You can load a time window, deserialize into `RawEvent`, and pass through the `CompressionEngine` to reconstruct compact sequences for analysis.
- Example (pseudo‑flow):
  - Query: `SELECT data FROM raw_events WHERE timestamp BETWEEN ? AND ? ORDER BY timestamp`.
  - Deserialize rows to `RawEvent` and call `CompressionEngine::compress_events(raws)`.
  - Inspect returned `(raws, compact_events)` to compare sequences.
  - Or, directly query `compact_events` and join to `raw_events` via `raw_event_ids` membership for alignment.

If you want, we can add a small `kronictl` subcommand to dump a side‑by‑side comparison for a time range.
