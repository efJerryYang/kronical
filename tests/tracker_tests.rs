use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use tempfile::tempdir;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::FileOptions};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemMetrics {
    pub timestamp: DateTime<Utc>,
    pub cpu_percent: f64,
    pub memory_bytes: u64,
    pub disk_io_bytes: u64,
}

fn write_metrics_to_zip_current(
    zip_path: &PathBuf,
    metrics_buffer: &[SystemMetrics],
) -> Result<()> {
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

fn write_metrics_to_zip_fixed(zip_path: &PathBuf, metrics_buffer: &[SystemMetrics]) -> Result<()> {
    let temp_path = zip_path.with_extension("tmp");

    let mut existing_metrics = Vec::new();
    if zip_path.exists() {
        existing_metrics = read_existing_metrics(zip_path)?;
    }

    existing_metrics.extend_from_slice(metrics_buffer);

    {
        let file = File::create(&temp_path)?;
        let mut zip = ZipWriter::new(BufWriter::new(file));
        let options: FileOptions<()> =
            FileOptions::default().compression_method(CompressionMethod::Deflated);

        zip.start_file("system-metrics.csv", options)?;

        {
            let mut csv_writer = csv::Writer::from_writer(&mut zip);
            for metric in &existing_metrics {
                csv_writer.serialize(metric)?;
            }
            csv_writer.flush()?;
        }

        zip.finish()?;
    }

    std::fs::rename(&temp_path, zip_path)?;
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

fn create_test_metrics(count: usize) -> Vec<SystemMetrics> {
    (0..count)
        .map(|i| SystemMetrics {
            timestamp: Utc::now(),
            cpu_percent: i as f64 * 0.1,
            memory_bytes: (i as u64) * 1000,
            disk_io_bytes: (i as u64) * 100,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_single_batch() {
        let temp_dir = tempdir().unwrap();
        let zip_path = temp_dir.path().join("test.zip");

        let metrics = create_test_metrics(5);

        write_metrics_to_zip_fixed(&zip_path, &metrics).unwrap();

        assert!(zip_path.exists());

        let read_metrics = read_existing_metrics(&zip_path).unwrap();
        assert_eq!(read_metrics.len(), 5);
        assert_eq!(read_metrics, metrics);
    }

    #[test]
    fn test_write_multiple_batches() {
        let temp_dir = tempdir().unwrap();
        let zip_path = temp_dir.path().join("test.zip");

        let batch1 = create_test_metrics(3);
        let batch2 = create_test_metrics(2);

        write_metrics_to_zip_fixed(&zip_path, &batch1).unwrap();

        let read_after_first = read_existing_metrics(&zip_path).unwrap();
        assert_eq!(read_after_first.len(), 3);

        write_metrics_to_zip_fixed(&zip_path, &batch2).unwrap();

        let read_after_second = read_existing_metrics(&zip_path).unwrap();
        assert_eq!(read_after_second.len(), 5);
    }

    #[test]
    fn test_zip_file_always_valid() {
        let temp_dir = tempdir().unwrap();
        let zip_path = temp_dir.path().join("test.zip");

        for i in 1..=10 {
            let metrics = create_test_metrics(i);
            write_metrics_to_zip_fixed(&zip_path, &metrics).unwrap();

            let read_metrics = read_existing_metrics(&zip_path).unwrap();
            assert!(
                !read_metrics.is_empty(),
                "Batch {} should produce readable zip",
                i
            );
        }
    }

    #[test]
    fn test_current_implementation_race_condition() {
        let temp_dir = tempdir().unwrap();
        let zip_path = temp_dir.path().join("test.zip");

        let metrics = create_test_metrics(3);
        write_metrics_to_zip_current(&zip_path, &metrics).unwrap();

        let file = File::create(&zip_path).unwrap();
        drop(file);

        let result = read_existing_metrics(&zip_path);
        assert!(result.is_err(), "Incomplete ZIP should fail to read");
    }

    #[test]
    fn test_empty_batch_handling() {
        let temp_dir = tempdir().unwrap();
        let zip_path = temp_dir.path().join("test.zip");

        let empty_metrics: Vec<SystemMetrics> = vec![];
        write_metrics_to_zip_fixed(&zip_path, &empty_metrics).unwrap();

        let read_metrics = read_existing_metrics(&zip_path).unwrap();
        assert_eq!(read_metrics.len(), 0);
    }
}
