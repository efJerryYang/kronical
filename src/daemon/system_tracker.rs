use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

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
            let mut metrics_buffer = Vec::new();
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

                        metrics_buffer.push(metrics);
                        batch_count += 1;

                        if batch_count % batch_size == 0 {
                            if let Err(e) = write_metrics_to_zip(&zip_path, &metrics_buffer) {
                                error!("Failed to write metrics batch to ZIP: {}", e);
                            } else {
                                debug!("Written {} metrics to ZIP file", metrics_buffer.len());
                                metrics_buffer.clear();
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

            if !metrics_buffer.is_empty() {
                if let Err(e) = write_metrics_to_zip(&zip_path, &metrics_buffer) {
                    error!("Failed to write final metrics batch: {}", e);
                } else {
                    info!("System tracker stopped, final metrics written successfully");
                }
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

fn write_metrics_to_zip(zip_path: &PathBuf, metrics_buffer: &[SystemMetrics]) -> Result<()> {
    let mut existing_metrics = Vec::new();

    if zip_path.exists() {
        existing_metrics = read_existing_metrics(zip_path)?;
    }

    existing_metrics.extend_from_slice(metrics_buffer);

    let file = File::create(zip_path)?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let options: FileOptions<()> =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("system-metrics.csv", options)?;

    let mut csv_writer = csv::Writer::from_writer(zip);

    for metric in &existing_metrics {
        csv_writer.serialize(metric)?;
    }

    csv_writer.flush()?;
    let zip_writer = match csv_writer.into_inner() {
        Ok(writer) => writer,
        Err(e) => return Err(anyhow::anyhow!("Failed to get inner ZIP writer: {}", e)),
    };

    zip_writer.finish()?;

    Ok(())
}

fn read_existing_metrics(zip_path: &PathBuf) -> Result<Vec<SystemMetrics>> {
    use std::io::Read;

    let file = File::open(zip_path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut csv_file = archive.by_name("system-metrics.csv")?;
    let mut contents = String::new();
    csv_file.read_to_string(&mut contents)?;

    let mut reader = csv::Reader::from_reader(contents.as_bytes());
    let mut metrics = Vec::new();

    for result in reader.deserialize() {
        let metric: SystemMetrics = result?;
        metrics.push(metric);
    }

    Ok(metrics)
}
