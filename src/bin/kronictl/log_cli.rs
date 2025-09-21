use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use clap::{Args, Subcommand};
use regex::Regex;

trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[OsString]) -> io::Result<ExitStatus>;
}

#[derive(Clone, Copy, Default)]
struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, program: &str, args: &[OsString]) -> io::Result<ExitStatus> {
        Command::new(program).args(args).status()
    }
}

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
    let stdout_handle = io::stdout();
    let stderr_handle = io::stderr();
    let stdout_is_terminal = stdout_handle.is_terminal();
    let mut stdout_lock = stdout_handle.lock();
    let mut stderr_lock = stderr_handle.lock();
    execute_with_runner(
        action,
        workspace_dir,
        &mut stdout_lock,
        &mut stderr_lock,
        &SystemCommandRunner,
        stdout_is_terminal,
    )
}

fn execute_with_runner(
    action: LogCommand,
    workspace_dir: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    runner: &dyn CommandRunner,
    stdout_is_terminal: bool,
) -> Result<()> {
    let log_dir = workspace_dir.join("logs");
    match action {
        LogCommand::List(args) => list_logs(&log_dir, args, stdout),
        LogCommand::Show(args) => {
            show_logs(&log_dir, &args, stdout, stderr, runner, stdout_is_terminal)
        }
        LogCommand::Filter(args) => filter_logs(&log_dir, &args, stdout, runner),
    }
}

fn list_logs(log_dir: &Path, args: ListArgs, stdout: &mut dyn Write) -> Result<()> {
    let mut entries = collect_log_entries(log_dir)?;

    if entries.is_empty() {
        writeln!(stdout, "No log files under {}", log_dir.display())?;
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

        writeln!(
            stdout,
            "{}  {}  {:>7}  {}",
            label,
            modified.format("%Y-%m-%d %H:%M:%S"),
            human_size(entry.size),
            entry.path.display()
        )?;
    }

    if args.reverse && total > 0 {
        let newest_negative = -1;
        let newest_positive = total - 1;
        writeln!(
            stdout,
            "(latest log: index {} or {})",
            newest_positive, newest_negative
        )?;
    }

    Ok(())
}

fn show_logs(
    log_dir: &Path,
    args: &ShowArgs,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    runner: &dyn CommandRunner,
    stdout_is_terminal: bool,
) -> Result<()> {
    let entries = collect_log_entries(log_dir)?;
    if entries.is_empty() {
        writeln!(stdout, "No log files under {}", log_dir.display())?;
        return Ok(());
    }

    let selected = parse_selection(&args.selection, entries.len())?;

    if selected.is_empty() {
        writeln!(stdout, "No matching log indexes")?;
        return Ok(());
    }

    let selected_entries: Vec<_> = selected
        .into_iter()
        .map(|idx| entries[idx].clone())
        .collect();

    let use_pager = !args.no_pager && stdout_is_terminal;
    if use_pager {
        match run_less(&selected_entries, runner) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                writeln!(stderr, "less not found; printing to stdout instead")?;
            }
            Err(e) => {
                return Err(anyhow!("failed to launch pager: {}", e));
            }
        }
    }

    for (i, entry) in selected_entries.iter().enumerate() {
        if i > 0 {
            writeln!(stdout)?;
        }
        writeln!(
            stdout,
            "===== [{}|{}] {} =====",
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
        io::copy(&mut file, stdout)?;
        if !ends_with_newline {
            writeln!(stdout)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn filter_logs(
    log_dir: &Path,
    args: &FilterArgs,
    stdout: &mut dyn Write,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let mut entries = collect_log_entries(log_dir)?;
    if entries.is_empty() {
        writeln!(stdout, "No log files under {}", log_dir.display())?;
        return Ok(());
    }

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(args.hours.saturating_mul(3600)))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    entries.retain(|entry| entry.modified >= cutoff);

    if entries.is_empty() {
        writeln!(stdout, "No log files from the last {} hours", args.hours)?;
        return Ok(());
    }

    if try_rg(&args.pattern, &entries, runner)? {
        return Ok(());
    }
    if try_grep(&args.pattern, &entries, runner)? {
        return Ok(());
    }

    filter_with_regex(&args.pattern, &entries, stdout)
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

fn run_less(entries: &[LogEntry], runner: &dyn CommandRunner) -> io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut args = Vec::with_capacity(2 + entries.len());
    args.push(OsString::from("-R"));
    args.push(OsString::from("--"));
    for entry in entries {
        args.push(entry.path.as_os_str().to_os_string());
    }
    let status = runner.run("less", &args)?;
    if status.success() { Ok(()) } else { Ok(()) }
}

fn try_rg(pattern: &str, entries: &[LogEntry], runner: &dyn CommandRunner) -> Result<bool> {
    if entries.is_empty() {
        return Ok(true);
    }
    let mut args = Vec::with_capacity(entries.len() + 3);
    args.push(OsString::from("--color=never"));
    args.push(OsString::from(pattern));
    args.push(OsString::from("--"));
    for entry in entries {
        args.push(entry.path.as_os_str().to_os_string());
    }
    match runner.run("rg", &args) {
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

fn try_grep(pattern: &str, entries: &[LogEntry], runner: &dyn CommandRunner) -> Result<bool> {
    if entries.is_empty() {
        return Ok(true);
    }
    let mut args = Vec::with_capacity(entries.len() + 3);
    args.push(OsString::from("-E"));
    args.push(OsString::from(pattern));
    args.push(OsString::from("--"));
    for entry in entries {
        args.push(entry.path.as_os_str().to_os_string());
    }
    match runner.run("grep", &args) {
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

fn filter_with_regex(pattern: &str, entries: &[LogEntry], stdout: &mut dyn Write) -> Result<()> {
    let regex = Regex::new(pattern)?;
    let mut found = false;
    for entry in entries {
        let file =
            File::open(&entry.path).with_context(|| format!("opening {}", entry.path.display()))?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if regex.is_match(&line) {
                writeln!(stdout, "{}:{}", entry.path.display(), line)?;
                found = true;
            }
        }
    }
    if !found {
        writeln!(stdout, "No matches found")?;
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
    use super::{CommandRunner, FilterArgs, ListArgs, LogCommand, ShowArgs};
    use mockall::{Sequence, mock};
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::io::Write;
    use std::path::Path;
    use std::process::ExitStatus;
    use tempfile::tempdir;

    mock! {
        pub CmdRunner {}
        impl CommandRunner for CmdRunner {
            fn run(&self, program: &str, args: &[OsString]) -> io::Result<ExitStatus>;
        }
    }

    type MockCommandRunner = MockCmdRunner;

    fn exit_status(code: i32) -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code)
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code as u32)
        }
    }

    fn write_log(workspace: &Path, name: &str, contents: &str) {
        let logs = workspace.join("logs");
        fs::create_dir_all(&logs).expect("create logs directory");
        fs::write(logs.join(name), contents).expect("write log");
    }

    use super::{
        SystemCommandRunner, normalize_index, parse_selection, split_range, trim_wrapping_parens,
    };

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

    #[test]
    fn human_size_formats_units() {
        assert_eq!(super::human_size(0), "0B");
        assert_eq!(super::human_size(512), "512B");
        assert_eq!(super::human_size(2048), "2.0K");
        assert_eq!(super::human_size(5 * 1024 * 1024), "5.0M");
        assert_eq!(super::human_size(3 * 1024 * 1024 * 1024), "3.0G");
    }

    #[test]
    fn run_less_honors_path_env() {
        use std::env;
        use std::time::SystemTime;

        let tmp = tempdir().expect("tempdir");
        let bin_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let less_path = bin_dir.join("less");
        let mut script = std::fs::File::create(&less_path).expect("create script");
        writeln!(script, "#!/bin/sh\nexit 0").expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&less_path)
                .expect("metadata")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&less_path, perms).expect("permissions");
        }

        let log_path = tmp.path().join("log.txt");
        std::fs::write(&log_path, "contents").expect("log content");
        let entry = super::LogEntry {
            index: 0,
            path: log_path,
            modified: SystemTime::now(),
            size: 8,
        };

        let saved_path = env::var("PATH").ok();
        unsafe { env::set_var("PATH", &bin_dir) };
        let result = super::run_less(&[entry], &SystemCommandRunner);
        if let Some(saved) = saved_path {
            unsafe { env::set_var("PATH", saved) };
        } else {
            unsafe { env::remove_var("PATH") };
        }
        assert!(result.is_ok());
    }

    #[test]
    fn execute_routes_to_list_logs() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "hello\n");

        let result = super::execute(
            LogCommand::List(ListArgs { reverse: false }),
            workspace.path(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn list_logs_prints_entries_and_reverse() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "first\n");
        write_log(workspace.path(), "kronid-002.log", "second\n");

        let mut output = Vec::new();
        super::list_logs(
            &workspace.path().join("logs"),
            ListArgs { reverse: false },
            &mut output,
        )
        .expect("list forward");
        let out = String::from_utf8(output).expect("utf8");
        assert!(out.contains("   0"));
        assert!(out.contains("   1"));

        let mut reverse_output = Vec::new();
        super::list_logs(
            &workspace.path().join("logs"),
            ListArgs { reverse: true },
            &mut reverse_output,
        )
        .expect("list reverse");
        let out_rev = String::from_utf8(reverse_output).expect("utf8");
        assert!(out_rev.contains(" -1"));
        assert!(out_rev.contains("(latest log: index 1 or -1)"));
    }

    #[test]
    fn show_logs_without_pager_prints_selection() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "hello\nworld\n");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut runner = MockCommandRunner::new();
        runner.expect_run().never();

        super::show_logs(
            &workspace.path().join("logs"),
            &ShowArgs {
                selection: "0".to_string(),
                no_pager: true,
            },
            &mut stdout,
            &mut stderr,
            &runner,
            false,
        )
        .expect("show logs");

        assert!(stderr.is_empty());
        let out = String::from_utf8(stdout).expect("utf8");
        assert!(out.contains("===== [0|-1]"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn show_logs_uses_less_when_terminal() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "data\n");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .withf(|program, args| {
                program == "less" && args.len() == 3 && args[0] == "-R" && args[1] == "--"
            })
            .returning(|_, _| Ok(exit_status(0)));

        super::show_logs(
            &workspace.path().join("logs"),
            &ShowArgs {
                selection: "0".to_string(),
                no_pager: false,
            },
            &mut stdout,
            &mut stderr,
            &runner,
            true,
        )
        .expect("pager");

        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn show_logs_falls_back_when_less_missing() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "fallback\n");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .returning(|_, _| Err(io::Error::new(io::ErrorKind::NotFound, "less")));

        super::show_logs(
            &workspace.path().join("logs"),
            &ShowArgs {
                selection: "0".to_string(),
                no_pager: false,
            },
            &mut stdout,
            &mut stderr,
            &runner,
            true,
        )
        .expect("fallback");

        let err = String::from_utf8(stderr).expect("utf8");
        assert!(err.contains("less not found"));
        let out = String::from_utf8(stdout).expect("utf8");
        assert!(out.contains("fallback"));
    }

    #[test]
    fn show_logs_propagates_pager_errors() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "data\n");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .returning(|_, _| Err(io::Error::new(io::ErrorKind::Other, "boom")));

        let err = super::show_logs(
            &workspace.path().join("logs"),
            &ShowArgs {
                selection: "0".to_string(),
                no_pager: false,
            },
            &mut stdout,
            &mut stderr,
            &runner,
            true,
        )
        .expect_err("pager error");

        assert!(err.to_string().contains("failed to launch pager"));
    }

    #[test]
    fn filter_logs_prefers_rg() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "match line\n");

        let mut stdout = Vec::new();
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .withf(|program, _| program == "rg")
            .returning(|_, _| Ok(exit_status(0)));

        super::filter_logs(
            &workspace.path().join("logs"),
            &FilterArgs {
                pattern: "match".to_string(),
                hours: 24,
            },
            &mut stdout,
            &runner,
        )
        .expect("rg path");
        assert!(stdout.is_empty());
    }

    #[test]
    fn filter_logs_uses_grep_when_rg_missing() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "another line\n");

        let mut stdout = Vec::new();
        let mut runner = MockCommandRunner::new();
        let mut seq = Sequence::new();
        runner
            .expect_run()
            .times(1)
            .in_sequence(&mut seq)
            .withf(|program, _| program == "rg")
            .returning(|_, _| Err(io::Error::new(io::ErrorKind::NotFound, "rg")));
        runner
            .expect_run()
            .times(1)
            .in_sequence(&mut seq)
            .withf(|program, _| program == "grep")
            .returning(|_, _| Ok(exit_status(0)));

        super::filter_logs(
            &workspace.path().join("logs"),
            &FilterArgs {
                pattern: "another".to_string(),
                hours: 24,
            },
            &mut stdout,
            &runner,
        )
        .expect("grep fallback");
        assert!(stdout.is_empty());
    }

    #[test]
    fn filter_logs_uses_regex_when_tools_missing() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "alpha\n");

        let mut stdout = Vec::new();
        let mut runner = MockCommandRunner::new();
        let mut seq = Sequence::new();
        runner
            .expect_run()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Err(io::Error::new(io::ErrorKind::NotFound, "rg")));
        runner
            .expect_run()
            .times(1)
            .in_sequence(&mut seq)
            .returning(|_, _| Err(io::Error::new(io::ErrorKind::NotFound, "grep")));

        super::filter_logs(
            &workspace.path().join("logs"),
            &FilterArgs {
                pattern: "alpha".to_string(),
                hours: 24,
            },
            &mut stdout,
            &runner,
        )
        .expect("regex fallback");

        let out = String::from_utf8(stdout).expect("utf8");
        assert!(out.contains("alpha"));
    }

    #[test]
    fn filter_logs_reports_when_no_matches() {
        let workspace = tempdir().expect("tempdir");
        write_log(workspace.path(), "kronid-001.log", "beta\n");

        let mut stdout = Vec::new();
        let mut runner = MockCommandRunner::new();
        runner
            .expect_run()
            .times(2)
            .returning(|_, _| Err(io::Error::new(io::ErrorKind::NotFound, "tool")));

        super::filter_logs(
            &workspace.path().join("logs"),
            &FilterArgs {
                pattern: "gamma".to_string(),
                hours: 24,
            },
            &mut stdout,
            &runner,
        )
        .expect("regex fallback");

        let out = String::from_utf8(stdout).expect("utf8");
        assert!(out.contains("No matches found"));
    }
}
