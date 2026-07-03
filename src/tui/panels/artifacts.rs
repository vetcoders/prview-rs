//! Artifacts panel - file browser for generated artifacts.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::tui::types::TuiState;

/// Draw the artifacts panel
pub fn draw(f: &mut Frame, state: &TuiState, area: Rect) {
    let entries = &state.artifacts_state.entries;

    if entries.is_empty() {
        let empty = Paragraph::new(
            "No artifacts generated yet.\n\nRun analysis to generate:\n  • diffs.md\n  • checks.md\n  • report.json\n  • zip archive",
        )
        .style(Style::default().fg(Color::DarkGray))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Artifacts "),
        );
        f.render_widget(empty, area);
        return;
    }

    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_selected = i == state.artifacts_state.selected;

            // Icon based on file type
            let icon = if entry.is_dir {
                "📁"
            } else if entry.name.ends_with(".md") {
                "📄"
            } else if entry.name.ends_with(".json") {
                "📋"
            } else if entry.name.ends_with(".zip") {
                "📦"
            } else {
                "📎"
            };

            // Size display (only for files)
            let size = if entry.is_dir {
                "     ".to_string()
            } else {
                format_size(entry.size)
            };

            let line = Line::from(vec![
                Span::raw(if is_selected { "▶ " } else { "  " }),
                Span::raw(icon),
                Span::raw(" "),
                Span::styled(
                    &entry.name,
                    Style::default()
                        .fg(if is_selected {
                            Color::Cyan
                        } else {
                            Color::White
                        })
                        .add_modifier(if entry.is_dir {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw("  "),
                Span::styled(size, Style::default().fg(Color::DarkGray)),
            ]);

            ListItem::new(line)
        })
        .collect();

    // Calculate total size
    let total_size: u64 = entries.iter().map(|e| e.size).sum();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(
                " Artifacts ({} files, {}) ",
                entries.len(),
                format_size(total_size)
            )),
    );

    f.render_widget(list, area);
}

/// Format file size for display
fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".to_string();
    }

    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", size, UNITS[unit_index])
    }
}
