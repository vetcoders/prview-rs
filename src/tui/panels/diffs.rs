//! Diffs panel - displays file changes and diff content.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::tui::types::{FileChangeKind, TuiState};

/// Draw the diffs panel
pub fn draw(f: &mut Frame, state: &TuiState, area: Rect) {
    // Split into stats, file list, and diff preview
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),      // Stats bar
            Constraint::Percentage(40), // File list
            Constraint::Percentage(50), // Diff preview
        ])
        .split(area);

    draw_diff_stats(f, state, chunks[0]);
    draw_file_list(f, state, chunks[1]);
    draw_diff_preview(f, state, chunks[2]);
}

/// Draw diff statistics bar
fn draw_diff_stats(f: &mut Frame, state: &TuiState, area: Rect) {
    let diffs = &state.diffs_state;

    let stats = Line::from(vec![
        Span::styled(" Commits: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            diffs.commits_count.to_string(),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  │  "),
        Span::styled(" Files: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            diffs.files.len().to_string(),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  │  "),
        Span::styled(
            format!("+{}", diffs.total_additions),
            Style::default().fg(Color::Green),
        ),
        Span::raw(" / "),
        Span::styled(
            format!("-{}", diffs.total_deletions),
            Style::default().fg(Color::Red),
        ),
    ]);

    let paragraph = Paragraph::new(stats).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" Summary "),
    );

    f.render_widget(paragraph, area);
}

/// Draw the list of changed files
fn draw_file_list(f: &mut Frame, state: &TuiState, area: Rect) {
    let files = &state.diffs_state.files;

    if files.is_empty() {
        let empty = Paragraph::new("No file changes. Run analysis to see diffs.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Changed Files "),
            );
        f.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .map(|(i, file)| {
            let is_selected = i == state.diffs_state.selected;

            // Change kind icon and color
            let (icon, icon_color) = match file.kind {
                FileChangeKind::Added => ("A", Color::Green),
                FileChangeKind::Modified => ("M", Color::Yellow),
                FileChangeKind::Deleted => ("D", Color::Red),
                FileChangeKind::Renamed => ("R", Color::Blue),
            };

            let line = Line::from(vec![
                Span::raw(if is_selected { "▶ " } else { "  " }),
                Span::styled(
                    format!("[{}]", icon),
                    Style::default().fg(icon_color).bold(),
                ),
                Span::raw(" "),
                Span::styled(
                    &file.path,
                    Style::default().fg(if is_selected {
                        Color::Cyan
                    } else {
                        Color::White
                    }),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("+{}", file.additions),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("/"),
                Span::styled(
                    format!("-{}", file.deletions),
                    Style::default().fg(Color::Red),
                ),
            ]);

            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(" Changed Files ({}) ", files.len())),
    );

    f.render_widget(list, area);
}

/// Draw diff preview for selected file
fn draw_diff_preview(f: &mut Frame, state: &TuiState, area: Rect) {
    let files = &state.diffs_state.files;

    let (title, content, scroll) = if files.is_empty() {
        (
            " Diff Preview ".to_string(),
            vec![Line::from("No file selected.")],
            0,
        )
    } else if let Some(file) = files.get(state.diffs_state.selected) {
        // If we have diff content stored, show it with syntax highlighting
        if !state.diffs_state.diff_content.is_empty() && state.diffs_state.show_diff {
            let total_lines = state.diffs_state.diff_content.lines().count();
            let scroll_pos = state.diffs_state.diff_scroll as usize;
            let title = format!(
                " {} [{}/{}] [j/k scroll] ",
                file.path,
                scroll_pos + 1,
                total_lines
            );

            let lines: Vec<Line> = state
                .diffs_state
                .diff_content
                .lines()
                .map(|line| {
                    let style = if line.starts_with('+') && !line.starts_with("+++") {
                        Style::default().fg(Color::Green)
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        Style::default().fg(Color::Red)
                    } else if line.starts_with("@@") {
                        Style::default().fg(Color::Cyan)
                    } else if line.starts_with("diff ") || line.starts_with("index ") {
                        Style::default().fg(Color::DarkGray)
                    } else {
                        Style::default()
                    };
                    Line::styled(line, style)
                })
                .collect();
            (title, lines, state.diffs_state.diff_scroll)
        } else {
            // Show placeholder with file info
            let title = format!(" {} ", file.path);
            let info = vec![
                Line::from(vec![
                    Span::styled("File: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(&file.path, Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::styled("Type: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        match file.kind {
                            FileChangeKind::Added => "Added",
                            FileChangeKind::Modified => "Modified",
                            FileChangeKind::Deleted => "Deleted",
                            FileChangeKind::Renamed => "Renamed",
                        },
                        Style::default().fg(Color::Yellow),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("Changes: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("+{}", file.additions),
                        Style::default().fg(Color::Green),
                    ),
                    Span::raw(" / "),
                    Span::styled(
                        format!("-{}", file.deletions),
                        Style::default().fg(Color::Red),
                    ),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Press [Enter] to view diff, [d] for full diff",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            (title, info, 0)
        }
    } else {
        (
            " Diff Preview ".to_string(),
            vec![Line::from("No file selected.")],
            0,
        )
    };

    let paragraph = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(title),
        )
        .scroll((scroll, 0));

    f.render_widget(paragraph, area);
}
