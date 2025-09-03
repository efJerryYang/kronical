# Troubleshooting

## Daemon Crashes

### kronid Unexpectedly Stops

**Symptoms:**
- kronid daemon stops running with low memory usage
- Error messages like:
  - `Error serving connection: hyper::Error(IncompleteMessage)`
  - `thread '<unnamed>' panicked at ~/.cargo/registry/src/index.crates.io-*/core-foundation-*/src/dictionary.rs`

**Cause:**
Implementation issues with winshift library's Core Foundation integration on macOS.

**Solution:**
Restart the daemon:

```bash
kronictl restart
```

Or manually:

```bash
kronictl stop
kronictl start
```

**Prevention:**
Check daemon status periodically:

```bash
kronictl status
```