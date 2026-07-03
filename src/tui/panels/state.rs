//! State panel — repo probe viewer for the TUI.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::state::hot::HOT_FILES_DISPLAY_LIMIT;
use crate::tui::types::TuiState;

/// Draw the state panel
pub fn draw(f: &mut Frame, state: &TuiState, area: Rect) {
    let repo_state = match &state.state_panel.repo_state {
        Some(rs) => rs,
        None => {
            let placeholder = Paragraph::new("No state data. Run with: prview state --tui")
                .block(Block::default().borders(Borders::ALL).title(" State "))
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(placeholder, area);
            return;
        }
    };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("Repository  ", Style::default().fg(Color::Cyan).bold()),
            Span::raw(&repo_state.repo),
        ]),
        Line::from(vec![
            Span::styled("Branch      ", Style::default().fg(Color::Cyan).bold()),
            Span::styled(&repo_state.branch, Style::default().fg(Color::Green)),
        ]),
        Line::from(vec![
            Span::styled("HEAD        ", Style::default().fg(Color::Cyan).bold()),
            Span::styled(&repo_state.head, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(""),
    ];

    // Diff stats
    lines.push(Line::from(vec![Span::styled(
        "Diff Stats  ",
        Style::default().fg(Color::Cyan).bold(),
    )]));

    if repo_state.files_changed == 0 {
        lines.push(Line::from(vec![Span::styled(
            "  Working tree clean",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        lines.push(Line::from(vec![
            Span::raw("  Files changed: "),
            Span::styled(
                repo_state.files_changed.to_string(),
                Style::default().fg(Color::White).bold(),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Insertions:    "),
            Span::styled(
                format!("+{}", repo_state.insertions),
                Style::default().fg(Color::Green),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  Deletions:     "),
            Span::styled(
                format!("-{}", repo_state.deletions),
                Style::default().fg(Color::Red),
            ),
        ]));
    }

    // Hot files
    if !repo_state.hot_files.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Hot Files   ", Style::default().fg(Color::Cyan).bold()),
            Span::styled(
                format!(
                    "(top {})",
                    repo_state.hot_files.len().min(HOT_FILES_DISPLAY_LIMIT)
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));

        for hot in repo_state.hot_files.iter().take(HOT_FILES_DISPLAY_LIMIT) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:>6}  ", hot.lines),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(&hot.path),
            ]));
        }
    }

    let max_scroll = lines.len().saturating_sub(1);
    let scroll = state.state_panel.scroll_offset.min(max_scroll) as u16;
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" State "),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));

    f.render_widget(paragraph, area);
}
