use crate::daemon::records::AggregatedActivity;
use anyhow::Result;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivityWindow {
    pub window_id: String,
    pub window_title: String,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub duration_seconds: u64,
    pub is_group: bool,
}

fn pretty_format_duration(seconds: u64) -> String {
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

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivityApp {
    pub app_name: String,
    pub pid: i32,
    pub process_start_time: u64,
    pub windows: Vec<ActivityWindow>,
    pub total_duration_secs: u64,
    pub total_duration_pretty: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MonitorResponse {
    pub daemon_status: String,
    pub data_file_size_mb: f64,
    pub data_directory: String,
    pub recent_apps: Vec<ActivityApp>,
}

pub struct SocketServer {
    socket_path: PathBuf,
    notification_socket_path: PathBuf,
    aggregated_activities: Arc<Mutex<Vec<AggregatedActivity>>>,
    notification_clients: Arc<Mutex<Vec<UnixStream>>>,
    app_short_max_duration_secs: u64,
    app_short_min_distinct: usize,
}

impl SocketServer {
    pub fn new(
        socket_path: PathBuf,
        app_short_max_duration_secs: u64,
        app_short_min_distinct: usize,
    ) -> Self {
        let notification_socket_path = socket_path.with_extension("notify.sock");
        Self {
            socket_path,
            notification_socket_path,
            aggregated_activities: Arc::new(Mutex::new(Vec::new())),
            notification_clients: Arc::new(Mutex::new(Vec::new())),
            app_short_max_duration_secs,
            app_short_min_distinct,
        }
    }

    fn notify_clients(&self) {
        let mut clients = self.notification_clients.lock().unwrap();
        clients.retain_mut(|stream| stream.write_all(&[1]).is_ok());
    }

    pub fn update_aggregated_data(&self, aggregated_activities: Vec<AggregatedActivity>) {
        let mut aggregated = self.aggregated_activities.lock().unwrap();
        *aggregated = aggregated_activities;
        debug!(
            "Updated socket server with {} aggregated activities",
            aggregated.len()
        );
        self.notify_clients();
    }

    pub fn start(&self) -> Result<()> {
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }
        if self.notification_socket_path.exists() {
            std::fs::remove_file(&self.notification_socket_path)?;
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        info!("Socket server listening on: {:?}", self.socket_path);

        let notification_listener = UnixListener::bind(&self.notification_socket_path)?;
        info!(
            "Notification server listening on: {:?}",
            self.notification_socket_path
        );

        let notification_clients = Arc::clone(&self.notification_clients);
        thread::spawn(move || {
            for stream in notification_listener.incoming() {
                match stream {
                    Ok(stream) => {
                        notification_clients.lock().unwrap().push(stream);
                    }
                    Err(e) => {
                        error!("Error accepting notification connection: {}", e);
                    }
                }
            }
        });

        let aggregated_activities = Arc::clone(&self.aggregated_activities);
        let socket_path = self.socket_path.clone();
        let app_short_max = self.app_short_max_duration_secs;
        let app_short_min = self.app_short_min_distinct;

        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let aggregated = Arc::clone(&aggregated_activities);
                        let socket_path_clone = socket_path.clone();
                        thread::spawn(move || {
                            if let Err(e) = handle_client(
                                stream,
                                aggregated,
                                socket_path_clone,
                                app_short_max,
                                app_short_min,
                            ) {
                                error!("Error handling client: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        error!("Error accepting connection: {}", e);
                    }
                }
            }
        });

        Ok(())
    }
}

fn handle_client(
    mut stream: UnixStream,
    aggregated_activities: Arc<Mutex<Vec<AggregatedActivity>>>,
    socket_path: PathBuf,
    app_short_max: u64,
    app_short_min: usize,
) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    reader.read_line(&mut line)?;
    let command = line.trim();

    debug!("Received command: {}", command);

    match command {
        "status" => {
            let aggregated = aggregated_activities.lock().unwrap();
            let response =
                generate_status_response(&aggregated, socket_path, app_short_max, app_short_min)?;
            let json_response = serde_json::to_string(&response)?;
            writeln!(stream, "{}", json_response)?;
        }
        _ => {
            writeln!(stream, "{{\"error\": \"Unknown command\"}}")?;
        }
    }

    Ok(())
}

fn generate_status_response(
    aggregated_activities: &[AggregatedActivity],
    socket_path: PathBuf,
    app_short_max: u64,
    app_short_min: usize,
) -> Result<MonitorResponse> {
    let data_dir = socket_path.parent().unwrap();
    let data_file_size_mb = get_data_file_size(data_dir);
    let recent_apps = build_app_tree(aggregated_activities, app_short_max, app_short_min);

    Ok(MonitorResponse {
        daemon_status: "running".to_string(),
        data_file_size_mb,
        data_directory: data_dir.to_string_lossy().to_string(),
        recent_apps,
    })
}

fn build_app_tree(
    aggregated_activities: &[AggregatedActivity],
    app_short_max_duration_secs: u64,
    app_short_min_distinct: usize,
) -> Vec<ActivityApp> {
    // Partition activities into normal and short-lived buckets per app name
    use std::collections::HashMap as StdHashMap;
    let mut short_per_name: StdHashMap<&str, Vec<&AggregatedActivity>> = StdHashMap::new();
    let mut normal: Vec<&AggregatedActivity> = Vec::new();

    for agg in aggregated_activities.iter() {
        if agg.total_duration_seconds <= app_short_max_duration_secs {
            short_per_name.entry(&agg.app_name).or_default().push(agg);
        } else {
            normal.push(agg);
        }
    }

    let mut items: Vec<ActivityApp> = Vec::new();

    // Render normal apps
    for agg in normal {
        let mut windows: Vec<ActivityWindow> = agg
            .windows
            .values()
            .map(|window| ActivityWindow {
                window_id: window.window_id.clone(),
                window_title: window.window_title.clone(),
                first_seen: window.first_seen,
                last_seen: window.last_seen,
                duration_seconds: window.duration_seconds,
                is_group: false,
            })
            .collect();

        let mut groups: Vec<ActivityWindow> = agg
            .ephemeral_groups
            .values()
            .map(|g| {
                let avg = if g.occurrence_count > 0 {
                    g.total_duration_seconds / g.occurrence_count as u64
                } else {
                    0
                };
                let title = format!(
                    "{} (short-lived) ×{} avg {}",
                    g.title_key,
                    g.distinct_ids.len(),
                    pretty_format_duration(avg)
                );
                ActivityWindow {
                    window_id: format!("group:{}", g.title_key),
                    window_title: title,
                    first_seen: g.first_seen,
                    last_seen: g.last_seen,
                    duration_seconds: g.total_duration_seconds,
                    is_group: true,
                }
            })
            .collect();

        windows.append(&mut groups);
        windows.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));

        items.push(ActivityApp {
            app_name: agg.app_name.clone(),
            pid: agg.pid,
            process_start_time: agg.process_start_time,
            windows,
            total_duration_secs: agg.total_duration_seconds,
            total_duration_pretty: pretty_format_duration(agg.total_duration_seconds),
        });
    }

    // Render grouped short-lived apps by name
    for (name, v) in short_per_name.into_iter() {
        if v.len() >= app_short_min_distinct {
            let mut total: u64 = 0;
            let mut first_seen =
                chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now());
            let mut last_seen =
                chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::UNIX_EPOCH);
            let mut rep_pid: i32 = 0;
            let mut rep_start: u64 = 0;
            let mut max_dur: u64 = 0;
            for agg in &v {
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
            let mut windows: Vec<ActivityWindow> = Vec::new();
            windows.push(ActivityWindow {
                window_id: format!("app-group:{}", name),
                window_title: format!("×{} avg {}", count, pretty_format_duration(avg)),
                first_seen,
                last_seen,
                duration_seconds: total,
                is_group: true,
            });
            items.push(ActivityApp {
                app_name: format!("{} (short-lived)", name),
                pid: rep_pid,
                process_start_time: rep_start,
                windows,
                total_duration_secs: total,
                total_duration_pretty: pretty_format_duration(total),
            });
        } else {
            // Not enough to group; render individually
            for agg in v {
                let mut windows: Vec<ActivityWindow> = agg
                    .windows
                    .values()
                    .map(|window| ActivityWindow {
                        window_id: window.window_id.clone(),
                        window_title: window.window_title.clone(),
                        first_seen: window.first_seen,
                        last_seen: window.last_seen,
                        duration_seconds: window.duration_seconds,
                        is_group: false,
                    })
                    .collect();
                let mut groups: Vec<ActivityWindow> = agg
                    .ephemeral_groups
                    .values()
                    .map(|g| {
                        let avg = if g.occurrence_count > 0 {
                            g.total_duration_seconds / g.occurrence_count as u64
                        } else {
                            0
                        };
                        let title = format!(
                            "{} (short-lived) ×{} avg {}",
                            g.title_key,
                            g.distinct_ids.len(),
                            pretty_format_duration(avg)
                        );
                        ActivityWindow {
                            window_id: format!("group:{}", g.title_key),
                            window_title: title,
                            first_seen: g.first_seen,
                            last_seen: g.last_seen,
                            duration_seconds: g.total_duration_seconds,
                            is_group: true,
                        }
                    })
                    .collect();
                windows.append(&mut groups);
                windows.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
                items.push(ActivityApp {
                    app_name: agg.app_name.clone(),
                    pid: agg.pid,
                    process_start_time: agg.process_start_time,
                    windows,
                    total_duration_secs: agg.total_duration_seconds,
                    total_duration_pretty: pretty_format_duration(agg.total_duration_seconds),
                });
            }
        }
    }

    // Sort apps by recent activity
    items.sort_by(|a, b| {
        let a_last =
            a.windows.first().map(|w| w.last_seen).unwrap_or(
                chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::UNIX_EPOCH),
            );
        let b_last =
            b.windows.first().map(|w| w.last_seen).unwrap_or(
                chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::UNIX_EPOCH),
            );
        b_last.cmp(&a_last)
    });

    items
}

fn get_data_file_size(data_dir: &std::path::Path) -> f64 {
    // Check for SQLite database first (default)
    let sqlite_file = data_dir.join("data.db");
    if sqlite_file.exists() {
        return std::fs::metadata(&sqlite_file)
            .map(|m| m.len() as f64 / 1024.0 / 1024.0)
            .unwrap_or(0.0);
    }

    // Fall back to JSON file
    let json_file = data_dir.join("data.json");
    if json_file.exists() {
        std::fs::metadata(&json_file)
            .map(|m| m.len() as f64 / 1024.0 / 1024.0)
            .unwrap_or(0.0)
    } else {
        0.0
    }
}

impl Drop for SocketServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.notification_socket_path);
    }
}
