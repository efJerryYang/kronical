Kronical
Activity tracking daemon (kronid) with dual-stream pattern and state transitions.

## State Transition Logic
- **Active**: Keyboard input detected (30s timeout → passive)
- **Passive**: Mouse movement only (300s timeout → inactive)  
- **Inactive**: No input activity
- **Locked**: Screen/session locked (system event)

## Dual Streams Pattern
- **Events Stream**: Raw input/focus events with complete metadata
- **Records Stream**: Analyzed activity records with state transitions

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
SQLite backend with separate indexing for events and records streams.

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
