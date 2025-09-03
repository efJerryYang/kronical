use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event as crossterm_event, execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
#[cfg(feature = "kroni-api")]
use hyper_util::rt::TokioIo;
use kronical as _; // ensure library is linked
use kronical::daemon::system_tracker::trigger_tracker_flush;
#[cfg(feature = "kroni-api")]
use kronical::kroni_api::kroni::v1::kroni_client::KroniClient;
#[cfg(feature = "kroni-api")]
use kronical::kroni_api::kroni::v1::{SnapshotRequest, WatchRequest};
use kronical::storage::StorageBackend;
use kronical::storage::sqlite3::SqliteStorage;
use kronical::util::config::AppConfig;
use log::{debug, error};
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
#[cfg(feature = "kroni-api")]
use tokio::runtime;
#[cfg(feature = "kroni-api")]
use tonic::transport::Endpoint;
#[cfg(feature = "kroni-api")]
use tower::service_fn;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, short, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Subcommand)]
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
    Replay {
        #[arg(long, default_value_t = 60.0)]
        speed: f64,
        #[arg(long)]
        minutes: Option<u64>,
    },
    ReplayMonitor,
    Tracker {
        #[command(subcommand)]
        action: TrackerAction,
    },
}

#[derive(Subcommand)]
enum TrackerAction {
    Show {
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
    let pid_file = data_file.parent().unwrap().join("kronid.pid");
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
    let pid_file = data_file.parent().unwrap().join("kronid.pid");
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
    let uds_http = data_file.parent().unwrap().join("kroni.http.sock");
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
    let res = run_monitor_loop(&mut terminal, data_file, false);
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

fn monitor_replay(data_file: PathBuf) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture
    )?;
    let res = run_monitor_loop(&mut terminal, data_file, true);
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

fn run_monitor_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    data_file: PathBuf,
    _replay: bool,
) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    let uds_http = data_file.parent().unwrap().join("kroni.http.sock");
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
                                    let pid_file = dirs::home_dir()
                                        .unwrap_or_default()
                                        .join(".kronical")
                                        .join("kronid.pid");
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
                                    // Middle: Aggregated apps summary
                                    let mut app_lines: Vec<Line> = Vec::new();
                                    if !snap.aggregated_apps.is_empty() {
                                        app_lines.push(Line::from("Apps (latest first):"));
                                        let mut shown = 0usize;
                                        for app in &snap.aggregated_apps {
                                            let label = format!(
                                                "{} [{}] • {}",
                                                app.app_name, app.pid, app.total_duration_pretty
                                            );
                                            app_lines.push(Line::from(label));
                                            // Show last-seen window for this app if available
                                            if let Some(win) = app.windows.first() {
                                                let wt = format!(
                                                    "  └─ {} • {}s{}",
                                                    win.window_title,
                                                    win.duration_seconds,
                                                    if win.is_group { " (group)" } else { "" }
                                                );
                                                app_lines.push(Line::from(wt));
                                            }
                                            shown += 1;
                                            if shown >= 5 {
                                                break;
                                            }
                                        }
                                    } else {
                                        app_lines.push(Line::from("Apps: (no recent activity)"));
                                    }
                                    let p_apps = Paragraph::new(app_lines)
                                        .block(Block::default().title("Apps").borders(Borders::ALL))
                                        .wrap(Wrap { trim: true });
                                    f.render_widget(p_apps, layout[1]);

                                    // Bottom: Details
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
                                        details.push(Line::from(format!(
                                            "Last transition: {:?}->{:?} at {}",
                                            t.from, t.to, t.at
                                        )));
                                    }
                                    if !snap.health.is_empty() {
                                        details.push(Line::from("Health:"));
                                        for h in &snap.health {
                                            details.push(Line::from(format!("- {}", h)));
                                        }
                                    }
                                    let p2 = Paragraph::new(details)
                                        .block(
                                            Block::default().title("Details").borders(Borders::ALL),
                                        )
                                        .wrap(Wrap { trim: true });
                                    f.render_widget(p2, layout[2]);
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

// legacy monitor socket removed

fn start_replay_daemon(
    data_file: PathBuf,
    app_config: AppConfig,
    speed: f64,
    minutes: u64,
) -> Result<()> {
    let workspace_dir = data_file.parent().unwrap().to_path_buf();
    let data_store: Box<dyn StorageBackend> =
        Box::new(SqliteStorage::new(&data_file).context("Failed to initialize SQLite data store")?);
    kronical::daemon::replay::run_replay(
        data_store,
        workspace_dir,
        speed,
        minutes,
        (app_config.active_grace_secs, app_config.idle_threshold_secs),
    )
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
        Commands::Snapshot { pretty } => {
            snapshot_autoselect(&config.workspace_dir.join("kroni.http.sock"), pretty)
        }
        Commands::Watch { pretty } => {
            watch_via_http(&config.workspace_dir.join("kroni.http.sock"), pretty)
        }
        Commands::Monitor => monitor_realtime(data_file),
        Commands::Replay { speed, minutes } => {
            let minutes = minutes.unwrap_or(config.retention_minutes);
            start_replay_daemon(data_file, config.clone(), speed, minutes)
        }
        Commands::ReplayMonitor => monitor_replay(data_file),
        Commands::Tracker { action } => match action {
            TrackerAction::Show { watch } => {
                tracker_show(&config.workspace_dir, watch, config.tracker_refresh_secs)
            }
            TrackerAction::Status => tracker_status(&config),
        },
    };
    if let Err(e) = result {
        error!("Error: {}", e);
        process::exit(1);
    }
}

fn snapshot_autoselect(uds_http: &PathBuf, pretty: bool) -> Result<()> {
    #[cfg(feature = "kroni-api")]
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
                        print_snapshot_line(&snap);
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

#[cfg(feature = "kroni-api")]
fn grpc_snapshot(uds_http: &PathBuf) -> Result<kronical::daemon::snapshot::Snapshot> {
    use chrono::{DateTime, Utc};
    let uds_grpc = uds_http.with_file_name("kroni.sock");
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
        // Map reply into our Snapshot for pretty printing/unified handling
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
                window_instance_start: DateTime::parse_from_rfc3339(&f.since_rfc3339)
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(Utc::now),
                window_position: None,
                window_size: None,
            });
        let last_transition =
            reply
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
                    at: DateTime::parse_from_rfc3339(&t.at_rfc3339)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                        .unwrap_or_else(Utc::now),
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
        let next_timeout = if reply.next_timeout_rfc3339.is_empty() {
            None
        } else {
            DateTime::parse_from_rfc3339(&reply.next_timeout_rfc3339)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        };
        let storage = reply
            .storage
            .map(|s| kronical::daemon::snapshot::StorageInfo {
                backlog_count: s.backlog_count,
                last_flush_at: if s.last_flush_rfc3339.is_empty() {
                    None
                } else {
                    DateTime::parse_from_rfc3339(&s.last_flush_rfc3339)
                        .ok()
                        .map(|dt| dt.with_timezone(&Utc))
                },
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
        let replay = reply
            .replay
            .map(|r| kronical::daemon::snapshot::ReplayInfo {
                mode: r.mode,
                position: if r.position == 0 {
                    None
                } else {
                    Some(r.position)
                },
            })
            .unwrap_or_default();
        let health = reply.health;
        // Map aggregated apps if present
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
                        first_seen: DateTime::parse_from_rfc3339(&w.first_seen_rfc3339)
                            .ok()
                            .map(|dt| dt.with_timezone(&Utc))
                            .unwrap_or_else(Utc::now),
                        last_seen: DateTime::parse_from_rfc3339(&w.last_seen_rfc3339)
                            .ok()
                            .map(|dt| dt.with_timezone(&Utc))
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
        let snap = kronical::daemon::snapshot::Snapshot {
            seq: reply.seq,
            mono_ns: reply.mono_ns,
            activity_state: state,
            focus,
            last_transition,
            counts,
            cadence_ms,
            cadence_reason,
            next_timeout,
            storage,
            config,
            replay,
            health,
            aggregated_apps,
        };
        Ok::<_, anyhow::Error>(snap)
    })
}

fn sse_watch_via_grpc_then_http(uds_path: &PathBuf, pretty: bool) -> Result<()> {
    #[cfg(feature = "kroni-api")]
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

#[cfg(feature = "kroni-api")]
fn grpc_watch(_uds_http_sock: &PathBuf, pretty: bool) -> Result<()> {
    // The gRPC UDS is at kroni.sock next to http.sock
    let uds_grpc = _uds_http_sock.with_file_name("kroni.sock");
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
                println!(
                    "seq={} state={:?} cadence={}ms",
                    item.seq,
                    item.activity_state,
                    item.cadence.as_ref().map(|c| c.current_ms).unwrap_or(0)
                );
            } else {
                println!("{}", serde_json::to_string(&item).unwrap_or_default());
            }
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn print_snapshot_pretty(s: &kronical::daemon::snapshot::Snapshot) {
    println!("Kronical Snapshot");
    println!("- seq: {}", s.seq);
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
        "- config: active={}s idle={}s retention={}m eph_max={}s eph_min={}",
        s.config.active_grace_secs,
        s.config.idle_threshold_secs,
        s.config.retention_minutes,
        s.config.ephemeral_max_duration_secs,
        s.config.ephemeral_min_distinct_ids
    );
    println!("- mode: {}", s.replay.mode);
    if !s.health.is_empty() {
        println!("- health: {}", s.health.join(", "));
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

    let daemon_pid_file = config.workspace_dir.join("kronid.pid");
    if let Ok(Some(pid)) = read_pid_file(&daemon_pid_file) {
        if is_process_running(pid) {
            println!("Status: ENABLED and running with daemon (PID: {})", pid);
            println!("Interval: {} seconds", config.tracker_interval_secs);
            println!("Batch size: {}", config.tracker_batch_size);

            let zip_path = config.workspace_dir.join("system-tracker.zip");
            if zip_path.exists() {
                let metadata = std::fs::metadata(&zip_path)?;
                println!(
                    "Data file: {} ({} bytes)",
                    zip_path.display(),
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

fn tracker_show(workspace_dir: &PathBuf, watch: bool, refresh_interval_secs: f64) -> Result<()> {
    let zip_path = workspace_dir.join("system-tracker.zip");

    cleanup_stale_tracker_pid(workspace_dir);

    let trigger_flush_and_show = || -> Result<()> {
        if let Err(_) = trigger_tracker_flush(workspace_dir) {
            debug!("Could not trigger tracker flush (daemon may not be running)");
        }

        std::thread::sleep(Duration::from_millis(100));

        if !zip_path.exists() {
            return Err(anyhow::anyhow!(
                "No tracker data found at {}. Make sure tracker_enabled = true in config and daemon is running.",
                zip_path.display()
            ));
        }

        show_tracker_data(&zip_path)
    };

    if watch {
        println!("Watching tracker data (press Ctrl+C to exit)...");
        let refresh_duration = Duration::from_secs_f64(refresh_interval_secs);

        loop {
            trigger_flush_and_show()?;
            std::thread::sleep(refresh_duration);
        }
    } else {
        trigger_flush_and_show()?;
    }

    Ok(())
}

fn show_tracker_data(zip_path: &PathBuf) -> Result<()> {
    use std::fs::File;
    use std::io::Read;
    use zip::ZipArchive;

    let file = File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)?;

    let mut csv_file = archive.by_name("system-metrics.csv")?;
    let mut contents = String::new();
    csv_file.read_to_string(&mut contents)?;

    let mut reader = csv::Reader::from_reader(contents.as_bytes());

    println!(
        "{:<20} {:>8} {:>12} {:>12}",
        "Timestamp", "CPU %", "Memory KB", "Disk IO KB"
    );
    println!("{}", "-".repeat(64));

    for result in reader.records() {
        let record = result?;
        if record.len() >= 4 {
            let timestamp = &record[0];
            let cpu = record[1].parse::<f64>().unwrap_or(0.0);
            let memory_kb = record[2].parse::<u64>().unwrap_or(0) / 1024;
            let disk_io_kb = record[3].parse::<u64>().unwrap_or(0) / 1024;

            let time_part = timestamp
                .split('T')
                .nth(1)
                .unwrap_or(timestamp)
                .split('.')
                .next()
                .unwrap_or("");
            println!(
                "{:<20} {:>8.1} {:>12.1} {:>12.1}",
                time_part, cpu, memory_kb as f64, disk_io_kb as f64
            );
        }
    }

    Ok(())
}
