Chronicle
Activity tracking daemon with input and window monitoring.

Permissions on macOS
- Accessibility: required for global input hooks (keyboard/mouse) and window focus tracking. Without this permission, Chronicle cannot start and will abort at launch.
  - System Settings > Privacy & Security > Accessibility
  - Add and enable your terminal (or the built Chronicle binary)
- Screen Recording: required for reading window titles on macOS. Without this permission, the app and PID are detected, but window titles are empty. This is an OS restriction.
  - System Settings > Privacy & Security > Screen Recording
  - Enable for your terminal (or the built Chronicle binary)
  - See active-win-pos-rs notes: its README explains why titles are empty without Screen Recording and how to enable it.

Linux/Windows
- No special permissions typically required. Behavior depends on the platform’s input/window APIs.

Quick run
- Build and start: `cargo run -- start`
- Monitor UI: `cargo run -- monitor` (press `q` to quit)
- Status snapshot: `cargo run -- status`

Project layout

```
src/
  daemon/
    coordinator.rs
    compression.rs
    events.rs
    focus_tracker.rs
    records.rs
    socket_server.rs
  storage.rs           # StorageBackend trait
  storage/
    sqlite3.rs         # SqliteStorage implementation
  monitor/
    ui.rs              # TUI frontend
  util.rs              # util root
  util/
    config.rs
    maps.rs
```

Storage: SQLite via `SqliteStorage`.

Troubleshooting
- Fails to start or exits immediately on macOS: missing Accessibility permission.
- Status/Monitor shows apps but empty window titles on macOS: grant Screen Recording.
- After changing permissions, fully quit your terminal and relaunch (macOS caches permissions per binary).
