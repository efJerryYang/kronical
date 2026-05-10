use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Timelike, Utc};
use clap::{Parser, Subcommand};
use crossterm::{
    event as crossterm_event, execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use hyper_util::rt::TokioIo;
use kronical as _;

use kronical::kroni_api::kroni::v1::{SnapshotRequest, WatchRequest, kroni_client::KroniClient};
use kronical::util::config::AppConfig;
use kronical::util::logging::error;
use kronical_core::records::{ActivityRecord, AggregatedActivity, aggregate_activities_since};
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

#[path = "kronictl/log_cli.rs"]
mod log_cli;

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

const LOGINWINDOW_APP: &str = "loginwindow";
const MONITOR_SLEEP_MIN_SECS: i64 = 4 * 60 * 60;
const MONITOR_SLEEP_MERGE_GAP_SECS: i64 = 60 * 60;
const MONITOR_SLEEP_WINDOW_START_HOUR: u32 = 21;
const MONITOR_SLEEP_TARGET_HOUR: u32 = 4;
const MONITOR_SLEEP_NIGHT_START_HOUR: u32 = 22;
const MONITOR_SLEEP_NIGHT_END_HOUR: u32 = 10;
const MONITOR_SLEEP_MIDPOINT_MAX_DIFF_SECS: i64 = 6 * 60 * 60;

struct AppsPeriod {
    since_utc: DateTime<Utc>,
    title: String,
    line: String,
}

fn local_midnight(now_local: DateTime<Local>) -> DateTime<Local> {
    let naive_midnight = now_local.date_naive().and_hms_opt(0, 0, 0).unwrap();
    Local
        .from_local_datetime(&naive_midnight)
        .earliest()
        .or_else(|| Local.from_local_datetime(&naive_midnight).latest())
        .unwrap_or(now_local)
}

fn sleep_window_bounds(now_local: DateTime<Local>) -> (DateTime<Local>, DateTime<Local>) {
    let today = now_local.date_naive();
    let start_today = today
        .and_hms_opt(MONITOR_SLEEP_WINDOW_START_HOUR, 0, 0)
        .unwrap();
    let start_today = Local
        .from_local_datetime(&start_today)
        .earliest()
        .unwrap_or(now_local);
    if now_local.hour() >= MONITOR_SLEEP_WINDOW_START_HOUR {
        let end = start_today + chrono::Duration::hours(24);
        (start_today, end)
    } else {
        let start = start_today - chrono::Duration::hours(24);
        (start, start_today)
    }
}

fn midpoint_score(midpoint_local: DateTime<Local>) -> f64 {
    let secs = midpoint_local.num_seconds_from_midnight() as i64;
    let target_secs = (MONITOR_SLEEP_TARGET_HOUR as i64) * 3600;
    let diff = (secs - target_secs).abs();
    let diff = diff.min(MONITOR_SLEEP_MIDPOINT_MAX_DIFF_SECS);
    1.0 - (diff as f64 / MONITOR_SLEEP_MIDPOINT_MAX_DIFF_SECS as f64)
}

fn overlap_seconds(
    start: DateTime<Local>,
    end: DateTime<Local>,
    window_start: DateTime<Local>,
    window_end: DateTime<Local>,
) -> i64 {
    let s = start.max(window_start);
    let e = end.min(window_end);
    (e - s).num_seconds().max(0)
}

fn night_band_overlap_seconds(start: DateTime<Local>, end: DateTime<Local>) -> i64 {
    let midpoint = start + (end - start) / 2;
    let base = midpoint.date_naive();
    let band_start = base
        .and_hms_opt(MONITOR_SLEEP_NIGHT_START_HOUR, 0, 0)
        .unwrap();
    let band_start = Local
        .from_local_datetime(&band_start)
        .earliest()
        .unwrap_or(midpoint);
    let band_end = band_start
        + chrono::Duration::hours(
            (24 - MONITOR_SLEEP_NIGHT_START_HOUR + MONITOR_SLEEP_NIGHT_END_HOUR) as i64,
        );
    overlap_seconds(start, end, band_start, band_end)
}

fn find_sleep_period_with_window(
    records: &[ActivityRecord],
    now: DateTime<Utc>,
    window_start_local: DateTime<Local>,
    window_end_local: DateTime<Local>,
    min_duration_secs: i64,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let mut segments: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    let mut current_start: Option<DateTime<Utc>> = None;
    let mut current_end: Option<DateTime<Utc>> = None;

    for record in records {
        let is_loginwindow = record
            .focus_info
            .as_ref()
            .map(|f| f.app_name.eq_ignore_ascii_case(LOGINWINDOW_APP))
            .unwrap_or(false);
        let rec_end = record.end_time.unwrap_or(now);

        if is_loginwindow {
            if current_start.is_none() {
                current_start = Some(record.start_time);
                current_end = Some(rec_end);
            } else if let Some(end) = current_end.as_mut() {
                if rec_end > *end {
                    *end = rec_end;
                }
            }
        } else if let (Some(start), Some(end)) = (current_start.take(), current_end.take()) {
            segments.push((start, end));
        }
    }

    if let (Some(start), Some(end)) = (current_start.take(), current_end.take()) {
        segments.push((start, end));
    }

    if segments.is_empty() {
        return None;
    }

    segments.sort_by(|a, b| a.0.cmp(&b.0));
    let mut merged: Vec<(DateTime<Utc>, DateTime<Utc>)> = Vec::new();
    for (start, end) in segments {
        if let Some(last) = merged.last_mut() {
            let gap = (start - last.1).num_seconds();
            if gap >= 0 && gap <= MONITOR_SLEEP_MERGE_GAP_SECS {
                if end > last.1 {
                    last.1 = end;
                }
                continue;
            }
        }
        merged.push((start, end));
    }

    let mut best: Option<(f64, (DateTime<Utc>, DateTime<Utc>))> = None;
    for (start_utc, end_utc) in merged {
        let start_local = start_utc.with_timezone(&Local);
        let end_local = end_utc.with_timezone(&Local);
        let overlap = overlap_seconds(start_local, end_local, window_start_local, window_end_local);
        if overlap <= 0 {
            continue;
        }
        let clipped_start = start_local.max(window_start_local);
        let clipped_end = end_local.min(window_end_local);
        let duration_secs = (clipped_end - clipped_start).num_seconds();
        if duration_secs < min_duration_secs {
            continue;
        }

        let midpoint = clipped_start + (clipped_end - clipped_start) / 2;
        let duration_hours = duration_secs as f64 / 3600.0;
        let duration_score = duration_hours.min(12.0) / 12.0;
        let midpoint_score = midpoint_score(midpoint);
        let night_overlap = night_band_overlap_seconds(clipped_start, clipped_end);
        let night_score = if duration_secs > 0 {
            night_overlap as f64 / duration_secs as f64
        } else {
            0.0
        };
        let score = 0.5 * duration_score + 0.3 * midpoint_score + 0.2 * night_score;

        let clipped_start_utc = clipped_start.with_timezone(&Utc);
        let clipped_end_utc = clipped_end.with_timezone(&Utc);
        if best
            .as_ref()
            .map(|(best_score, _)| score > *best_score)
            .unwrap_or(true)
        {
            best = Some((score, (clipped_start_utc, clipped_end_utc)));
        }
    }

    best.map(|(_, period)| period)
}

fn find_sleep_period(
    records: &[ActivityRecord],
    now: DateTime<Utc>,
    min_duration_secs: i64,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let now_local = Local::now();
    let (window_start_local, window_end_local) = sleep_window_bounds(now_local);
    find_sleep_period_with_window(
        records,
        now,
        window_start_local,
        window_end_local,
        min_duration_secs,
    )
}

fn select_apps_period(records: &[ActivityRecord]) -> AppsPeriod {
    let now_utc = Utc::now();
    let now_local = Local::now();
    if let Some((start, end)) = find_sleep_period(records, now_utc, MONITOR_SLEEP_MIN_SECS) {
        let local_start = start.with_timezone(&Local);
        let local_end = end.with_timezone(&Local);
        let duration_secs = (end - start).num_seconds().max(0) as u64;
        let title = format!("Apps (since {})", local_start.format("%H:%M"));
        let line = format!(
            "Period: last loginwindow {}–{} ({})",
            local_start.format("%H:%M"),
            local_end.format("%H:%M"),
            pretty_duration(duration_secs)
        );
        AppsPeriod {
            since_utc: start,
            title,
            line,
        }
    } else {
        let midnight_local = local_midnight(now_local);
        let title = format!("Apps (since {})", midnight_local.format("%H:%M"));
        let line = format!(
            "Period: since {} (fallback: local midnight)",
            midnight_local.format("%H:%M")
        );
        AppsPeriod {
            since_utc: midnight_local.with_timezone(&Utc),
            title,
            line,
        }
    }
}

fn build_app_tree(
    aggregated_activities: &[AggregatedActivity],
    app_short_max_duration_secs: u64,
    app_short_min_distinct: usize,
) -> Vec<kronical::daemon::snapshot::SnapshotApp> {
    use kronical::daemon::snapshot::{SnapshotApp, SnapshotWindow};
    use std::collections::HashMap as StdHashMap;

    let mut short_per_name: StdHashMap<&str, Vec<&AggregatedActivity>> =
        StdHashMap::with_capacity(16);
    let mut normal: Vec<&AggregatedActivity> = Vec::with_capacity(aggregated_activities.len());
    for agg in aggregated_activities.iter() {
        if agg.total_duration_seconds <= app_short_max_duration_secs {
            short_per_name.entry(&agg.app_name).or_default().push(agg);
        } else {
            normal.push(agg);
        }
    }

    let mut items: Vec<SnapshotApp> = Vec::with_capacity(normal.len() + short_per_name.len());
    for agg in normal {
        let mut windows: Vec<SnapshotWindow> = Vec::with_capacity(agg.windows.len());
        for w in agg.windows.values() {
            windows.push(SnapshotWindow {
                window_id: w.window_id.to_string(),
                window_title: (*w.window_title).clone(),
                first_seen: w.first_seen,
                last_seen: w.last_seen,
                duration_seconds: w.duration_seconds,
                is_group: false,
            });
        }
        let mut groups: Vec<SnapshotWindow> = Vec::with_capacity(agg.ephemeral_groups.len());
        for g in agg.ephemeral_groups.values() {
            let avg = if g.occurrence_count > 0 {
                g.total_duration_seconds / g.occurrence_count as u64
            } else {
                0
            };
            let title = format!(
                "(short-lived) ×{} avg {}",
                g.distinct_ids.len(),
                pretty_duration(avg)
            );
            groups.push(SnapshotWindow {
                window_id: format!("group:{}", g.title_key),
                window_title: title,
                first_seen: g.first_seen,
                last_seen: g.last_seen,
                duration_seconds: g.total_duration_seconds,
                is_group: true,
            });
        }
        let mut temporal: Vec<SnapshotWindow> = Vec::with_capacity(agg.temporal_groups.len());
        for (idx, g) in agg.temporal_groups.iter().enumerate() {
            let avg = if g.occurrence_count > 0 {
                g.total_duration_seconds / g.occurrence_count as u64
            } else {
                0
            };
            let title = format!(
                "(temporal locality) ×{} avg {} max {}",
                g.occurrence_count,
                pretty_duration(avg),
                pretty_duration(g.max_duration_seconds),
            );
            let window_id = format!(
                "group-temporal:{}:{}:{}",
                g.title_key,
                g.anchor_last_seen.timestamp(),
                idx
            );
            temporal.push(SnapshotWindow {
                window_id,
                window_title: title,
                first_seen: g.first_seen,
                last_seen: g.last_seen,
                duration_seconds: g.total_duration_seconds,
                is_group: true,
            });
        }
        windows.append(&mut groups);
        windows.append(&mut temporal);
        windows.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
        items.push(SnapshotApp {
            app_name: (*agg.app_name).clone(),
            pid: agg.pid,
            process_start_time: agg.process_start_time,
            windows,
            total_duration_secs: agg.total_duration_seconds,
            total_duration_pretty: pretty_duration(agg.total_duration_seconds),
        });
    }

    for (name, v) in short_per_name.into_iter() {
        if v.len() >= app_short_min_distinct {
            let mut total = 0u64;
            let mut first_seen = chrono::Utc::now();
            let mut last_seen = chrono::Utc::now();
            let mut rep_pid = 0;
            let mut rep_start = 0u64;
            let mut max_dur = 0u64;
            for agg in v.iter() {
                total = total.saturating_add(agg.total_duration_seconds);
                if agg.first_seen < first_seen {
                    first_seen = agg.first_seen;
                }
                if agg.last_seen > last_seen {
                    last_seen = agg.last_seen;
                }
                if agg.total_duration_seconds > max_dur {
                    max_dur = agg.total_duration_seconds;
                    rep_pid = agg.pid;
                    rep_start = agg.process_start_time;
                }
            }
            let count = v.len();
            let avg = if count > 0 { total / count as u64 } else { 0 };
            let windows = vec![SnapshotWindow {
                window_id: format!("app-group:{}", name),
                window_title: format!("×{} avg {}", count, pretty_duration(avg)),
                first_seen,
                last_seen,
                duration_seconds: total,
                is_group: true,
            }];
            items.push(SnapshotApp {
                app_name: name.to_string(),
                pid: rep_pid,
                process_start_time: rep_start,
                windows,
                total_duration_secs: total,
                total_duration_pretty: pretty_duration(total),
            });
        }
    }

    items.sort_by(|a, b| {
        let a_last = a.windows.first().map(|w| w.last_seen).unwrap_or_else(|| {
            chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::UNIX_EPOCH)
        });
        let b_last = b.windows.first().map(|w| w.last_seen).unwrap_or_else(|| {
            chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::UNIX_EPOCH)
        });
        b_last.cmp(&a_last)
    });
    items
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
    Start {
        #[arg(long, value_name = "UUID")]
        run: Option<String>,
    },
    Stop,
    Restart {
        #[arg(long, value_name = "UUID")]
        run: Option<String>,
    },
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
    Log {
        #[command(subcommand)]
        action: log_cli::LogCommand,
    },
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

fn parse_run_id(run_id: Option<String>) -> Result<Option<String>> {
    match run_id {
        Some(id) => kronical::util::run_id::parse_run_id(&id).map(Some),
        None => Ok(None),
    }
}

fn load_previous_run_id(workspace_dir: &PathBuf) -> Result<Option<String>> {
    let run_id_path = kronical::util::paths::run_id_file(workspace_dir);
    if !run_id_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&run_id_path)
        .with_context(|| format!("reading run id file {}", run_id_path.display()))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    kronical::util::run_id::parse_run_id(trimmed)
        .map(Some)
        .with_context(|| format!("invalid run id in {}", run_id_path.display()))
}

fn spawn_kronid(run_id: Option<&str>) -> Result<()> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    let cmd = if let Some(dir) = exe_dir {
        dir.join("kronid")
    } else {
        PathBuf::from("kronid")
    };
    let mut command = Command::new(cmd);
    if let Some(id) = run_id {
        command.arg("--run").arg(id);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to spawn kronid")?;
    Ok(())
}

fn stop_daemon(data_file: PathBuf) -> Result<()> {
    let pid_file = kronical::util::paths::pid_file(data_file.parent().unwrap());
    let run_id = load_previous_run_id(&data_file.parent().unwrap().to_path_buf())
        .ok()
        .flatten()
        .unwrap_or_else(|| "null".to_string());
    if let Some(pid) = read_pid_file(&pid_file)? {
        if is_process_running(pid) {
            println!(
                "Stopping Kronical daemon (PID: {}, run_id: {})...",
                pid, run_id
            );
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_millis(500));
            if !is_process_running(pid) {
                println!("Kronical daemon stopped successfully (run_id: {})", run_id);
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

fn start_daemon(data_file: PathBuf, _app_config: AppConfig, run_id: Option<String>) -> Result<()> {
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
    let run_id_str = run_id.as_deref().unwrap_or("null");
    println!(
        "Starting Kronical daemon in background (run_id: {})...",
        run_id_str
    );
    spawn_kronid(run_id.as_deref())?;
    Ok(())
}

fn restart_daemon(data_file: PathBuf, app_config: AppConfig, run_id: Option<String>) -> Result<()> {
    println!("Restarting Kronical daemon...");
    let _ = stop_daemon(data_file.clone());
    let run_id = match run_id {
        Some(id) => Some(id),
        None => load_previous_run_id(&app_config.workspace_dir)?,
    };
    start_daemon(data_file, app_config, run_id)
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
            println!("  Run: {}", snap.run_id.as_deref().unwrap_or("-"));
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

fn monitor_realtime(data_file: PathBuf, config: AppConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableMouseCapture
    )?;
    let res = run_monitor_loop(&mut terminal, data_file, &config);
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
    config: &AppConfig,
) -> io::Result<()> {
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
                        let payload = line[5..].trim_end();
                        data_buf.push_str(payload);
                        data_buf.push('\n');
                    } else if line == "\r\n" || line == "\n" {
                        if !data_buf.is_empty() {
                            let snap_result = serde_json::from_str::<
                                kronical::daemon::snapshot::Snapshot,
                            >(data_buf.trim_end());
                            if let Ok(snap) = snap_result {
                                terminal.draw(|f| {
                                    let size = f.area();
                                    let layout = Layout::default()
                                        .direction(Direction::Vertical)
                                        .constraints([
                                            Constraint::Length(7),
                                            Constraint::Length(12),
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
                                            Span::styled("Run: ", Style::default().fg(Color::Gray)),
                                            Span::raw(snap.run_id.as_deref().unwrap_or("-")),
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
                                    let (apps, apps_period) =
                                        if config.monitor_apps_sleep_filter_enabled {
                                            let period = select_apps_period(&snap.records);
                                            let aggregated = aggregate_activities_since(
                                                &snap.records,
                                                period.since_utc,
                                                Utc::now(),
                                                config.ephemeral_max_duration_secs,
                                                config.ephemeral_min_distinct_ids,
                                                config.max_windows_per_app,
                                            );
                                            let apps = build_app_tree(
                                                &aggregated,
                                                config.ephemeral_app_max_duration_secs,
                                                config.ephemeral_app_min_distinct_procs,
                                            );
                                            (apps, Some(period))
                                        } else {
                                            (snap.aggregated_apps.clone(), None)
                                        };

                                    let mut app_lines: Vec<Line> = Vec::new();
                                    if let Some(period) = &apps_period {
                                        app_lines.push(Line::from(Span::styled(
                                            period.line.clone(),
                                            Style::default().fg(Color::Gray),
                                        )));
                                    }
                                    if !apps.is_empty() {
                                        let mut shown = 0usize;
                                        for app in &apps {
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
                                    } else if apps_period.is_some() {
                                        app_lines
                                            .push(Line::from("Apps: (no activity in this period)"));
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
                                    details.push(Line::from(format!(
                                        "Transitions captured: {}",
                                        snap.transitions_recent.len()
                                    )));
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
                                    let apps_title = apps_period
                                        .as_ref()
                                        .map(|p| p.title.as_str())
                                        .unwrap_or("Apps");
                                    let p_apps = Paragraph::new(app_lines)
                                        .block(
                                            Block::default()
                                                .title(apps_title)
                                                .borders(Borders::ALL),
                                        )
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
        Commands::Start { run } => {
            let run_id = match parse_run_id(run) {
                Ok(id) => id,
                Err(e) => {
                    error!("Invalid run id: {}", e);
                    process::exit(1);
                }
            };
            start_daemon(data_file, config.clone(), run_id)
        }
        Commands::Stop => stop_daemon(data_file),
        Commands::Restart { run } => {
            let run_id = match parse_run_id(run) {
                Ok(id) => id,
                Err(e) => {
                    error!("Invalid run id: {}", e);
                    process::exit(1);
                }
            };
            restart_daemon(data_file, config.clone(), run_id)
        }
        Commands::Status => get_status(data_file),
        Commands::Snapshot { pretty } => snapshot_autoselect(
            &kronical::util::paths::http_uds(&config.workspace_dir),
            pretty,
        ),
        Commands::Watch { pretty } => watch_via_http(
            &kronical::util::paths::http_uds(&config.workspace_dir),
            pretty,
        ),
        Commands::Monitor => monitor_realtime(data_file, config.clone()),
        Commands::Log { action } => log_cli::execute(action, &config.workspace_dir),
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

#[cfg(test)]
mod tests {
    use super::{LOGINWINDOW_APP, find_sleep_period_with_window, local_midnight};
    use chrono::{Local, TimeZone, Timelike, Utc};
    use kronical_core::events::{MousePosition, WindowFocusInfo};
    use kronical_core::records::{ActivityRecord, ActivityState};
    use std::sync::Arc;

    fn record_with_focus(
        app_name: &str,
        start: chrono::DateTime<Utc>,
        end: chrono::DateTime<Utc>,
    ) -> ActivityRecord {
        ActivityRecord {
            record_id: 0,
            run_id: None,
            start_time: start,
            end_time: Some(end),
            state: ActivityState::Active,
            focus_info: Some(WindowFocusInfo {
                pid: 1,
                process_start_time: 0,
                app_name: Arc::new(app_name.to_string()),
                window_title: Arc::new("title".to_string()),
                window_id: 1,
                window_instance_start: start,
                window_position: Some(MousePosition { x: 0, y: 0 }),
                window_size: Some((1, 1)),
            }),
            event_count: 0,
            triggering_events: Vec::new(),
        }
    }

    #[test]
    fn find_sleep_period_prefers_best_midnight_window() {
        let base_local = Local.with_ymd_and_hms(2024, 1, 1, 21, 0, 0).unwrap();
        let t0 = base_local.with_timezone(&Utc);
        let records = vec![
            record_with_focus(
                LOGINWINDOW_APP,
                t0 + chrono::Duration::hours(1),
                t0 + chrono::Duration::hours(3),
            ),
            record_with_focus(
                "Terminal",
                t0 + chrono::Duration::hours(3),
                t0 + chrono::Duration::hours(4),
            ),
            record_with_focus(
                LOGINWINDOW_APP,
                t0 + chrono::Duration::hours(5),
                t0 + chrono::Duration::hours(10),
            ),
            record_with_focus(
                "Browser",
                t0 + chrono::Duration::hours(10),
                t0 + chrono::Duration::hours(11),
            ),
            record_with_focus(
                LOGINWINDOW_APP,
                t0 + chrono::Duration::hours(12),
                t0 + chrono::Duration::hours(15),
            ),
        ];

        let window_start = base_local.date_naive().and_hms_opt(21, 0, 0).unwrap();
        let window_start = Local
            .from_local_datetime(&window_start)
            .earliest()
            .unwrap_or(base_local);
        let window_end = window_start + chrono::Duration::hours(24);
        let period = find_sleep_period_with_window(
            &records,
            t0 + chrono::Duration::hours(24),
            window_start,
            window_end,
            4 * 60 * 60,
        )
        .expect("expected long loginwindow period");

        assert_eq!(period.0, t0 + chrono::Duration::hours(5));
        assert_eq!(period.1, t0 + chrono::Duration::hours(10));
    }

    #[test]
    fn find_sleep_period_merges_consecutive_records() {
        let base_local = Local.with_ymd_and_hms(2024, 1, 2, 21, 0, 0).unwrap();
        let t0 = base_local.with_timezone(&Utc);
        let records = vec![
            record_with_focus(
                LOGINWINDOW_APP,
                t0 + chrono::Duration::hours(1),
                t0 + chrono::Duration::hours(2),
            ),
            record_with_focus(
                LOGINWINDOW_APP,
                t0 + chrono::Duration::hours(2),
                t0 + chrono::Duration::hours(4),
            ),
            record_with_focus(
                "Terminal",
                t0 + chrono::Duration::hours(4),
                t0 + chrono::Duration::hours(5),
            ),
        ];

        let window_start = base_local.date_naive().and_hms_opt(21, 0, 0).unwrap();
        let window_start = Local
            .from_local_datetime(&window_start)
            .earliest()
            .unwrap_or(base_local);
        let window_end = window_start + chrono::Duration::hours(24);
        let period = find_sleep_period_with_window(
            &records,
            t0 + chrono::Duration::hours(6),
            window_start,
            window_end,
            2 * 60 * 60,
        )
        .expect("expected merged loginwindow period");

        assert_eq!(period.0, t0 + chrono::Duration::hours(1));
        assert_eq!(period.1, t0 + chrono::Duration::hours(4));
    }

    #[test]
    fn local_midnight_returns_start_of_day() {
        let local_now = chrono::Local
            .with_ymd_and_hms(2024, 6, 1, 13, 45, 0)
            .unwrap();
        let midnight = local_midnight(local_now);
        assert_eq!(midnight.hour(), 0);
        assert_eq!(midnight.minute(), 0);
        assert_eq!(midnight.second(), 0);
    }
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
            process_start_time: f.process_start_time,
            app_name: Arc::new(f.app_name),
            window_title: Arc::new(f.window_title),
            window_id: f.window_id.parse().unwrap_or(0),
            window_instance_start: f
                .window_instance_start
                .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32))
                .unwrap_or_else(Utc::now),
            window_position: f
                .window_position
                .map(|pos| kronical::daemon::events::MousePosition { x: pos.x, y: pos.y }),
            window_size: f.window_size.map(|size| (size.width, size.height)),
        });
    let map_record_focus = |f: kronical::kroni_api::kroni::v1::snapshot_reply::Focus| {
        kronical::daemon::events::WindowFocusInfo {
            pid: f.pid,
            process_start_time: f.process_start_time,
            app_name: Arc::new(f.app_name),
            window_title: Arc::new(f.window_title),
            window_id: f.window_id.parse().unwrap_or(0),
            window_instance_start: f
                .window_instance_start
                .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32))
                .unwrap_or_else(Utc::now),
            window_position: f
                .window_position
                .map(|pos| kronical::daemon::events::MousePosition { x: pos.x, y: pos.y }),
            window_size: f.window_size.map(|size| (size.width, size.height)),
        }
    };
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
            run_id: if t.run_id.is_empty() {
                None
            } else {
                Some(t.run_id)
            },
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
    let records = reply
        .records
        .into_iter()
        .map(|r| kronical::daemon::records::ActivityRecord {
            record_id: r.record_id,
            run_id: if r.run_id.is_empty() {
                None
            } else {
                Some(r.run_id)
            },
            start_time: r
                .start_time
                .and_then(|ts| chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32))
                .unwrap_or_else(Utc::now),
            end_time: r.end_time.and_then(|ts| {
                chrono::DateTime::<Utc>::from_timestamp(ts.seconds, ts.nanos as u32)
            }),
            state: match r.state {
                1 => kronical::daemon::records::ActivityState::Active,
                2 => kronical::daemon::records::ActivityState::Passive,
                3 => kronical::daemon::records::ActivityState::Inactive,
                4 => kronical::daemon::records::ActivityState::Locked,
                _ => kronical::daemon::records::ActivityState::Inactive,
            },
            focus_info: r.focus.map(map_record_focus),
            event_count: r.event_count,
            triggering_events: r.triggering_events,
        })
        .collect();
    kronical::daemon::snapshot::Snapshot {
        seq: reply.seq,
        mono_ns: reply.mono_ns,
        run_id: if reply.run_id.is_empty() {
            None
        } else {
            Some(reply.run_id)
        },
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
        title_revisions_recent: Vec::new(),
        records,
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
    println!("- monoNs: {}", s.mono_ns);
    println!("- runId: {}", s.run_id.as_deref().unwrap_or("-"));
    println!("- state: {:?}", s.activity_state);
    if let Some(f) = &s.focus {
        println!("- focus: {} [{}] - {}", f.app_name, f.pid, f.window_title);
    } else {
        println!("- focus: -");
    }
    if let Some(t) = &s.last_transition {
        println!("- lastTransition: {:?} -> {:?} at {}", t.from, t.to, t.at);
    }
    println!("- cadence: {}ms ({})", s.cadence_ms, s.cadence_reason);
    if let Some(nt) = &s.next_timeout {
        println!("- nextTimeout: {}", nt);
    }
    println!(
        "- counts: signals={} hints={} records={}",
        s.counts.signals_seen, s.counts.hints_seen, s.counts.records_emitted
    );
    println!(
        "- storage: backlog={} lastFlushAt={}",
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
    println!("- records ({}):", s.records.len());
    for record in &s.records {
        let end = record
            .end_time
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "-".to_string());
        let duration_s = record
            .end_time
            .map(|t| (t - record.start_time).num_seconds().max(0))
            .map(|d| d.to_string())
            .unwrap_or_else(|| "-".to_string());
        let focus = record.focus_info.as_ref().map(|f| {
            format!(
                "{} [{}] - {} (wid={})",
                f.app_name, f.pid, f.window_title, f.window_id
            )
        });
        println!(
            "  - id={} state={:?} start={} end={} dur={}s events={} triggers={}",
            record.record_id,
            record.state,
            record.start_time.to_rfc3339(),
            end,
            duration_s,
            record.event_count,
            record.triggering_events.len()
        );
        if let Some(info) = focus {
            println!("    focus: {}", info);
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
        "seq={} run={} state={:?} focus={} cad={}ms backlog={}",
        s.seq,
        s.run_id.as_deref().unwrap_or("-"),
        s.activity_state,
        focus,
        s.cadence_ms,
        s.storage.backlog_count
    );
}

// gRPC API always available; feature gate removed
