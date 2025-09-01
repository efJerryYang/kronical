use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use zip::{CompressionMethod, ZipWriter, write::FileOptions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub timestamp: DateTime<Utc>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_io_bytes: u64,
}

pub struct SystemTracker {
    pid: u32,
    interval: Duration,
    batch_size: usize,
    zip_path: PathBuf,
    running: Arc<Mutex<bool>>,
}

impl SystemTracker {
    pub fn new(pid: u32, interval_secs: f64, batch_size: usize, zip_path: PathBuf) -> Self {
        Self {
            pid,
            interval: Duration::from_secs_f64(interval_secs),
            batch_size,
            zip_path,
            running: Arc::new(Mutex::new(false)),
        }
    }

    pub fn start(&self) -> Result<()> {
        {
            let mut running = self.running.lock().unwrap();
            if *running {
                return Err(anyhow::anyhow!("System tracker is already running"));
            }
            *running = true;
        }

        info!(
            "Starting system tracker for PID {} with interval {:?}",
            self.pid, self.interval
        );

        let pid = self.pid;
        let interval = self.interval;
        let batch_size = self.batch_size;
        let zip_path = self.zip_path.clone();
        let running = Arc::clone(&self.running);

        thread::spawn(move || {
            let mut csv_writer = match create_csv_writer(&zip_path) {
                Ok(writer) => writer,
                Err(e) => {
                    error!("Failed to create CSV writer: {}", e);
                    return;
                }
            };

            let mut last_disk_io = 0u64;
            let mut batch_count = 0;

            while *running.lock().unwrap() {
                let start_time = Instant::now();

                match collect_system_metrics(pid, last_disk_io) {
                    Ok((cpu_percent, memory_bytes, disk_io_bytes)) => {
                        last_disk_io = disk_io_bytes;

                        let metrics = SystemMetrics {
                            timestamp: Utc::now(),
                            cpu_percent,
                            memory_bytes,
                            disk_io_bytes,
                        };

                        if let Err(e) = csv_writer.serialize(&metrics) {
                            error!("Failed to write metrics: {}", e);
                        } else {
                            batch_count += 1;

                            if batch_count % batch_size == 0 {
                                if let Err(e) = csv_writer.flush() {
                                    error!("Failed to flush CSV writer: {}", e);
                                } else {
                                    debug!("Flushed {} metrics to disk", batch_count);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if !is_process_running(pid) {
                            info!("Process {} exited, stopping tracker", pid);
                            break;
                        }
                        warn!("Failed to collect system metrics: {}", e);
                    }
                }

                let elapsed = start_time.elapsed();
                if elapsed < interval {
                    thread::sleep(interval - elapsed);
                }
            }

            if let Err(e) = csv_writer.flush() {
                error!("Failed to flush final metrics: {}", e);
            } else {
                info!("System tracker stopped, final metrics flushed");
            }
        });

        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        {
            let mut running = self.running.lock().unwrap();
            if !*running {
                return Err(anyhow::anyhow!("System tracker is not running"));
            }
            *running = false;
        }

        info!("System tracker stop requested");
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        *self.running.lock().unwrap()
    }
}

fn collect_system_metrics(pid: u32, last_disk_io: u64) -> Result<(f64, u64, u64)> {
    let cpu_percent = get_cpu_usage(pid)?;
    let memory_bytes = get_memory_usage(pid)?;
    let current_disk_io = get_disk_io(pid)?;

    let _disk_io_delta = if current_disk_io >= last_disk_io {
        current_disk_io - last_disk_io
    } else {
        current_disk_io
    };

    Ok((cpu_percent, memory_bytes, current_disk_io))
}

#[cfg(target_os = "macos")]
fn get_cpu_usage(pid: u32) -> Result<f64> {
    use std::process::Command;

    let output = Command::new("ps")
        .args(["-o", "%cpu=", "-p", &pid.to_string()])
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!("ps command failed"));
    }

    let cpu_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    cpu_str
        .parse::<f64>()
        .map_err(|e| anyhow::anyhow!("Failed to parse CPU usage: {}", e))
}

#[cfg(target_os = "macos")]
fn get_memory_usage(pid: u32) -> Result<u64> {
    use std::process::Command;

    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!("ps command failed"));
    }

    let rss_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let rss_kb: u64 = rss_str
        .parse()
        .map_err(|e| anyhow::anyhow!("Failed to parse RSS: {}", e))?;

    Ok(rss_kb * 1024)
}

#[cfg(target_os = "macos")]
fn get_disk_io(pid: u32) -> Result<u64> {
    use std::process::Command;

    let output = Command::new("ps")
        .args(["-o", "pid,comm", "-p", &pid.to_string()])
        .output()?;

    if !output.status.success() {
        return Ok(0);
    }

    Ok(0)
}

fn is_process_running(pid: u32) -> bool {
    use std::process::Command;

    Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn create_csv_writer(zip_path: &PathBuf) -> Result<csv::Writer<ZipWriter<BufWriter<File>>>> {
    let file = if zip_path.exists() {
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(zip_path)?
    } else {
        File::create(zip_path)?
    };

    let mut zip = ZipWriter::new(BufWriter::new(file));
    let options = FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("system-metrics.csv", options)?;

    let writer = csv::Writer::from_writer(zip);
    Ok(writer)
}
