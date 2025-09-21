use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use crossterm::{
    event as crossterm_event, execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use hyper_util::rt::TokioIo;
use kronical as _;

use kronical::kroni_api::kroni::v1::{
    SnapshotRequest, SystemMetricsRequest, WatchRequest, kroni_client::KroniClient,
};
use kronical::util::config::AppConfig;
use log::error;
use ratatui::{
    Terminal,
    prelude::{Backend, Constraint, CrosstermBackend, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::io::{self};
use std::path::PathBuf;
use std::process::{self, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use tokio::runtime;

use tonic::transport::Endpoint;

use tower::service_fn;

fn pretty_duration(seconds: u64) -> String {
    if seconds == 0 {
        return "0s".to_string();
    }
    let days = seconds / (24 * 3600);
    let hours = (seconds % (24 * 3600)) / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    let mut result = String::new();
    if days > 0 {
        result.push_str(&format!("{}d", days));
    }
    if hours > 0 {
        result.push_str(&format!("{}h", hours));
    }
    if minutes > 0 {
        result.push_str(&format!("{}m", minutes));
    }
    if secs > 0 || result.is_empty() {
        result.push_str(&format!("{}s", secs));
    }
    result
}

fn group_label_for_display(window_id: &str, window_title: &str) -> String {
    let canonical = if let Some(rest) = window_id.strip_prefix("group:") {
        Some(rest)
    } else if let Some(rest) = window_id.strip_prefix("group-temporal:") {
        Some(rest.split(':').next().unwrap_or(rest))
    } else {
        None
    };

    let mut label = if let Some(key) = canonical {
        strip_group_prefix(window_title, key)
            .unwrap_or(window_title)
            .to_string()
    } else {
        window_title.to_string()
    };

    label = label.trim_start().to_string();
    if label.is_empty() {
        label = window_title.to_string();
    }
    label
}

fn strip_group_prefix<'a>(title: &'a str, canonical: &str) -> Option<&'a str> {
    let prefix = canonical.trim();
    if prefix.is_empty() {
        return None;
    }
    if let Some(rest) = title.strip_prefix(prefix) {
        return Some(rest.trim_start());
    }
    None
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, short, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Subcommand)]
// TODO: add journalctl like functionality.
enum Commands {
    Start,
    Stop,
    Restart,
    Status,
    Snapshot {
        #[arg(long)]
        pretty: bool,
    },
    Watch {
        #[arg(long)]
        pretty: bool,
    },
    Monitor,
    Tracker {
        #[command(subcommand)]
        action: TrackerAction,
    },
}

#[derive(Subcommand)]
enum TrackerAction {
    Show {
        /// Show last N rows from current daemon run (default: 10)
        #[arg(value_name = "count")]
        count: Option<usize>,
        #[arg(long)]
        watch: bool,
    },
    Status,
}

fn setup_logging(verbose: u8) {
    let level = match verbose {
        0 => log::LevelFilter::Warn,
        1 => log::LevelFilter::Info,
        2 => log::LevelFilter::Debug,
        _ => log::LevelFilter::Trace,
    };
    env_logger::Builder::from_default_env()
        .filter_level(level)
        .init();
}

fn ensure_workspace_dir(workspace_dir: &PathBuf) -> Result<()> {
    if !workspace_dir.exists() {
        std::fs::create_dir_all(workspace_dir).context("Failed to create workspace directory")?;
    }
    Ok(())
}

fn read_pid_file(pid_file: &PathBuf) -> Result<Option<u32>> {
    if !pid_file.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(pid_file).context("Failed to read PID file")?;
    let pid = content
        .trim()
        .parse::<u32>()
        .context("Invalid PID in file")?;
    Ok(Some(pid))
}

fn is_process_running(pid: u32) -> bool {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from(pid as usize)]),
        false,
        ProcessRefreshKind::nothing(),
    );
    system.process(Pid::from(pid as usize)).is_some()
}

fn spawn_kronid() -> Result<()> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    let cmd = if let Some(dir) = exe_dir {
        dir.join("kronid")
    } else {
        PathBuf::from("kronid")
    };
    Command::new(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn kronid")?;
    Ok(())
}

fn stop_daemon(data_file: PathBuf) -> Result<()> {
    let pid_file = kronical::util::paths::pid_file(data_file.parent().unwrap());
    if let Some(pid) = read_pid_file(&pid_file)? {
        if is_process_running(pid) {
            println!("Stopping Kronical daemon (PID: {})...", pid);
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_millis(500));
            if !is_process_running(pid) {
                println!("Kronical daemon stopped successfully");
                let _ = std::fs::remove_file(&pid_file);
            } else {
                println!(
                    "Kronical daemon did not stop gracefully, you may need to kill it manually"
                );
            }
        } else {
            println!("Kronical daemon is not running (stale PID file)");
            let _ = std::fs::remove_file(&pid_file);
        }
    } else {
        println!("Kronical daemon is not running");
    }
    Ok(())
}

fn start_daemon(data_file: PathBuf, _app_config: AppConfig) -> Result<()> {
    let pid_file = kronical::util::paths::pid_file(data_file.parent().unwrap());
    if let Some(existing_pid) = read_pid_file(&pid_file)? {
        if is_process_running(existing_pid) {
            return Err(anyhow::anyhow!(
                "Kronical daemon is already running (PID: {}). Use 'kronictl stop' first.",
                existing_pid
            ));
        } else {
            let _ = std::fs::remove_file(&pid_file);
        }
    }
    println!("Starting Kronical daemon in background...");
    spawn_kronid()?;
    Ok(())
}

fn restart_daemon(data_file: PathBuf, app_config: AppConfig) -> Result<()> {
    println!("Restarting Kronical daemon...");
    let _ = stop_daemon(data_file.clone());
    start_daemon(data_file, app_config)
}

fn get_status(data_file: PathBuf) -> Result<()> {
    let now = chrono::Utc::now();
    let local_now = now.with_timezone(&chrono::Local);
    println!(
        "Kronical Status Snapshot - {}",
        local_now.format("%Y-%m-%d %H:%M:%S %Z")
    );
    println!("═════════════════════════════════════════════════════════════\n");
    let uds_http = kronical::util::paths::http_uds(data_file.parent().unwrap());
    match http_get_snapshot(&uds_http) {
        Ok(snap) => {
            println!("Kronical Daemon:");
            println!("  State: {:?}", snap.activity_state);
            if let Some(f) = snap.focus {
                println!("  Focus: {} [{}] - {}", f.app_name, f.pid, f.window_title);
            }
            println!("  Cadence: {}ms ({})", snap.cadence_ms, snap.cadence_reason);
            println!(
                "  Counts: signals={} hints={} records={}",
                snap.counts.signals_seen, snap.counts.hints_seen, snap.counts.records_emitted
            );
            if let Some(t) = snap.next_timeout {
                println!("  Next timeout: {}", t);
            }
            println!(
                "  Storage: backlog={} last_flush={}",
                snap.storage.backlog_count,
                snap.storage
                    .last_flush_at
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default()
            );
        }
        Err(_) => {
            println!("Kronical Daemon: Not running");
        }
    }
    println!("\nTip: Use 'kronictl monitor' for real-time updates");
    Ok(())
}

fn monitor_realtime(data_file: PathBuf) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture
    )?;
    let res = run_monitor_loop(&mut terminal, data_file);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    if let Err(err) = res {
        println!("Error in monitor: {:?}", err);
    }
    Ok(())
}

fn run_monitor_loop<B: Backend>(terminal: &mut Terminal<B>, data_file: PathBuf) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    let uds_http = kronical::util::paths::http_uds(data_file.parent().unwrap());
    loop {
        match StdUnixStream::connect(&uds_http) {
            Ok(mut stream) => {
                let req = b"GET /v1/stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n";
                if let Err(_) = stream.write_all(req) {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                // Skip headers
                loop {
                    line.clear();
                    let n = reader.read_line(&mut line)?;
                    if n == 0 {
                        break;
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut data_buf = String::new();
                loop {
                    if crossterm_event::poll(Duration::from_millis(10))? {
                        if let crossterm_event::Event::Key(key) = crossterm_event::read()? {
                            if key.code == crossterm_event::KeyCode::Char('q') {
                                return Ok(());
                            }
                        }
                    }
                    line.clear();
                    let n = reader.read_line(&mut line)?;
                    if n == 0 {
                        break;
                    }
                    if line.starts_with("data:") {
                        let payload = line[5..].trim();
                        data_buf.push_str(payload);
                        data_buf.push('\n');
                    } else if line == "\r\n" || line == "\n" {
                        if !data_buf.is_empty() {
                            if let Ok(snap) = serde_json::from_str::<
                                kronical::daemon::snapshot::Snapshot,
                            >(data_buf.trim_end())
                            {
                                terminal.draw(|f| {
                                    let size = f.area();
                                    let layout = Layout::default()
                                        .direction(Direction::Vertical)
                                        .constraints([
                                            Constraint::Length(7),
                                            Constraint::Length(10),
                                            Constraint::Min(5),
                                        ])
                                        .split(size);
                                    let top = Block::default()
                                        .title("Daemon Stats")
                                        .borders(Borders::ALL);
                                    let pid_file = kronical::util::paths::pid_file(
                                        &dirs::home_dir().unwrap_or_default().join(".kronical"),
                                    );
                                    let pid = std::fs::read_to_string(pid_file)
                                        .ok()
                                        .and_then(|s| s.trim().parse::<u32>().ok())
                                        .map(|p| p.to_string())
                                        .unwrap_or_else(|| "unknown".into());
                                    let lines = vec![
                                        Line::from(vec![
                                            Span::styled(
                                                "Status: ",
                                                Style::default().fg(Color::Gray),
                                            ),
                                            Span::styled(
                                                "running",
                                                Style::default()
                                                    .fg(Color::Green)
                                                    .add_modifier(Modifier::BOLD),
                                            ),
                                        ]),
                                        Line::from(vec![
                                            Span::styled("PID: ", Style::default().fg(Color::Gray)),
                                            Span::styled(
                                                pid,
                                                Style::default()
                                                    .fg(Color::Yellow)
                                                    .add_modifier(Modifier::BOLD),
                                            ),
                                        ]),
                                        Line::from(vec![
                                            Span::styled(
                                                "State: ",
                                                Style::default().fg(Color::Gray),
                                            ),
                                            Span::styled(
                                                format!("{:?}", snap.activity_state),
                                                Style::default()
                                                    .fg(Color::Cyan)
                                                    .add_modifier(Modifier::BOLD),
                                            ),
                                        ]),
                                        Line::from(vec![
                                            Span::styled(
                                                "Cadence: ",
                                                Style::default().fg(Color::Gray),
                                            ),
                                            Span::raw(format!(
                                                "{}ms ({})",
                                                snap.cadence_ms, snap.cadence_reason
                                            )),
                                        ]),
                                        Line::from(vec![
                                            Span::styled(
                                                "Focus: ",
                                                Style::default().fg(Color::Gray),
                                            ),
                                            Span::raw(
                                                snap.focus
                                                    .as_ref()
                                                    .map(|f| {
                                                        format!(
                                                            "{} [{}] - {}",
                                                            f.app_name, f.pid, f.window_title
                                                        )
                                                    })
                                                    .unwrap_or_else(|| "-".to_string()),
                                            ),
                                        ]),
                                    ];
                                    let p = Paragraph::new(lines).block(top);
                                    f.render_widget(p, layout[0]);
                                    // Middle: Details (moved above Apps)
                                    let mut app_lines: Vec<Line> = Vec::new();
                                    if !snap.aggregated_apps.is_empty() {
                                        let mut shown = 0usize;
                                        for app in &snap.aggregated_apps {
                                            // Header: [pid] AppName • Total
                                            let header = vec![
                                                Span::styled(
                                                    format!("[{}]", app.pid),
                                                    Style::default()
                                                        .fg(Color::Green)
                                                        .add_modifier(Modifier::BOLD),
                                                ),
                                                Span::raw(" "),
                                                Span::styled(
                                                    format!("{}", app.app_name),
                                                    Style::default()
                                                        .fg(Color::Yellow)
                                                        .add_modifier(Modifier::BOLD),
                                                ),
                                                Span::raw(" • "),
                                                Span::styled(
                                                    format!("{}", app.total_duration_pretty),
                                                    Style::default().fg(Color::Cyan),
                                                ),
                                            ];
                                            app_lines.push(Line::from(header));

                                            // Lines: windows (up to 5), aligned with right info
                                            let mut count = 0usize;
                                            let total = app.windows.len();
                                            let width = layout[2].width.saturating_sub(2) as usize; // minus borders
                                            for (i, win) in app.windows.iter().take(5).enumerate() {
                                                let local_first = win
                                                    .first_seen
                                                    .with_timezone(&chrono::Local)
                                                    .format("%H:%M:%S")
                                                    .to_string();
                                                let dur_pretty =
                                                    pretty_duration(win.duration_seconds);

                                                let prefix = if i + 1 < total && i < 4 {
                                                    "  ├── "
                                                } else {
                                                    "  └── "
                                                };

                                                // Right info as rendered: "{duration} • since {time}"
                                                let right_info = format!(
                                                    "{} • since {}",
                                                    dur_pretty, local_first
                                                );
                                                let visible_right =
                                                    UnicodeWidthStr::width(right_info.as_str());

                                                // Left fixed widths: prefix + id + one space before title
                                                let id_text = format!("<#{}>", win.window_id);
                                                let left_fixed = UnicodeWidthStr::width(prefix)
                                                    + UnicodeWidthStr::width(id_text.as_str())
                                                    + 1; // single space between id and title

                                                // Base title (plus group tag if any)
                                                let base_title = if win.is_group {
                                                    let mut label = group_label_for_display(
                                                        win.window_id.as_str(),
                                                        win.window_title.as_str(),
                                                    );
                                                    label.push_str(" [group]");
                                                    label
                                                } else {
                                                    win.window_title.clone()
                                                };
                                                let mut title_display = base_title.clone();
                                                // Truncate if needed by display width, accounting for ellipsis width 1
                                                let total_needed = left_fixed
                                                    + UnicodeWidthStr::width(
                                                        title_display.as_str(),
                                                    )
                                                    + visible_right;
                                                if total_needed >= width {
                                                    let remain = width
                                                        .saturating_sub(left_fixed + visible_right);
                                                    if remain == 0 {
                                                        title_display = "…".to_string();
                                                    } else {
                                                        let mut acc = 0usize;
                                                        let mut out = String::new();
                                                        for ch in base_title.chars() {
                                                            let w = UnicodeWidthChar::width(ch)
                                                                .unwrap_or(0);
                                                            // Reserve width 1 for ellipsis when truncating
                                                            if acc + w > remain.saturating_sub(2) {
                                                                out.push('…');
                                                                break;
                                                            }
                                                            acc += w;
                                                            out.push(ch);
                                                        }
                                                        title_display = out;
                                                    }
                                                }

                                                // Compute padding spaces between left and right
                                                let visible_left_final = left_fixed
                                                    + UnicodeWidthStr::width(
                                                        title_display.as_str(),
                                                    );
                                                let pad_spaces =
                                                    if width > visible_left_final + visible_right {
                                                        width - visible_left_final - visible_right
                                                    } else {
                                                        1
                                                    };

                                                // Render parts
                                                let mut parts: Vec<Span> = Vec::new();
                                                parts.push(Span::styled(
                                                    prefix,
                                                    Style::default().fg(Color::DarkGray),
                                                ));
                                                parts.push(Span::styled(
                                                    id_text.clone(),
                                                    Style::default().fg(Color::Green),
                                                ));
                                                parts.push(Span::raw(" "));
                                                parts.push(Span::styled(
                                                    title_display,
                                                    if win.is_group {
                                                        Style::default().fg(Color::Magenta)
                                                    } else {
                                                        Style::default().fg(Color::White)
                                                    },
                                                ));
                                                parts.push(Span::raw(" ".repeat(pad_spaces)));
                                                // Right: duration (cyan) • since time (gray)
                                                parts.push(Span::styled(
                                                    dur_pretty,
                                                    Style::default().fg(Color::Cyan),
                                                ));
                                                parts.push(Span::raw(" • "));
                                                parts.push(Span::styled(
                                                    format!("since {}", local_first),
                                                    Style::default().fg(Color::Gray),
                                                ));
                                                app_lines.push(Line::from(parts));
                                                count += 1;
                                                if count >= 5 {
                                                    break;
                                                }
                                            }
                                            shown += 1;
                                            if shown >= 8 {
                                                break;
                                            }
                                        }
                                    } else {
                                        app_lines.push(Line::from("Apps: (no recent activity)"));
                                    }
                                    // Build Details content
                                    let mut details: Vec<Line> = Vec::new();
                                    details.push(Line::from(format!(
                                        "Counts: signals={} hints={} records={}",
                                        snap.counts.signals_seen,
                                        snap.counts.hints_seen,
                                        snap.counts.records_emitted
                                    )));
                                    details.push(Line::from(format!(
                                        "Storage: backlog={} last_flush={}",
                                        snap.storage.backlog_count,
                                        snap.storage
                                            .last_flush_at
                                            .map(|t| t.to_rfc3339())
                                            .unwrap_or_else(|| "".into())
                                    )));
                                    if let Some(t) = &snap.last_transition {
                                        details.push(Line::from(vec![
                                            Span::styled(
                                                "Last transition: ",
                                                Style::default().fg(Color::Gray),
                                            ),
                                            Span::styled(
                                                format!("{:?}", t.from),
                                                Style::default()
                                                    .fg(Color::Yellow)
                                                    .add_modifier(Modifier::BOLD),
                                            ),
                                            Span::raw(" → "),
                                            Span::styled(
                                                format!("{:?}", t.to),
                                                Style::default()
                                                    .fg(Color::Green)
                                                    .add_modifier(Modifier::BOLD),
                                            ),
                                            Span::raw(" at "),
                                            Span::styled(
                                                t.at.with_timezone(&chrono::Local)
                                                    .format("%H:%M:%S")
                                                    .to_string(),
                                                Style::default().fg(Color::Gray),
                                            ),
                                            if t.by_signal.is_some() {
                                                Span::raw("  ")
                                            } else {
                                                Span::raw("")
                                            },
                                            if t.by_signal.is_some() {
                                                Span::styled(
                                                    "● ",
                                                    Style::default().fg(Color::Green),
                                                )
                                            } else {
                                                Span::raw("")
                                            },
                                            if let Some(sig) = &t.by_signal {
                                                Span::styled(
                                                    sig.clone(),
                                                    Style::default().fg(Color::Green),
                                                )
                                            } else {
                                                Span::raw("")
                                            },
                                        ]));
                                    }
                                    // Recent transitions (last 5)
                                    if !snap.transitions_recent.is_empty() {
                                        details.push(Line::from("Recent transitions:"));
                                        for tr in snap.transitions_recent.iter().take(5) {
                                            let dot_color = match tr.by_signal.as_deref() {
                                                Some("KeyboardInput") => Color::Yellow,
                                                Some("MouseInput") => Color::Cyan,
                                                Some("AppChanged") => Color::Green,
                                                Some("WindowChanged") => Color::Magenta,
                                                Some("ActivityPulse") => Color::Green,
                                                Some("LockStart") => Color::Red,
                                                Some("LockEnd") => Color::Green,
                                                _ => Color::Gray,
                                            };
                                            details.push(Line::from(vec![
                                                Span::styled(
                                                    "  ● ",
                                                    Style::default().fg(dot_color),
                                                ),
                                                Span::styled(
                                                    format!("{:?}", tr.from),
                                                    Style::default().fg(Color::Yellow),
                                                ),
                                                Span::raw(" → "),
                                                Span::styled(
                                                    format!("{:?}", tr.to),
                                                    Style::default().fg(Color::Green),
                                                ),
                                                Span::raw(" at "),
                                                Span::styled(
                                                    tr.at
                                                        .with_timezone(&chrono::Local)
                                                        .format("%H:%M:%S")
                                                        .to_string(),
                                                    Style::default().fg(Color::Gray),
                                                ),
                                                if tr.by_signal.is_some() {
                                                    Span::raw("  ")
                                                } else {
                                                    Span::raw("")
                                                },
                                                if let Some(sig) = &tr.by_signal {
                                                    Span::styled(
                                                        sig.clone(),
                                                        Style::default().fg(dot_color),
                                                    )
                                                } else {
                                                    Span::raw("")
                                                },
                                            ]));
                                        }
                                    }
                                    if !snap.health.is_empty() {
                                        details.push(Line::from("Health:"));
                                        for h in &snap.health {
                                            details.push(Line::from(format!("- {}", h)));
                                        }
                                    }
                                    // Render Details in the middle panel
                                    let p2 = Paragraph::new(details)
                                        .block(
                                            Block::default().title("Details").borders(Borders::ALL),
                                        )
                                        .wrap(Wrap { trim: true });
                                    f.render_widget(p2, layout[1]);

                                    // Bottom: Aggregated apps summary (moved below Details)
                                    let p_apps = Paragraph::new(app_lines)
                                        .block(Block::default().title("Apps").borders(Borders::ALL))
                                        .wrap(Wrap { trim: true });
                                    f.render_widget(p_apps, layout[2]);
                                })?;
                            }
                            data_buf.clear();
                        }
                    }
                }
            }
            Err(_) => {
                terminal.draw(|f| {
                    let size = f.area();
                    let block = Block::default()
                        .title("Connecting...")
                        .borders(Borders::ALL);
                    let p = Paragraph::new("Could not connect to admin UDS. Waiting for daemon...")
                        .block(block);
                    f.render_widget(p, size);
                })?;
                if crossterm_event::poll(Duration::from_millis(1000))? {
                    if let crossterm_event::Event::Key(key) = crossterm_event::read()? {
                        if key.code == crossterm_event::KeyCode::Char('q') {
                            return Ok(());
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn main() {
    let cli = Cli::parse();
    setup_logging(cli.verbose);
    let config = match AppConfig::load() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            process::exit(1);
        }
    };
    if let Err(e) = ensure_workspace_dir(&config.workspace_dir) {
        error!("Failed to create workspace directory: {}", e);
        process::exit(1);
    }
    let data_file = config.workspace_dir.join("data.db");
    let result = match cli.command {
        Commands::Start => start_daemon(data_file, config.clone()),
        Commands::Stop => stop_daemon(data_file),
        Commands::Restart => restart_daemon(data_file, config.clone()),
        Commands::Status => get_status(data_file),
        Commands::Snapshot { pretty } => snapshot_autoselect(
            &kronical::util::paths::http_uds(&config.workspace_dir),
            pretty,
        ),
        Commands::Watch { pretty } => watch_via_http(
            &kronical::util::paths::http_uds(&config.workspace_dir),
            pretty,
        ),
        Commands::Monitor => monitor_realtime(data_file),
        Commands::Tracker { action } => match action {
            TrackerAction::Show { count, watch } => tracker_show(
                &config.workspace_dir,
                count,
                watch,
                config.tracker_refresh_secs,
            ),
            TrackerAction::Status => tracker_status(&config),
        },
    };
    if let Err(e) = result {
        error!("Error: {}", e);
        process::exit(1);
    }
}

fn snapshot_autoselect(uds_http: &PathBuf, pretty: bool) -> Result<()> {
    {
        if let Ok(snap) = grpc_snapshot(uds_http) {
            if pretty {
                print_snapshot_pretty(&snap);
            } else {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            }
            return Ok(());
        }
    }
    let snap = http_get_snapshot(uds_http)?;
    if pretty {
        print_snapshot_pretty(&snap);
    } else {
        println!("{}", serde_json::to_string_pretty(&snap)?);
    }
    Ok(())
}

fn watch_via_http(uds_path: &PathBuf, pretty: bool) -> Result<()> {
    // Try SSE stream first; fall back to polling if that fails.
    match sse_watch_via_grpc_then_http(uds_path, pretty) {
        Ok(()) => Ok(()),
        Err(_) => loop {
            let snap = http_get_snapshot(uds_path)?;
            if pretty {
                print_snapshot_line(&snap);
            } else {
                println!(
                    "seq={} state={:?} focus={}",
                    snap.seq,
                    snap.activity_state,
                    snap.focus
                        .as_ref()
                        .map(|f| f.window_title.as_str())
                        .unwrap_or("-")
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        },
    }
}

fn http_get_snapshot(uds_path: &PathBuf) -> Result<kronical::daemon::snapshot::Snapshot> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    let mut stream =
        StdUnixStream::connect(uds_path).with_context(|| format!("connect UDS {:?}", uds_path))?;
    let req = b"GET /v1/snapshot HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req)?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    // Split headers and body
    let resp = String::from_utf8_lossy(&buf);
    if let Some(idx) = resp.find("\r\n\r\n") {
        let body = &resp[(idx + 4)..];
        let snap: kronical::daemon::snapshot::Snapshot =
            serde_json::from_str(body).context("parse snapshot JSON")?;
        Ok(snap)
    } else {
        Err(anyhow::anyhow!("invalid HTTP response from admin UDS"))
    }
}

fn sse_watch_via_http(uds_path: &PathBuf, pretty: bool) -> Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    let mut stream =
        StdUnixStream::connect(uds_path).with_context(|| format!("connect UDS {:?}", uds_path))?;
    let req = b"GET /v1/stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n";
    stream.write_all(req)?;
    let mut reader = BufReader::new(stream);
    // Skip headers
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(anyhow::anyhow!("closed before headers"));
        }
        if line == "\r\n" {
            break;
        }
    }
    // Read SSE events
    let mut data_buf = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        if line.starts_with("data:") {
            let payload = line[5..].trim();
            data_buf.push_str(payload);
            data_buf.push('\n');
        } else if line == "\r\n" || line == "\n" {
            // event delimiter
            if !data_buf.is_empty() {
                if let Ok(snap) = serde_json::from_str::<kronical::daemon::snapshot::Snapshot>(
                    data_buf.trim_end(),
                ) {
                    if pretty {
                        print_snapshot_pretty(&snap);
                    } else {
                        println!("{}", data_buf.trim_end());
                    }
                }
                data_buf.clear();
            }
        }
    }
    Ok(())
}

fn grpc_snapshot(uds_http: &PathBuf) -> Result<kronical::daemon::snapshot::Snapshot> {
    let uds_grpc = kronical::util::paths::grpc_uds(uds_http.parent().unwrap());
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let ep = Endpoint::try_from("http://localhost")?;
        let channel = ep
            .connect_with_connector(service_fn(move |_| {
                let p = uds_grpc.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(p).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await?;
        let mut client = KroniClient::new(channel);
        let reply = client
            .snapshot(tonic::Request::new(SnapshotRequest {
                sections: vec![],
                detail: "summary".into(),
            }))
            .await?
            .into_inner();
        Ok::<_, anyhow::Error>(map_pb_snapshot(reply))
    })
}

fn map_pb_snapshot(
    reply: kronical::kroni_api::kroni::v1::SnapshotReply,
) -> kronical::daemon::snapshot::Snapshot {
    // not using Utc here anymore; the server handles time bounds
    let state = match reply.activity_state {
        1 => kronical::daemon::records::ActivityState::Active,
        2 => kronical::daemon::records::ActivityState::Passive,
        3 => kronical::daemon::records::ActivityState::Inactive,
        4 => kronical::daemon::records::ActivityState::Locked,
        _ => kronical::daemon::records::ActivityState::Inactive,
    };
    let focus = reply
        .focus
        .map(|f| kronical::daemon::events::WindowFocusInfo {
            pid: f.pid,
            process_start_time: 0,
            app_name: Arc::new(f.app),
            window_title: Arc::new(f.title),
            window_id: f.window_id.parse().unwrap_or(0),
            window_instance_start: f
                .since
                .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32))
                .unwrap_or_else(Utc::now),
            window_position: None,
            window_size: None,
        });
    let last_transition = reply
        .last_transition
        .map(|t| kronical::daemon::snapshot::Transition {
            from: match t.from {
                1 => kronical::daemon::records::ActivityState::Active,
                2 => kronical::daemon::records::ActivityState::Passive,
                3 => kronical::daemon::records::ActivityState::Inactive,
                4 => kronical::daemon::records::ActivityState::Locked,
                _ => kronical::daemon::records::ActivityState::Inactive,
            },
            to: match t.to {
                1 => kronical::daemon::records::ActivityState::Active,
                2 => kronical::daemon::records::ActivityState::Passive,
                3 => kronical::daemon::records::ActivityState::Inactive,
                4 => kronical::daemon::records::ActivityState::Locked,
                _ => kronical::daemon::records::ActivityState::Inactive,
            },
            at: t
                .at
                .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32))
                .unwrap_or_else(Utc::now),
            by_signal: None,
        });
    let counts = reply
        .counts
        .map(|c| kronical::daemon::snapshot::Counts {
            signals_seen: c.signals_seen,
            hints_seen: c.hints_seen,
            records_emitted: c.records_emitted,
        })
        .unwrap_or_default();
    let cadence_ms = reply
        .cadence
        .as_ref()
        .map(|c| c.current_ms)
        .unwrap_or_default();
    let cadence_reason = reply.cadence.map(|c| c.reason).unwrap_or_default();
    let next_timeout = reply
        .next_timeout
        .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32));
    let storage = reply
        .storage
        .map(|s| kronical::daemon::snapshot::StorageInfo {
            backlog_count: s.backlog_count,
            last_flush_at: s.last_flush.and_then(|ts| {
                chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
            }),
        })
        .unwrap_or_default();
    let config = reply
        .config
        .map(|c| kronical::daemon::snapshot::ConfigSummary {
            active_grace_secs: c.active_grace_secs,
            idle_threshold_secs: c.idle_threshold_secs,
            retention_minutes: c.retention_minutes,
            ephemeral_max_duration_secs: c.ephemeral_max_duration_secs,
            ephemeral_min_distinct_ids: c.ephemeral_min_distinct_ids as usize,
            ephemeral_app_max_duration_secs: c.ephemeral_app_max_duration_secs,
            ephemeral_app_min_distinct_procs: c.ephemeral_app_min_distinct_procs as usize,
        })
        .unwrap_or_default();
    let health = reply.health;
    fn pretty_dur(seconds: u64) -> String {
        if seconds == 0 {
            return "0s".to_string();
        }
        let days = seconds / (24 * 3600);
        let hours = (seconds % (24 * 3600)) / 3600;
        let minutes = (seconds % 3600) / 60;
        let secs = seconds % 60;
        let mut result = String::new();
        if days > 0 {
            result.push_str(&format!("{}d", days));
        }
        if hours > 0 {
            result.push_str(&format!("{}h", hours));
        }
        if minutes > 0 {
            result.push_str(&format!("{}m", minutes));
        }
        if secs > 0 || result.is_empty() {
            result.push_str(&format!("{}s", secs));
        }
        result
    }
    let aggregated_apps = reply
        .aggregated_apps
        .into_iter()
        .map(|a| {
            let windows = a
                .windows
                .into_iter()
                .map(|w| kronical::daemon::snapshot::SnapshotWindow {
                    window_id: w.window_id,
                    window_title: w.window_title,
                    first_seen: w
                        .first_seen
                        .and_then(|ts| {
                            chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
                        })
                        .unwrap_or_else(Utc::now),
                    last_seen: w
                        .last_seen
                        .and_then(|ts| {
                            chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
                        })
                        .unwrap_or_else(Utc::now),
                    duration_seconds: w.duration_seconds,
                    is_group: w.is_group,
                })
                .collect();
            kronical::daemon::snapshot::SnapshotApp {
                app_name: a.app_name,
                pid: a.pid,
                process_start_time: a.process_start_time,
                windows,
                total_duration_secs: a.total_duration_secs,
                total_duration_pretty: if a.total_duration_pretty.is_empty() {
                    pretty_dur(a.total_duration_secs)
                } else {
                    a.total_duration_pretty
                },
            }
        })
        .collect();
    kronical::daemon::snapshot::Snapshot {
        seq: reply.seq,
        mono_ns: reply.mono_ns,
        activity_state: state,
        focus,
        last_transition,
        transitions_recent: Vec::new(),
        counts,
        cadence_ms,
        cadence_reason,
        next_timeout,
        storage,
        config,
        health,
        aggregated_apps,
    }
}

fn sse_watch_via_grpc_then_http(uds_path: &PathBuf, pretty: bool) -> Result<()> {
    {
        if let Err(_e) = grpc_watch(uds_path, pretty) {
            // Fallback to HTTP SSE
            return sse_watch_via_http(uds_path, pretty);
        } else {
            return Ok(());
        }
    }
    #[allow(unreachable_code)]
    sse_watch_via_http(uds_path, pretty)
}

fn grpc_watch(_uds_http_sock: &PathBuf, pretty: bool) -> Result<()> {
    // The gRPC UDS path is derived from the workspace dir
    let uds_grpc = kronical::util::paths::grpc_uds(_uds_http_sock.parent().unwrap());
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let ep = Endpoint::try_from("http://localhost")?;
        let channel = ep
            .connect_with_connector(service_fn(move |_| {
                let p = uds_grpc.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(p).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await?;
        let mut client = KroniClient::new(channel);
        let stream = client
            .watch(tonic::Request::new(WatchRequest {
                sections: vec![],
                detail: "summary".into(),
            }))
            .await?
            .into_inner();
        use tonic::codec::Streaming;
        let mut s: Streaming<kronical::kroni_api::kroni::v1::SnapshotReply> = stream;
        while let Some(item) = s.message().await? {
            if pretty {
                let snap = map_pb_snapshot(item);
                print_snapshot_pretty(&snap);
            } else {
                let snap = map_pb_snapshot(item);
                println!("{}", serde_json::to_string(&snap).unwrap_or_default());
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn print_snapshot_pretty(s: &kronical::daemon::snapshot::Snapshot) {
    println!("Kronical Snapshot");
    println!("- seq: {}", s.seq);
    println!("- mono_ns: {}", s.mono_ns);
    println!("- state: {:?}", s.activity_state);
    if let Some(f) = &s.focus {
        println!("- focus: {} [{}] - {}", f.app_name, f.pid, f.window_title);
    } else {
        println!("- focus: -");
    }
    if let Some(t) = &s.last_transition {
        println!("- last_transition: {:?} -> {:?} at {}", t.from, t.to, t.at);
    }
    println!("- cadence: {}ms ({})", s.cadence_ms, s.cadence_reason);
    if let Some(nt) = &s.next_timeout {
        println!("- next_timeout: {}", nt);
    }
    println!(
        "- counts: signals={} hints={} records={}",
        s.counts.signals_seen, s.counts.hints_seen, s.counts.records_emitted
    );
    println!(
        "- storage: backlog={} last_flush={}",
        s.storage.backlog_count,
        s.storage
            .last_flush_at
            .map(|t| t.to_rfc3339())
            .unwrap_or("".into())
    );
    println!(
        "- config: active={}s idle={}s retention={}m eph_max={}s eph_min={} eph_app_max={}s eph_app_min_procs={}",
        s.config.active_grace_secs,
        s.config.idle_threshold_secs,
        s.config.retention_minutes,
        s.config.ephemeral_max_duration_secs,
        s.config.ephemeral_min_distinct_ids,
        s.config.ephemeral_app_max_duration_secs,
        s.config.ephemeral_app_min_distinct_procs
    );
    if !s.health.is_empty() {
        println!("- health: {}", s.health.join(", "));
    }
    println!("- apps ({}):", s.aggregated_apps.len());
    for app in &s.aggregated_apps {
        println!(
            "  - app: {} [{}] total={} ({}s)",
            app.app_name, app.pid, app.total_duration_pretty, app.total_duration_secs
        );
        if !app.windows.is_empty() {
            println!("    windows ({}):", app.windows.len());
            for w in &app.windows {
                println!("      - title: {}", w.window_title);
                println!(
                    "        id: {} first_seen: {} last_seen: {} dur: {}s{}",
                    w.window_id,
                    w.first_seen,
                    w.last_seen,
                    w.duration_seconds,
                    if w.is_group { " (group)" } else { "" }
                );
            }
        } else {
            println!("    windows: -");
        }
    }
}

fn print_snapshot_line(s: &kronical::daemon::snapshot::Snapshot) {
    let focus = s
        .focus
        .as_ref()
        .map(|f| f.window_title.as_str())
        .unwrap_or("-");
    println!(
        "seq={} state={:?} focus={} cad={}ms backlog={}",
        s.seq, s.activity_state, focus, s.cadence_ms, s.storage.backlog_count
    );
}

fn tracker_status(config: &AppConfig) -> Result<()> {
    println!("System Tracker Status");
    println!("════════════════════");

    if !config.tracker_enabled {
        println!("Status: DISABLED");
        println!("To enable: Set tracker_enabled = true in config.toml and restart daemon");
        return Ok(());
    }

    let daemon_pid_file = kronical::util::paths::pid_file(&config.workspace_dir);
    if let Ok(Some(pid)) = read_pid_file(&daemon_pid_file) {
        if is_process_running(pid) {
            println!("Status: ENABLED and running with daemon (PID: {})", pid);
            println!("Interval: {} seconds", config.tracker_interval_secs);
            println!("Batch size: {}", config.tracker_batch_size);

            let db_path = kronical::util::paths::tracker_db_with_backend(
                &config.workspace_dir,
                &config.tracker_db_backend,
            );
            if db_path.exists() {
                let metadata = std::fs::metadata(&db_path)?;
                println!(
                    "Data file: {} ({} bytes)",
                    db_path.display(),
                    metadata.len()
                );
                let modified = metadata.modified()?;
                println!("Last modified: {:?}", modified);
            } else {
                println!("Data file: Not yet created");
            }
        } else {
            println!("Status: ENABLED but daemon not running");
        }
    } else {
        println!("Status: ENABLED but daemon not running (no PID file)");
    }

    Ok(())
}

fn cleanup_stale_tracker_pid(workspace_dir: &PathBuf) {
    let tracker_pid_file = workspace_dir.join("tracker.pid");
    if tracker_pid_file.exists() {
        println!("Removing stale tracker.pid file (tracker is now part of daemon)");
        let _ = std::fs::remove_file(&tracker_pid_file);
    }
}

fn tracker_show(
    workspace_dir: &PathBuf,
    count: Option<usize>,
    watch: bool,
    refresh_interval_secs: f64,
) -> Result<()> {
    cleanup_stale_tracker_pid(workspace_dir);

    let show_data = || -> Result<()> {
        let daemon_pid_file = kronical::util::paths::pid_file(workspace_dir);
        let pid = if let Ok(Some(pid)) = read_pid_file(&daemon_pid_file) {
            if !is_process_running(pid) {
                return Err(anyhow::anyhow!(
                    "Daemon not running. Start the daemon first with 'kronictl start'"
                ));
            }
            pid
        } else {
            return Err(anyhow::anyhow!(
                "Daemon not running. Start the daemon first with 'kronictl start'"
            ));
        };

        show_tracker_data_grpc(workspace_dir, count, pid)
    };

    if watch {
        println!("Watching tracker data (press Ctrl+C to exit)...");
        let refresh_duration = Duration::from_secs_f64(refresh_interval_secs);

        loop {
            show_data()?;
            std::thread::sleep(refresh_duration);
        }
    } else {
        show_data()?;
    }

    Ok(())
}

fn show_tracker_data_grpc(
    workspace_dir: &PathBuf,
    count: Option<usize>,
    daemon_pid: u32,
) -> Result<()> {
    use tonic::transport::Endpoint;
    use tower::service_fn;

    // Server is responsible for flush-before-query to avoid client/server
    // workspace drift and snapshot ordering issues.

    let uds_grpc = kronical::util::paths::grpc_uds(workspace_dir);
    let rt = runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let uds_grpc_dbg = uds_grpc.clone();
        let ep = Endpoint::try_from("http://localhost")?;
        let channel = ep
            .connect_with_connector(service_fn(move |_| {
                let p = uds_grpc.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(p).await?;
                    Ok::<_, std::io::Error>(TokioIo::new(stream))
                }
            }))
            .await?;

        let mut client = KroniClient::new(channel);

        // Default to the last 10 rows from the current daemon run (by PID).
        let limit_rows: usize = count.unwrap_or(10);
        let pid_to_query = daemon_pid;

        // Ask the server for the latest N rows for this PID (no time range).
        let request = SystemMetricsRequest {
            pid: pid_to_query,
            start_time: None,
            end_time: None,
            limit: limit_rows as u32,
        };

        println!(
            "Debug: UDS={} PID={} limit={} (server flush-before-query)",
            uds_grpc_dbg.display(),
            pid_to_query,
            limit_rows
        );

        let response = client
            .get_system_metrics(tonic::Request::new(request))
            .await?
            .into_inner();

        if response.metrics.is_empty() {
            println!("No tracker data found for current daemon run");
            return Ok(());
        }

        println!(
            "{:<32} {:>8} {:>12} {:>12}",
            "Timestamp (RFC3339)", "CPU %", "Memory KB", "Disk IO KB"
        );
        println!("{}", "-".repeat(68));

        for metric in &response.metrics {
            let time_part = match &metric.timestamp {
                Some(ts) => {
                    let secs = ts.seconds;
                    let nanos = ts.nanos;
                    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, nanos as u32)
                        .ok_or_else(|| anyhow::anyhow!("Invalid timestamp range"))?;
                    use chrono::SecondsFormat;
                    // Fixed-width RFC3339 with micros to keep columns aligned (32 chars in UTC)
                    dt.to_rfc3339_opts(SecondsFormat::Micros, true)
                }
                None => "unknown".to_string(),
            };
            let memory_kb = metric.memory_bytes / 1024;
            let disk_io_kb = metric.disk_io_bytes / 1024;

            println!(
                "{:<32} {:>8.1} {:>12} {:>12}",
                time_part, metric.cpu_percent, memory_kb, disk_io_kb
            );
        }

        if let (Some(first), Some(last)) = (response.metrics.first(), response.metrics.last()) {
            let first_time = first
                .timestamp
                .as_ref()
                .and_then(|ts| {
                    use chrono::SecondsFormat;
                    chrono::DateTime::<chrono::Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
                        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Micros, true))
                })
                .unwrap_or_else(|| "unknown".to_string());
            let last_time = last
                .timestamp
                .as_ref()
                .and_then(|ts| {
                    use chrono::SecondsFormat;
                    chrono::DateTime::<chrono::Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
                        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Micros, true))
                })
                .unwrap_or_else(|| "unknown".to_string());

            println!();
            println!(
                "Showing {} entries from {} to {}",
                response.metrics.len(),
                first_time,
                last_time
            );
            // Help users diagnose mismatches: show the PID we queried
            println!("Source: PID={}", pid_to_query);

            // Diagnostic: show recency delta
            if let Some(ts) = last.timestamp.as_ref() {
                if let Some(last_dt) =
                    chrono::DateTime::<chrono::Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
                {
                    let now = chrono::Utc::now();
                    let delta = now.signed_duration_since(last_dt).num_seconds();
                    println!("Debug: last_ts_age={}s (now={})", delta, now.to_rfc3339());
                }
            }
        }

        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

// gRPC API always available; feature gate removed
