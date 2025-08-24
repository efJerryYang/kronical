use crate::records::ActivityRecord;
use anyhow::Result;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivityWindow {
    pub window_id: String,
    pub window_title: String,
    pub last_active: String,
    pub duration_seconds: u64,
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
    recent_records: Arc<Mutex<Vec<ActivityRecord>>>,
    notification_clients: Arc<Mutex<Vec<UnixStream>>>,
}

impl SocketServer {
    pub fn new(socket_path: PathBuf) -> Self {
        let notification_socket_path = socket_path.with_extension("notify.sock");
        Self {
            socket_path,
            notification_socket_path,
            recent_records: Arc::new(Mutex::new(Vec::new())),
            notification_clients: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn notify_clients(&self) {
        let mut clients = self.notification_clients.lock().unwrap();
        clients.retain_mut(|stream| match stream.write_all(&[1]) {
            Ok(_) => true,
            Err(_) => false,
        });
    }

    pub fn update_records(&self, records: Vec<ActivityRecord>) {
        let mut recent = self.recent_records.lock().unwrap();
        *recent = records;
        info!("Updated socket server with {} records", recent.len());
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
                if let Ok(stream) = stream {
                    notification_clients.lock().unwrap().push(stream);
                }
            }
        });

        let recent_records = Arc::clone(&self.recent_records);
        let socket_path = self.socket_path.clone();

        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let records = Arc::clone(&recent_records);
                        let socket_path_clone = socket_path.clone();
                        thread::spawn(move || {
                            if let Err(e) = handle_client(stream, records, socket_path_clone) {
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
    recent_records: Arc<Mutex<Vec<ActivityRecord>>>,
    socket_path: PathBuf,
) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();

    reader.read_line(&mut line)?;
    let command = line.trim();

    info!("Received command: {}", command);

    match command {
        "status" => {
            let response = generate_status_response(recent_records, socket_path)?;
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
    recent_records: Arc<Mutex<Vec<ActivityRecord>>>,
    socket_path: PathBuf,
) -> Result<MonitorResponse> {
    let records = recent_records.lock().unwrap();
    let data_dir = socket_path.parent().unwrap();
    let data_file_size_mb = get_data_file_size(data_dir);
    let recent_apps = build_app_tree(&records);

    Ok(MonitorResponse {
        daemon_status: "running".to_string(),
        data_file_size_mb,
        data_directory: data_dir.to_string_lossy().to_string(),
        recent_apps,
    })
}

fn build_app_tree(records: &[ActivityRecord]) -> Vec<ActivityApp> {
    // The outer key is the unique process identifier (pid + start_time).
    // The inner tuple holds the final ActivityApp and a map from window_id to its index in the windows vector.
    let mut app_map: HashMap<String, (ActivityApp, HashMap<String, usize>)> = HashMap::new();

    for record in records.iter().rev().take(50) {
        if let Some(focus_info) = &record.focus_info {
            let app_key = format!("{}_{}", focus_info.pid, focus_info.process_start_time);

            let duration = if let Some(end_time) = record.end_time {
                (end_time - record.start_time).num_seconds() as u64
            } else {
                0
            };

            let (app, window_map) = app_map.entry(app_key).or_insert_with(|| {
                (
                    ActivityApp {
                        app_name: focus_info.app_name.clone(),
                        pid: focus_info.pid,
                        process_start_time: focus_info.process_start_time,
                        windows: Vec::new(),
                        total_duration_secs: 0,
                        total_duration_pretty: "0s".to_string(),
                    },
                    HashMap::new(),
                )
            });

            app.total_duration_secs += duration;

            if !focus_info.window_title.is_empty() {
                // Check if we've already seen this window_id
                if let Some(index) = window_map.get(&focus_info.window_id) {
                    // Yes, just add to its duration
                    app.windows[*index].duration_seconds += duration;
                } else {
                    // No, this is the first time we see this window (iterating backwards in time).
                    // This means this record has the most recent title and last_active time.
                    let new_window = ActivityWindow {
                        window_id: focus_info.window_id.clone(),
                        window_title: focus_info.window_title.clone(),
                        last_active: record
                            .start_time
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M:%S")
                            .to_string(),
                        duration_seconds: duration,
                    };
                    app.windows.push(new_window);
                    let new_index = app.windows.len() - 1;
                    window_map.insert(focus_info.window_id.clone(), new_index);
                }
            }
        }
    }

    let mut apps: Vec<ActivityApp> = app_map
        .into_values()
        .map(|(mut app, _)| {
            app.total_duration_pretty = pretty_format_duration(app.total_duration_secs);
            app
        })
        .collect();

    apps.sort_by(|a, b| b.total_duration_secs.cmp(&a.total_duration_secs));
    apps.truncate(5);

    for app in &mut apps {
        app.windows
            .sort_by(|a, b| b.last_active.cmp(&a.last_active));
        app.windows.truncate(5);
    }

    apps
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
