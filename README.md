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
kronical start                    # Start kronid daemon
kronical status                   # Check daemon status
kronical stop                     # Stop daemon

# Data export
kronical export --output data.json
kronical export --format csv

# Monitoring
kronical monitor                  # Live TUI (press 'q' to quit)
kronical debug                    # Debug information

# System tracking
kronical system start --output metrics.csv
kronical system plot --input metrics.csv
```

## Permissions (macOS)
- **Accessibility**: Required for input hooks and window tracking
- **Screen Recording**: Required for window titles

Without these, kronid will abort at launch or show empty titles.

## Storage
SQLite backend with separate indexing for events and records streams.
