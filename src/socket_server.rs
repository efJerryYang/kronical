use crate::records::ActivityRecord;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::io::{BufRead, BufReader, Write};
use log::{error, info};

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
    pub memory_mb: f64,
    pub cpu_percent: f64,
    pub data_file_size_mb: f64,
    pub recent_apps: Vec<ActivityApp>,
    pub compression_stats: CompressionInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionInfo {
    pub events_in_ring_buffer: usize,
    pub compact_events_stored: usize,
    pub event_compression_ratio: f32,
    pub memory_compression_ratio: f32,
    pub bytes_saved_kb: usize,
    pub ring_buffer_size_kb: usize,
    pub compact_storage_size_kb: usize,
    pub string_table_size_kb: usize,
    pub raw_events_memory_mb: f32,
    pub compact_events_memory_mb: f32,
    pub records_collection_memory_mb: f32,
    pub activities_collection_memory_mb: f32,
}

pub struct SocketServer {
    socket_path: PathBuf,
    notification_socket_path: PathBuf,
    recent_records: Arc<Mutex<Vec<ActivityRecord>>>,
    compression_stats: Arc<Mutex<Option<CompressionInfo>>>,
    notification_clients: Arc<Mutex<Vec<UnixStream>>>,
}

impl SocketServer {
    pub fn new(socket_path: PathBuf) -> Self {
        let notification_socket_path = socket_path.with_extension("notify.sock");
        Self {
            socket_path,
            notification_socket_path,
            recent_records: Arc::new(Mutex::new(Vec::new())),
            compression_stats: Arc::new(Mutex::new(None)),
            notification_clients: Arc::new(Mutex::new(Vec::new()))
        }
    }

    fn notify_clients(&self) {
        let mut clients = self.notification_clients.lock().unwrap();
        clients.retain_mut(|stream| {
            match stream.write_all(&[1]) {
                Ok(_) => true,
                Err(_) => false,
            }
        });
    }
    
    pub fn update_records(&self, records: Vec<ActivityRecord>) {
        let mut recent = self.recent_records.lock().unwrap();
        *recent = records;
        info!("Updated socket server with {} records", recent.len());
        self.notify_clients();
    }
    
    pub fn update_compression_stats(&self, stats: CompressionInfo) {
        let mut compression = self.compression_stats.lock().unwrap();
        *compression = Some(stats);
        info!("Updated socket server with compression stats");
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
        info!("Notification server listening on: {:?}", self.notification_socket_path);

        let notification_clients = Arc::clone(&self.notification_clients);
        thread::spawn(move || {
            for stream in notification_listener.incoming() {
                if let Ok(stream) = stream {
                    notification_clients.lock().unwrap().push(stream);
                }
            }
        });
        
        let recent_records = Arc::clone(&self.recent_records);
        let compression_stats = Arc::clone(&self.compression_stats);
        let socket_path = self.socket_path.clone();
        
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        let records = Arc::clone(&recent_records);
                        let stats = Arc::clone(&compression_stats);
                        let socket_path_clone = socket_path.clone();
                        thread::spawn(move || {
                            if let Err(e) = handle_client(stream, records, stats, socket_path_clone) {
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
    compression_stats: Arc<Mutex<Option<CompressionInfo>>>,
    socket_path: PathBuf
) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    
    reader.read_line(&mut line)?;
    let command = line.trim();
    
    info!("Received command: {}", command);
    
    match command {
        "status" => {
            let response = generate_status_response(recent_records, compression_stats, socket_path)?;
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
    compression_stats: Arc<Mutex<Option<CompressionInfo>>>,
    socket_path: PathBuf
) -> Result<MonitorResponse> {
    let records = recent_records.lock().unwrap();
    let compression = compression_stats.lock().unwrap();
    
    let memory_mb = get_current_process_memory();
    let cpu_percent = get_current_process_cpu();
    let data_file_size_mb = get_data_file_size(socket_path.parent().unwrap());
    
    let recent_apps = build_app_tree(&records);
    
    let compression_info = compression.clone().unwrap_or(CompressionInfo {
        events_in_ring_buffer: 0,
        compact_events_stored: 0,
        event_compression_ratio: 1.0,
        memory_compression_ratio: 1.0,
        bytes_saved_kb: 0,
        ring_buffer_size_kb: 0,
        compact_storage_size_kb: 0,
        string_table_size_kb: 0,
        raw_events_memory_mb: 0.0,
        compact_events_memory_mb: 0.0,
        records_collection_memory_mb: 0.0,
        activities_collection_memory_mb: 0.0,
    });
    
    Ok(MonitorResponse {
        daemon_status: "running".to_string(),
        memory_mb,
        cpu_percent,
        data_file_size_mb,
        recent_apps,
        compression_stats: compression_info,
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
                        last_active: record.start_time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string(),
                        duration_seconds: duration,
                    };
                    app.windows.push(new_window);
                    let new_index = app.windows.len() - 1;
                    window_map.insert(focus_info.window_id.clone(), new_index);
                }
            }
        }
    }
    
    let mut apps: Vec<ActivityApp> = app_map.into_values().map(|(mut app, _)| {
        app.total_duration_pretty = pretty_format_duration(app.total_duration_secs);
        app
    }).collect();

    apps.sort_by(|a, b| b.total_duration_secs.cmp(&a.total_duration_secs));
    apps.truncate(5);
    
    for app in &mut apps {
        app.windows.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        app.windows.truncate(5);
    }
    
    apps
}

use once_cell::sync::Lazy;
use sysinfo::{System, Pid};

static SYSTEM: Lazy<Mutex<System>> = Lazy::new(|| Mutex::new(System::new()));

fn get_current_process_memory() -> f64 {
    let mut system = SYSTEM.lock().unwrap();
    let current_pid = std::process::id();
    system.refresh_process(Pid::from(current_pid as usize));
    
    if let Some(process) = system.process(Pid::from(current_pid as usize)) {
        process.memory() as f64 / 1024.0 / 1024.0
    } else {
        0.0
    }
}

fn get_current_process_cpu() -> f64 {
    let mut system = SYSTEM.lock().unwrap();
    let current_pid = std::process::id();
    system.refresh_process(Pid::from(current_pid as usize));
    
    if let Some(process) = system.process(Pid::from(current_pid as usize)) {
        process.cpu_usage() as f64
    } else {
        0.0
    }
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
