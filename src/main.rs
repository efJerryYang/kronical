mod aggregation_manager;
mod compression;
mod config;
mod coordinator;
mod events;
mod focus_tracker;
mod monitor_ui;
mod records;
mod simple_sqlite_storage;
mod socket_server;
mod storage;
mod storage_backend;

use crate::config::AppConfig;
use crate::simple_sqlite_storage::SimpleSqliteStorage;
use crate::storage_backend::StorageBackend;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use coordinator::EventCoordinator;
use crossterm::{
    event::{self as crossterm_event, DisableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use log::{debug, error, info, trace, warn};
use ratatui::prelude::Backend;
use ratatui::prelude::CrosstermBackend;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use std::io::Read;
use std::io::{self, Write};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

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
    Monitor,
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

fn start_daemon(data_file: PathBuf) -> Result<()> {
    let pid_file = data_file.parent().unwrap().join("chronicle.pid");

    if let Some(existing_pid) = read_pid_file(&pid_file)? {
        if is_process_running(existing_pid) {
            return Err(anyhow::anyhow!(
                "Chronicle daemon is already running (PID: {}). Use 'chronicle stop' first.",
                existing_pid
            ));
        } else {
            let _ = std::fs::remove_file(&pid_file);
        }
    }

    write_pid_file(&pid_file)?;

    info!("Starting Chronicle activity tracking daemon");
    info!("Data file: {:?}", data_file);
    info!("PID file: {:?}", pid_file);

    let data_store: Box<dyn StorageBackend> = Box::new(
        SimpleSqliteStorage::new(&data_file).context("Failed to initialize SQLite data store")?,
    );

    let coordinator = EventCoordinator::new();

    info!("Chronicle will run on MAIN THREAD (required by macOS hooks)");
    let result = coordinator.start_main_thread(data_store, pid_file.clone());

    let _ = std::fs::remove_file(&pid_file);

    info!("Chronicle daemon stopped gracefully");
    result
}

fn get_status(data_file: PathBuf) -> Result<()> {
    let now = chrono::Utc::now();
    let local_now = now.with_timezone(&chrono::Local);

    println!(
        "Chronicle Status Snapshot - {}",
        local_now.format("%Y-%m-%d %H:%M:%S %Z")
    );
    println!("═════════════════════════════════════════════════════════════\n");

    match query_daemon_via_socket(&data_file) {
        Ok(response) => {
            println!("Chronicle Daemon:");
            println!("  Status: {}", response.daemon_status);

            let pid_file = data_file.parent().unwrap().join("chronicle.pid");
            if let Ok(Some(pid)) = read_pid_file(&pid_file) {
                println!("  PID: {}", pid);
            }

            println!("\nStorage:");
            println!("  Data file: {:.4} MB", response.data_file_size_mb);

            if !response.recent_apps.is_empty() {
                println!(
                    "\nRecent Activity (Top {} Apps):",
                    response.recent_apps.len()
                );
                for app in response.recent_apps {
                    println!(
                        "  - {} (PID: {}) - {} total",
                        app.app_name, app.pid, app.total_duration_pretty
                    );
                    for window in app.windows {
                        if !window.window_title.is_empty() {
                            println!("    └─ {} ({})", window.window_title, window.last_active);
                        }
                    }
                }
            } else {
                println!("\nRecent Activity: No data available");
            }
        }
        Err(_) => {
            println!("Chronicle Daemon: Not running");
        }
    }

    println!("\nTip: Use 'chronicle monitor' for real-time updates");
    Ok(())
}

fn stop_daemon(data_file: PathBuf) -> Result<()> {
    let pid_file = data_file.parent().unwrap().join("chronicle.pid");

    if let Some(pid) = read_pid_file(&pid_file)? {
        if is_process_running(pid) {
            println!("Stopping Chronicle daemon (PID: {})...", pid);

            #[cfg(unix)]
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }

            thread::sleep(Duration::from_millis(500));

            if !is_process_running(pid) {
                println!("Chronicle daemon stopped successfully");
                let _ = std::fs::remove_file(&pid_file);
            } else {
                println!(
                    "Chronicle daemon did not stop gracefully, you may need to kill it manually"
                );
            }
        } else {
            println!("Chronicle daemon is not running (stale PID file)");
            let _ = std::fs::remove_file(&pid_file);
        }
    } else {
        println!("Chronicle daemon is not running");
    }

    Ok(())
}

fn restart_daemon(data_file: PathBuf) -> Result<()> {
    println!("Restarting Chronicle daemon...");

    // Note: stop_daemon has handled the waiting, we don't need to wait again
    // and we should not encounter any Err in such case, if any, unexpected
    // behavior we'd better yield.
    if let Err(e) = stop_daemon(data_file.clone()) {
        panic!("Unexpected error when stopping daemon: {}", e);
    }

    start_daemon(data_file)
}

fn monitor_realtime(data_file: PathBuf) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // It's important to disable mouse capture to allow text selection in the terminal.
    execute!(terminal.backend_mut(), DisableMouseCapture)?;

    let res = run_monitor_loop(&mut terminal, data_file);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("Error in monitor: {:?}", err);
    }

    Ok(())
}

fn run_monitor_loop<B: Backend>(terminal: &mut Terminal<B>, data_file: PathBuf) -> io::Result<()> {
    let notification_socket_path = data_file.parent().unwrap().join("chronicle.notify.sock");

    loop {
        let mut notify_stream;

        // Connection loop
        loop {
            match UnixStream::connect(&notification_socket_path) {
                Ok(stream) => {
                    notify_stream = stream;
                    notify_stream.set_nonblocking(true)?;
                    break;
                }
                Err(_) => {
                    terminal.draw(|f| {
                        let size = f.area();
                        let block = Block::default()
                            .title("Connecting...")
                            .borders(Borders::ALL);
                        let p = Paragraph::new(
                            "Could not connect to Chronicle daemon. Waiting for it to start...",
                        )
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
                }
            }
        }

        // Monitoring loop
        let mut notify_buf = [0u8; 1];
        'monitor: loop {
            match query_daemon_via_socket(&data_file) {
                Ok(response) => {
                    terminal.draw(|f| monitor_ui::ui(f, &response))?;
                }
                Err(_) => {
                    // Failed to query, daemon might have stopped. Break to reconnect.
                    break 'monitor;
                }
            }

            // Event handling loop
            loop {
                if crossterm_event::poll(Duration::from_millis(100))? {
                    if let crossterm_event::Event::Key(key) = crossterm_event::read()? {
                        if key.code == crossterm_event::KeyCode::Char('q') {
                            return Ok(());
                        }
                    }
                }

                match notify_stream.read(&mut notify_buf) {
                    Ok(0) => {
                        // 0 bytes read means EOF, socket closed
                        break 'monitor;
                    }
                    Ok(_) => {
                        // Notification received, break to redraw
                        break;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // No notification, continue waiting
                    }
                    Err(_) => {
                        // Any other error, assume disconnection
                        break 'monitor;
                    }
                }
            }
        }
    }
}

fn query_daemon_via_socket(data_file: &PathBuf) -> Result<socket_server::MonitorResponse> {
    let socket_path = data_file.parent().unwrap().join("chronicle.sock");

    let mut stream = UnixStream::connect(socket_path)?;
    writeln!(stream, "status")?;

    let mut reader = BufReader::new(&stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;

    let parsed: socket_server::MonitorResponse = serde_json::from_str(&response)?;
    Ok(parsed)
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

fn write_pid_file(pid_file: &PathBuf) -> Result<()> {
    let pid = std::process::id();
    std::fs::write(pid_file, pid.to_string()).context("Failed to write PID file")?;
    Ok(())
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

fn main() {
    let cli = Cli::parse();
    setup_logging(cli.verbose);

    // Load configuration
    let config = match AppConfig::load() {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            process::exit(1);
        }
    };

    // Ensure workspace directory exists
    if let Err(e) = ensure_workspace_dir(&config.workspace_dir) {
        error!("Failed to create workspace directory: {}", e);
        process::exit(1);
    }

    let data_file = config.workspace_dir.join("data.db");

    let result = match cli.command {
        Commands::Start => start_daemon(data_file),
        Commands::Stop => stop_daemon(data_file),
        Commands::Restart => restart_daemon(data_file),
        Commands::Status => get_status(data_file),
        Commands::Monitor => monitor_realtime(data_file),
    };

    if let Err(e) = result {
        error!("Error: {}", e);
        process::exit(1);
    }
}
