use std::cmp::Ordering;
use std::fs;
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use clap::{Args, Subcommand};
use regex::Regex;

#[derive(Subcommand, Debug)]
pub enum LogCommand {
    /// List available daemon log files.
    #[command(alias = "ls")]
    List(ListArgs),
    /// Show the contents of one or more log files.
    Show(ShowArgs),
    /// Filter recent log files by a regex pattern.
    Filter(FilterArgs),
}

#[derive(Args, Debug, Clone, Copy)]
pub struct ListArgs {
    /// Show latest entries first and label them with negative indices.
    #[arg(short, long)]
    pub reverse: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    /// Comma separated indexes or ranges (e.g. 0,2,3 or 1-(-1)).
    #[arg(value_name = "selection")]
    pub selection: String,
    /// Print directly instead of attempting to launch a pager.
    #[arg(long)]
    pub no_pager: bool,
}

#[derive(Args, Debug, Clone)]
pub struct FilterArgs {
    /// Regex used to filter log lines.
    pub pattern: String,
    /// How many hours of logs to consider (default: 24).
    #[arg(short = 'H', long = "hours", default_value_t = 24)]
    pub hours: u64,
}

#[derive(Clone, Debug)]
struct LogEntry {
    index: usize,
    path: PathBuf,
    modified: SystemTime,
    size: u64,
}

pub fn execute(action: LogCommand, workspace_dir: &Path) -> Result<()> {
    let log_dir = workspace_dir.join("logs");
    match action {
        LogCommand::List(args) => list_logs(&log_dir, args),
        LogCommand::Show(args) => show_logs(&log_dir, &args),
        LogCommand::Filter(args) => filter_logs(&log_dir, &args),
    }
}

fn list_logs(log_dir: &Path, args: ListArgs) -> Result<()> {
    let mut entries = collect_log_entries(log_dir)?;

    if entries.is_empty() {
        println!("No log files under {}", log_dir.display());
        return Ok(());
    }

    if args.reverse {
        entries.reverse();
    }

    let total = entries.len();

    for (position, entry) in entries.iter().enumerate() {
        let raw_index = if args.reverse {
            -(position as isize + 1)
        } else {
            entry.index as isize
        };

        let modified: DateTime<Local> = entry.modified.into();
        let label = format!("{:>4}", raw_index);

        println!(
            "{}  {}  {:>7}  {}",
            label,
            modified.format("%Y-%m-%d %H:%M:%S"),
            human_size(entry.size),
            entry.path.display()
        );
    }

    if args.reverse && total > 0 {
        let newest_negative = -1;
        let newest_positive = total - 1;
        println!(
            "(latest log: index {} or {})",
            newest_positive, newest_negative
        );
    }

    Ok(())
}

fn show_logs(log_dir: &Path, args: &ShowArgs) -> Result<()> {
    let entries = collect_log_entries(log_dir)?;
    if entries.is_empty() {
        println!("No log files under {}", log_dir.display());
        return Ok(());
    }

    let selected = parse_selection(&args.selection, entries.len())?;

    if selected.is_empty() {
        println!("No matching log indexes");
        return Ok(());
    }

    let selected_entries: Vec<_> = selected
        .into_iter()
        .map(|idx| entries[idx].clone())
        .collect();

    let use_pager = !args.no_pager && io::stdout().is_terminal();
    if use_pager {
        match run_less(&selected_entries) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                eprintln!("less not found; printing to stdout instead");
            }
            Err(e) => {
                return Err(anyhow!("failed to launch pager: {}", e));
            }
        }
    }

    let mut stdout = io::stdout().lock();
    for (i, entry) in selected_entries.iter().enumerate() {
        if i > 0 {
            writeln!(stdout)?;
        }
        writeln!(
            stdout,
            "===== [{}|-{}] {} =====",
            entry.index,
            neg_index(entry.index, entries.len()),
            entry.path.display()
        )?;
        let mut file =
            File::open(&entry.path).with_context(|| format!("opening {}", entry.path.display()))?;
        let metadata = file.metadata()?;
        let mut ends_with_newline = true;
        if metadata.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            ends_with_newline = last[0] == b'\n';
            file.seek(SeekFrom::Start(0))?;
        }
        io::copy(&mut file, &mut stdout)?;
        if !ends_with_newline {
            writeln!(stdout)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn filter_logs(log_dir: &Path, args: &FilterArgs) -> Result<()> {
    let mut entries = collect_log_entries(log_dir)?;
    if entries.is_empty() {
        println!("No log files under {}", log_dir.display());
        return Ok(());
    }

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(args.hours.saturating_mul(3600)))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    entries.retain(|entry| entry.modified >= cutoff);

    if entries.is_empty() {
        println!("No log files from the last {} hours", args.hours);
        return Ok(());
    }

    if try_rg(&args.pattern, &entries)? {
        return Ok(());
    }
    if try_grep(&args.pattern, &entries)? {
        return Ok(());
    }

    filter_with_regex(&args.pattern, &entries)
}

fn collect_log_entries(log_dir: &Path) -> Result<Vec<LogEntry>> {
    if !log_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(log_dir)? {
        let entry = entry?;
        let metadata = match entry.metadata() {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }
        let raw_path = entry.path();
        let canonical = raw_path.canonicalize().unwrap_or(raw_path.clone());
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let size = metadata.len();
        entries.push(LogEntry {
            index: 0,
            path: canonical,
            modified,
            size,
        });
    }

    entries.sort_by(|a, b| match a.modified.cmp(&b.modified) {
        Ordering::Equal => a.path.cmp(&b.path),
        ord => ord,
    });

    for (idx, entry) in entries.iter_mut().enumerate() {
        entry.index = idx;
    }

    Ok(entries)
}

fn parse_selection(selection: &str, len: usize) -> Result<Vec<usize>> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for token in selection
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if let Some((left, right)) = split_range(token) {
            let start = resolve_index(left, len)?;
            let end = resolve_index(right, len)?;
            if start <= end {
                for idx in start..=end {
                    push_unique(&mut result, idx);
                }
            } else {
                for idx in (end..=start).rev() {
                    push_unique(&mut result, idx);
                }
            }
        } else {
            let idx = resolve_index(token, len)?;
            push_unique(&mut result, idx);
        }
    }
    Ok(result)
}

fn split_range(token: &str) -> Option<(&str, &str)> {
    let trimmed = token.trim();
    let mut depth: i32 = 0;
    for (i, ch) in trimmed.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '-' if depth == 0 && i != 0 => {
                let (left, rest) = trimmed.split_at(i);
                let right = rest.get(1..)?;
                return Some((left.trim(), right.trim()));
            }
            _ => {}
        }
    }
    None
}

fn resolve_index(token: &str, len: usize) -> Result<usize> {
    if len == 0 {
        return Err(anyhow!("no log files available"));
    }
    let trimmed = trim_wrapping_parens(token.trim());
    let value: isize = trimmed
        .parse()
        .with_context(|| format!("invalid index '{}'", trimmed))?;
    normalize_index(value, len)
}

fn trim_wrapping_parens(mut value: &str) -> &str {
    loop {
        let trimmed = value.trim();
        if trimmed.starts_with('(') && trimmed.ends_with(')') && trimmed.len() > 2 {
            value = &trimmed[1..trimmed.len() - 1];
            continue;
        }
        return trimmed;
    }
}

fn normalize_index(value: isize, len: usize) -> Result<usize> {
    if value >= 0 {
        let idx = value as usize;
        if idx >= len {
            Err(anyhow!("index {} out of range (0..{})", idx, len - 1))
        } else {
            Ok(idx)
        }
    } else {
        let len_isize = len as isize;
        let idx = len_isize + value;
        if idx < 0 {
            Err(anyhow!("index {} out of range (available {})", value, len))
        } else {
            Ok(idx as usize)
        }
    }
}

fn push_unique(vec: &mut Vec<usize>, value: usize) {
    if !vec.contains(&value) {
        vec.push(value);
    }
}

fn neg_index(index: usize, len: usize) -> isize {
    index as isize - len as isize
}

fn run_less(entries: &[LogEntry]) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let status = Command::new("less")
        .arg("-R")
        .arg("--")
        .args(entries.iter().map(|entry| entry.path.as_os_str()))
        .status()?;
    if status.success() { Ok(()) } else { Ok(()) }
}

fn try_rg(pattern: &str, entries: &[LogEntry]) -> Result<bool> {
    if entries.is_empty() {
        return Ok(true);
    }
    let mut command = Command::new("rg");
    command.arg("--color=never");
    command.arg(pattern);
    command.arg("--");
    for entry in entries {
        command.arg(&entry.path);
    }
    match command.status() {
        Ok(status) => {
            if status.success() || status.code() == Some(1) {
                Ok(true)
            } else {
                Err(anyhow!("rg exited with status {:?}", status.code()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!(e)),
    }
}

fn try_grep(pattern: &str, entries: &[LogEntry]) -> Result<bool> {
    if entries.is_empty() {
        return Ok(true);
    }
    let mut command = Command::new("grep");
    command.arg("-E");
    command.arg(pattern);
    command.arg("--");
    for entry in entries {
        command.arg(&entry.path);
    }
    match command.status() {
        Ok(status) => {
            if status.success() || status.code() == Some(1) {
                Ok(true)
            } else {
                Err(anyhow!("grep exited with status {:?}", status.code()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!(e)),
    }
}

fn filter_with_regex(pattern: &str, entries: &[LogEntry]) -> Result<()> {
    let regex = Regex::new(pattern)?;
    let mut found = false;
    for entry in entries {
        let file =
            File::open(&entry.path).with_context(|| format!("opening {}", entry.path.display()))?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if regex.is_match(&line) {
                println!("{}:{}", entry.path.display(), line);
                found = true;
            }
        }
    }
    if !found {
        println!("No matches found");
    }
    Ok(())
}

fn human_size(size: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    if size < 1024 {
        return format!("{}B", size);
    }
    let size_f = size as f64;
    if size_f < MB {
        format!("{:.1}K", size_f / KB)
    } else if size_f < GB {
        format!("{:.1}M", size_f / MB)
    } else {
        format!("{:.1}G", size_f / GB)
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_index, parse_selection, split_range, trim_wrapping_parens};

    #[test]
    fn trim_parens_strips_outer_pairs() {
        assert_eq!(trim_wrapping_parens("(1)"), "1");
        assert_eq!(trim_wrapping_parens("((1))"), "1");
        assert_eq!(trim_wrapping_parens("( 1 )"), "1");
        assert_eq!(trim_wrapping_parens("1"), "1");
    }

    #[test]
    fn split_range_identifies_delimiter() {
        assert_eq!(split_range("0-5"), Some(("0", "5")));
        assert_eq!(split_range("1-(-1)"), Some(("1", "(-1)")));
        assert_eq!(split_range("-1"), None);
    }

    #[test]
    fn normalize_positive_and_negative() {
        assert_eq!(normalize_index(0, 5).unwrap(), 0);
        assert_eq!(normalize_index(-1, 5).unwrap(), 4);
        assert!(normalize_index(5, 5).is_err());
        assert!(normalize_index(-6, 5).is_err());
    }

    #[test]
    fn parse_selection_handles_ranges_and_lists() {
        let vec = parse_selection("0,2,3", 5).unwrap();
        assert_eq!(vec, vec![0, 2, 3]);

        let vec = parse_selection("1-3", 5).unwrap();
        assert_eq!(vec, vec![1, 2, 3]);

        let vec = parse_selection("1-(-1)", 5).unwrap();
        assert_eq!(vec, vec![1, 2, 3, 4]);

        let vec = parse_selection("0-(-4)", 6).unwrap();
        assert_eq!(vec, vec![0, 1, 2]);
    }

    #[test]
    fn parse_selection_handles_descending_and_duplicates() {
        let vec = parse_selection("3-1", 5).unwrap();
        assert_eq!(vec, vec![3, 2, 1]);

        let vec = parse_selection("0,1,1,-1", 4).unwrap();
        assert_eq!(vec, vec![0, 1, 3]);
    }

    #[test]
    fn parse_selection_errors_when_out_of_bounds() {
        assert!(parse_selection("5", 3).is_err());
        assert!(parse_selection("-7", 3).is_err());
        assert!(parse_selection("2-7", 4).is_err());
    }
}
