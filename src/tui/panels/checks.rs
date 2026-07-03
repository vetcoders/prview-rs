//! Checks panel - displays check execution status and results.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::tui::types::{CheckLifecycle, TuiState};

/// Draw the checks panel
pub fn draw(f: &mut Frame, state: &TuiState, area: Rect) {
    // Split into list and detail areas
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(50), // Check list
            Constraint::Percentage(50), // Selected check output
        ])
        .split(area);

    draw_check_list(f, state, chunks[0]);
    draw_check_output(f, state, chunks[1]);
}

/// Draw the list of checks with their status
fn draw_check_list(f: &mut Frame, state: &TuiState, area: Rect) {
    let entries = &state.checks_state.entries;

    if entries.is_empty() {
        let empty = Paragraph::new("No checks configured. Press [r] to run analysis.")
            .style(Style::default().fg(Color::DarkGray))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Checks "),
            );
        f.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_selected = i == state.checks_state.selected;

            // Status icon with color
            let (icon, icon_color) = match entry.status {
                CheckLifecycle::Pending => ("○", Color::DarkGray),
                CheckLifecycle::Running => ("◐", Color::Yellow),
                CheckLifecycle::Completed => ("●", Color::Green),
                CheckLifecycle::Warned => ("⚠", Color::Yellow),
                CheckLifecycle::Failed => ("✗", Color::Red),
                CheckLifecycle::Skipped => ("○", Color::DarkGray),
                CheckLifecycle::Cached => ("●", Color::Blue),
            };

            // Duration display
            let duration = entry
                .duration
                .map(|d| format!("{:.1}s", d.as_secs_f32()))
                .unwrap_or_else(|| "-".to_string());

            // Status label
            let status_label = match entry.status {
                CheckLifecycle::Pending => "pending",
                CheckLifecycle::Running => "running",
                CheckLifecycle::Completed => "passed",
                CheckLifecycle::Warned => "warnings",
                CheckLifecycle::Failed => "failed",
                CheckLifecycle::Skipped => "skipped",
                CheckLifecycle::Cached => "cached",
            };

            let line = Line::from(vec![
                Span::raw(if is_selected { "▶ " } else { "  " }),
                Span::styled(icon, Style::default().fg(icon_color)),
                Span::raw(" "),
                Span::styled(
                    format!("{:<15}", entry.name),
                    Style::default().fg(if is_selected {
                        Color::Cyan
                    } else {
                        Color::White
                    }),
                ),
                Span::styled(
                    format!("[{}]", status_label),
                    Style::default().fg(icon_color),
                ),
                Span::raw("  "),
                Span::styled(duration, Style::default().fg(Color::DarkGray)),
            ]);

            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(
                " Checks ({}/{}) ",
                entries
                    .iter()
                    .filter(|e| e.status == CheckLifecycle::Completed
                        || e.status == CheckLifecycle::Cached)
                    .count(),
                entries.len()
            )),
    );

    f.render_widget(list, area);
}

/// Draw the output of the selected check
fn draw_check_output(f: &mut Frame, state: &TuiState, area: Rect) {
    let entries = &state.checks_state.entries;

    let (title, content) = if entries.is_empty() {
        (
            " Output ".to_string(),
            "Select a check to view output.".to_string(),
        )
    } else if let Some(entry) = entries.get(state.checks_state.selected) {
        let title = format!(" {} Output ", entry.name);
        let content = if entry.output.is_empty() {
            match entry.status {
                CheckLifecycle::Pending => "Check has not run yet.".to_string(),
                CheckLifecycle::Running => "Check is running...".to_string(),
                CheckLifecycle::Skipped => "Check was skipped.".to_string(),
                _ => "No output available.".to_string(),
            }
        } else {
            entry.output.clone()
        };
        (title, content)
    } else {
        (" Output ".to_string(), "No check selected.".to_string())
    };

    let paragraph = Paragraph::new(content)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title(title),
        )
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::White));

    f.render_widget(paragraph, area);
}
