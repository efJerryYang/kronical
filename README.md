Kronical
Activity tracking daemon (kronid) with dual-stream pattern and state transitions.

## State Transition Logic
- **Active**: Keyboard input detected (30s timeout → passive)
- **Passive**: Mouse movement only (60s timeout → inactive)  
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

# Replay functionality
kronictl replay                   # Replay with default speed
kronictl replay --speed 2.0       # Replay at 2x speed
kronictl replay-monitor           # Monitor replay mode

# System tracking (when enabled in config)
kronictl tracker status           # Show tracker status
kronictl tracker show             # Show tracker data
kronictl tracker show --follow    # Follow tracker updates
```

## Permissions (macOS)
- **Accessibility**: Required for input hooks and window tracking
- **Screen Recording**: Required for window titles

Without these, kronid will abort at launch or show empty titles.

## Storage
SQLite backend with separate indexing for events and records streams.
