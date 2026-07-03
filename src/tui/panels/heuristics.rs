//! Heuristics panel - displays loctree analysis results.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::tui::types::TuiState;

/// Draw the heuristics panel
pub fn draw(f: &mut Frame, state: &TuiState, area: Rect) {
    let heuristics = &state.heuristics_state;

    // Check if we have any results
    let has_results = heuristics.dead_exports_count > 0
        || heuristics.cycles_count > 0
        || heuristics.twins_count > 0;

    if !has_results && state.report.is_none() {
        let empty = Paragraph::new(
            "No heuristics results yet.\n\nRun analysis to detect:\n  • Dead exports (unused public symbols)\n  • Circular imports (dependency cycles)\n  • Code twins (duplicate code patterns)",
        )
        .style(Style::default().fg(Color::DarkGray))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Heuristics (loctree) "),
        );
        f.render_widget(empty, area);
        return;
    }

    // Build sections
    let sections = [
        (
            "Dead Exports",
            heuristics.dead_exports_count,
            Color::Yellow,
            "Unused public symbols that could be removed",
        ),
        (
            "Circular Imports",
            heuristics.cycles_count,
            Color::Red,
            "Dependency cycles that may cause issues",
        ),
        (
            "Code Twins",
            heuristics.twins_count,
            Color::Magenta,
            "Duplicate code patterns (potential refactor)",
        ),
    ];

    let mut lines: Vec<Line> = vec![];

    for (i, (name, count, color, description)) in sections.iter().enumerate() {
        let is_selected = i == heuristics.selected_section;
        let icon = if *count > 0 { "●" } else { "○" };

        lines.push(Line::from(vec![
            Span::raw(if is_selected { "▶ " } else { "  " }),
            Span::styled(
                icon,
                Style::default().fg(if *count > 0 { *color } else { Color::DarkGray }),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{:<20}", name),
                Style::default()
                    .fg(if is_selected {
                        Color::Cyan
                    } else {
                        Color::White
                    })
                    .add_modifier(if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Span::styled(format!("{:>5} found", count), Style::default().fg(*color)),
        ]));

        // Show description for selected section
        if is_selected {
            lines.push(Line::from(vec![
                Span::raw("     "),
                Span::styled(*description, Style::default().fg(Color::DarkGray)),
            ]));
            lines.push(Line::from(""));
        }
    }

    // Add summary
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "─".repeat(40),
        Style::default().fg(Color::DarkGray),
    )]));

    let total_issues =
        heuristics.dead_exports_count + heuristics.cycles_count + heuristics.twins_count;

    let health_color = if total_issues == 0 {
        Color::Green
    } else if total_issues < 10 {
        Color::Yellow
    } else {
        Color::Red
    };

    lines.push(Line::from(vec![
        Span::styled("  Code Health: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            if total_issues == 0 {
                "Excellent ●"
            } else if total_issues < 10 {
                "Good ◐"
            } else {
                "Needs Attention ○"
            },
            Style::default().fg(health_color).bold(),
        ),
    ]));

    lines.push(Line::from(vec![
        Span::styled("  Total Issues: ", Style::default().fg(Color::DarkGray)),
        Span::styled(total_issues.to_string(), Style::default().fg(health_color)),
    ]));

    if state.report.is_some() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Full details in the Artifacts panel [4]",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Heuristics (loctree) "),
        )
        .wrap(Wrap { trim: true })
        .scroll((heuristics.scroll_offset as u16, 0));

    f.render_widget(paragraph, area);
}
