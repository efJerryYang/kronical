use crate::daemon::socket_server::{ActivityApp, MonitorResponse};
use anyhow::{Context, Result};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::path::PathBuf;

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut result = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for word in text.split_whitespace() {
        let word_len = word.chars().count();

        if current_width + word_len + if current_width > 0 { 1 } else { 0 } > max_width {
            if !current_line.is_empty() {
                result.push(current_line);
            }
            current_line = word.to_string();
            current_width = word_len;
        } else {
            if current_width > 0 {
                current_line.push(' ');
                current_width += 1;
            }
            current_line.push_str(word);
            current_width += word_len;
        }
    }

    if !current_line.is_empty() {
        result.push(current_line);
    }

    result
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

pub fn ui(frame: &mut Frame, response: &MonitorResponse) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(10)])
        .split(frame.area());

    draw_daemon_stats(frame, main_layout[0], response);
    draw_recent_activity(frame, main_layout[1], &response.recent_apps);
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

fn draw_daemon_stats(frame: &mut Frame, area: Rect, response: &MonitorResponse) {
    let block = Block::default().title("Daemon Stats").borders(Borders::ALL);

    let pid_file = PathBuf::from(&response.data_directory).join("chronicle.pid");
    let pid = read_pid_file(&pid_file)
        .unwrap_or_default()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Ensure state string is length 10
    let mut state = response.state_history.clone();
    if state.len() > 10 {
        state = state[state.len() - 10..].to_string();
    } else if state.len() < 10 {
        let pad = " ".repeat(10 - state.len());
        state = format!("{}{}", state, pad);
    }

    let text = vec![
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::Gray)),
            Span::styled(
                response.daemon_status.clone(),
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
            Span::styled("State: ", Style::default().fg(Color::Gray)),
            Span::styled(
                state,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_recent_activity(frame: &mut Frame, area: Rect, recent_apps: &[ActivityApp]) {
    let block = Block::default()
        .title(format!("Recent Activity (Top {} Apps)", recent_apps.len()))
        .borders(Borders::ALL);
    if recent_apps.is_empty() {
        let text = Paragraph::new("No recent activity data")
            .block(block)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        frame.render_widget(text, area);
        return;
    }

    // Use ~80% of width to reduce crowding
    let approx_width = ((area.width as f32) * 0.8) as usize;
    let content_width = approx_width.max(20);

    let items: Vec<ListItem> = recent_apps
        .iter()
        .flat_map(|app| {
            let mut app_items = vec![ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", app.app_name),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("[PID: {}; Start: {}]", app.pid, app.process_start_time),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!(" - {} total", app.total_duration_pretty)),
            ]))];

            let window_items: Vec<ListItem> = app
                .windows
                .iter()
                .flat_map(|window| {
                    let window_duration_pretty = pretty_format_duration(window.duration_seconds);
                    let mut window_lines = Vec::new();

                    window_lines.push(ListItem::new(Line::from(vec![
                        Span::raw("  └─ "),
                        Span::styled(
                            format!(
                                "[wid: {}, duration: {}] ",
                                window.window_id, window_duration_pretty
                            ),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(
                            format!(
                                "[{}, {}]",
                                window
                                    .first_seen
                                    .with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M:%S"),
                                window
                                    .last_seen
                                    .with_timezone(&chrono::Local)
                                    .format("%Y-%m-%d %H:%M:%S")
                            ),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])));

                    // Wrapped window title with continuation marker
                    let wrap_width_first = content_width.saturating_sub(8); // account for prefix
                    let mut wrapped_title = wrap_text(&window.window_title, wrap_width_first);
                    let is_wrapped = wrapped_title.len() > 1;
                    if is_wrapped {
                        if let Some(first) = wrapped_title.first_mut() {
                            if first.len() < wrap_width_first {
                                first.push_str(" …");
                            }
                        }
                    }
                    for (i, line) in wrapped_title.iter().enumerate() {
                        let (prefix, color) = if window.is_group {
                            (if i == 0 { "      " } else { "      ↳ " }, Color::Magenta)
                        } else {
                            (if i == 0 { "      " } else { "      ↳ " }, Color::White)
                        };
                        window_lines.push(ListItem::new(Line::from(vec![
                            Span::raw(prefix),
                            Span::styled(line.clone(), Style::default().fg(color)),
                        ])));
                    }

                    window_lines
                })
                .collect();

            app_items.extend(window_items);
            app_items.push(ListItem::new(Line::from(" ")));
            app_items
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}
