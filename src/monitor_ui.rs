use crate::socket_server::{ActivityApp, MonitorResponse};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};

pub fn ui(frame: &mut Frame, response: &MonitorResponse) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Min(10),
        ])
        .split(frame.area());

    draw_daemon_stats(frame, main_layout[0], response);
    draw_compression_stats(frame, main_layout[1], response);
    draw_memory_stats(frame, main_layout[2], response);
    draw_recent_activity(frame, main_layout[3], &response.recent_apps);
}

fn draw_daemon_stats(frame: &mut Frame, area: Rect, response: &MonitorResponse) {
    let block = Block::default().title("Daemon Stats").borders(Borders::ALL);
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
            Span::styled("Memory: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{:.4} MB", response.memory_mb)),
        ]),
        Line::from(vec![
            Span::styled("CPU:    ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{:.4}%", response.cpu_percent)),
        ]),
    ];
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_compression_stats(frame: &mut Frame, area: Rect, response: &MonitorResponse) {
    let stats = &response.compression_stats;
    let block = Block::default()
        .title("Event Compression")
        .borders(Borders::ALL);
    let text = vec![
        Line::from(vec![
            Span::styled("Raw/Compact Events: ", Style::default().fg(Color::Gray)),
            Span::raw(format!(
                "{} / {} ",
                stats.events_in_ring_buffer, stats.compact_events_stored
            )),
            Span::styled(
                format!("({:.2}x ratio)", stats.event_compression_ratio),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("Raw/Compact Memory: ", Style::default().fg(Color::Gray)),
            Span::raw(format!(
                "{:.2}MB / {:.2}MB ",
                stats.raw_events_memory_mb, stats.compact_events_memory_mb
            )),
            Span::styled(
                format!("({:.2}x ratio)", stats.memory_compression_ratio),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::styled("String Table Size:  ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{} KB", stats.string_table_size_kb)),
        ]),
    ];
    let paragraph = Paragraph::new(text).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_memory_stats(frame: &mut Frame, area: Rect, response: &MonitorResponse) {
    let stats = &response.compression_stats;
    let block = Block::default()
        .title("Data Structure Memory")
        .borders(Borders::ALL);
    let text = vec![
        Line::from(vec![
            Span::styled("Compact Events: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{:.4} MB", stats.compact_events_memory_mb)),
        ]),
        Line::from(vec![
            Span::styled("Records:        ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{:.4} MB", stats.records_collection_memory_mb)),
        ]),
        Line::from(vec![
            Span::styled("Data File:      ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{:.4} MB", response.data_file_size_mb)),
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
                .map(|window| {
                    ListItem::new(Line::from(vec![
                        Span::raw("  └─ "),
                        Span::styled(
                            format!("[{}] ", window.window_id),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(
                            format!("{} ", window.window_title),
                            Style::default().fg(Color::White),
                        ),
                        Span::styled(
                            format!("[{}]", window.last_active),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();

            app_items.extend(window_items);
            app_items.push(ListItem::new(Line::from(" "))); // spacer
            app_items
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}
