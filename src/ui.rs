use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Wrap},
};

use crate::app::{App, InputMode, LibraryRow};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HitTarget {
    Timeline { area: Rect },
    Volume { area: Rect },
}

pub fn render(frame: &mut Frame, app: &App) {
    let layout = compute_layout(frame.size());

    render_library(frame, app, layout.library);
    if let Some(details) = layout.details {
        render_details(frame, app, details);
    }
    render_transport(frame, app, layout.transport);
    render_status(frame, app, layout.status);

    if !matches!(app.input_mode, InputMode::Normal) {
        render_input_dialog(frame, app);
    }
}

fn render_library(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.visible_rows();
    let items = if rows.is_empty() {
        let text = if app.filter_query.is_empty() {
            "No audiobooks found. Press 'a' to add a directory."
        } else {
            "No matches for the current filter."
        };
        vec![ListItem::new(text)]
    } else {
        rows.into_iter()
            .map(|row| match row {
                LibraryRow::GroupHeader { title, count } => {
                    ListItem::new(Line::from(vec![Span::styled(
                        format!(
                            "{} ({})",
                            truncate_middle(&title, area.width.saturating_sub(8) as usize),
                            count
                        ),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )]))
                }
                LibraryRow::Item { item_index } => {
                    let item = &app.library_items[item_index];
                    let line = Line::from(vec![
                        Span::raw("  "),
                        Span::styled(
                            truncate_middle(&item.title, area.width.saturating_sub(12) as usize),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!("  .{}", item.extension)),
                    ]);
                    ListItem::new(line)
                }
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.library_title()),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    let mut state = app.list_state.clone();
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_details(frame: &mut Frame, app: &App, area: Rect) {
    let body = if let Some(item) = app.selected_item() {
        let resume = app.resume_label(item.path.as_path());
        let duration = app
            .current_duration()
            .map(format_duration)
            .unwrap_or_else(|| "Unknown".to_owned());
        let root = app
            .library_root_for_path(item.path.as_path())
            .unwrap_or("Unknown root");
        let details_width = area.width.saturating_sub(10) as usize;

        vec![
            Line::from(vec![
                Span::styled("Title: ", Style::default().fg(Color::Yellow)),
                Span::raw(item.title.clone()),
            ]),
            Line::from(vec![
                Span::styled("Root: ", Style::default().fg(Color::Yellow)),
                Span::raw(truncate_middle(root, details_width)),
            ]),
            Line::from(vec![
                Span::styled("Path: ", Style::default().fg(Color::Yellow)),
                Span::raw(truncate_middle(
                    &item.path.display().to_string(),
                    details_width,
                )),
            ]),
            Line::from(vec![
                Span::styled("Format: ", Style::default().fg(Color::Yellow)),
                Span::raw(item.extension.clone()),
            ]),
            Line::from(vec![
                Span::styled("Source: ", Style::default().fg(Color::Yellow)),
                Span::raw(if item.metadata_title.is_some() {
                    "metadata".to_owned()
                } else {
                    "filename".to_owned()
                }),
            ]),
            Line::from(vec![
                Span::styled("Duration: ", Style::default().fg(Color::Yellow)),
                Span::raw(duration),
            ]),
            Line::from(vec![
                Span::styled("Resume: ", Style::default().fg(Color::Yellow)),
                Span::raw(resume),
            ]),
        ]
    } else {
        vec![Line::raw("Add a directory to start building your library.")]
    };

    let paragraph = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title("Details"))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_transport(frame: &mut Frame, app: &App, area: Rect) {
    let sections = transport_sections(area);
    let snapshot = app.playback_snapshot();

    let timeline_ratio = snapshot
        .as_ref()
        .and_then(|snapshot| {
            snapshot
                .duration
                .map(|duration| ratio(snapshot.position, duration))
        })
        .unwrap_or(0.0);
    let timeline_label = snapshot
        .as_ref()
        .map(|snapshot| {
            let now = format_duration(snapshot.position);
            let total = snapshot
                .duration
                .map(format_duration)
                .unwrap_or_else(|| "Unknown".to_owned());
            format!("{now} / {total}")
        })
        .unwrap_or_else(|| "No active file".to_owned());
    let timeline_title = snapshot
        .as_ref()
        .map(|snapshot| {
            if snapshot.is_paused {
                "Timeline | Paused"
            } else {
                "Timeline | Playing"
            }
        })
        .unwrap_or("Timeline");

    let timeline = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(timeline_title))
        .gauge_style(Style::default().fg(Color::Cyan))
        .ratio(timeline_ratio)
        .label(timeline_label)
        .use_unicode(true);
    frame.render_widget(timeline, sections.timeline);

    let volume_value = snapshot
        .as_ref()
        .map(|snapshot| snapshot.volume)
        .unwrap_or_else(|| app.player.volume());
    let volume_ratio = volume_value as f64 / 100.0;
    let volume = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Volume"))
        .gauge_style(Style::default().fg(Color::Green))
        .ratio(volume_ratio)
        .label(format!("{}%", volume_value))
        .use_unicode(true);
    frame.render_widget(volume, sections.volume);

    if let Some(help_area) = sections.help {
        let help = Paragraph::new(Line::from(vec![
            Span::raw("a add dir  "),
            Span::raw("/ filter  "),
            Span::raw("s sort  "),
            Span::raw("e seek by  "),
            Span::raw("d drop root  "),
            Span::raw("Enter play  "),
            Span::raw("Space pause  "),
            Span::raw("q quit"),
        ]));
        frame.render_widget(help, help_area);
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let style = if app.idle_paused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

    frame.render_widget(Paragraph::new(app.status_line()).style(style), area);
}

fn render_input_dialog(frame: &mut Frame, app: &App) {
    let popup_height = if frame.size().height < 12 { 5 } else { 7 };
    let popup = centered_rect(70, popup_height, frame.size());
    frame.render_widget(Clear, popup);

    let display = with_cursor(&app.input_buffer, app.input_cursor);
    let input = Paragraph::new(display)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(app.input_title())
                .border_style(Style::default().fg(Color::Magenta)),
        )
        .wrap(Wrap { trim: false });

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(popup);

    frame.render_widget(input, sections[0]);
    if popup_height >= 7 {
        frame.render_widget(Paragraph::new(app.input_help()), sections[1]);
    }
}

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(height),
            Constraint::Min(1),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn ratio(position: Duration, duration: Duration) -> f64 {
    if duration.is_zero() {
        return 0.0;
    }

    (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
}

pub fn hit_test(area: Rect, column: u16, row: u16) -> Option<HitTarget> {
    let layout = compute_layout(area);
    let sections = transport_sections(layout.transport);

    if contains(sections.timeline, column, row) {
        return Some(HitTarget::Timeline {
            area: sections.timeline,
        });
    }

    if contains(sections.volume, column, row) {
        return Some(HitTarget::Volume {
            area: sections.volume,
        });
    }

    None
}

pub fn ratio_from_gauge_click(area: Rect, column: u16) -> f64 {
    let left = area.x.saturating_add(1);
    let right = area.right().saturating_sub(2);

    if right <= left {
        return 0.0;
    }

    let clamped = column.clamp(left, right);
    let span = (right - left) as f64;
    let offset = (clamped - left) as f64;
    (offset / span).clamp(0.0, 1.0)
}

#[derive(Clone, Copy)]
struct UiLayout {
    library: Rect,
    details: Option<Rect>,
    transport: Rect,
    status: Rect,
}

fn compute_layout(area: Rect) -> UiLayout {
    let transport_height = if area.height >= 18 { 7 } else { 6 };
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Length(transport_height),
            Constraint::Length(2),
        ])
        .split(area);

    let show_details = layout[0].height >= 8 && area.width >= 100;
    if show_details {
        let top = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
            .split(layout[0]);

        UiLayout {
            library: top[0],
            details: Some(top[1]),
            transport: layout[1],
            status: layout[2],
        }
    } else {
        UiLayout {
            library: layout[0],
            details: None,
            transport: layout[1],
            status: layout[2],
        }
    }
}

struct TransportSections {
    timeline: Rect,
    volume: Rect,
    help: Option<Rect>,
}

fn transport_sections(area: Rect) -> TransportSections {
    if area.height >= 7 {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        return TransportSections {
            timeline: sections[0],
            volume: sections[1],
            help: Some(sections[2]),
        };
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3)])
        .split(area);

    TransportSections {
        timeline: sections[0],
        volume: sections[1],
        help: None,
    }
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

pub fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn truncate_middle(text: &str, max_len: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if max_len < 8 || chars.len() <= max_len {
        return text.to_owned();
    }

    let keep = (max_len.saturating_sub(1)) / 2;
    let start: String = chars.iter().take(keep).collect();
    let end: String = chars
        .iter()
        .rev()
        .take(keep)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{start}…{end}")
}

fn with_cursor(text: &str, cursor: usize) -> Line<'static> {
    let safe_cursor = cursor.min(text.len());
    let (left, right) = text.split_at(safe_cursor);
    let mut spans = Vec::new();
    spans.push(Span::raw(left.to_owned()));

    if let Some(ch) = right.chars().next() {
        spans.push(Span::styled(
            ch.to_string(),
            Style::default().bg(Color::White).fg(Color::Black),
        ));
        spans.push(Span::raw(right[ch.len_utf8()..].to_owned()));
    } else {
        spans.push(Span::styled(
            " ".to_owned(),
            Style::default().bg(Color::White).fg(Color::Black),
        ));
    }

    Line::from(spans)
}
