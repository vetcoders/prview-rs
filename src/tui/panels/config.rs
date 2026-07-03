//! Config panel - displays current configuration settings with editable fields.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::tui::types::{ConfigField, TuiState};

/// Draw the config panel
pub fn draw(f: &mut Frame, state: &TuiState, area: Rect) {
    let config = &state.config;
    let selected = state.config_state.selected_field;

    // Helper function to create a selectable line
    fn selectable_line(
        selected: ConfigField,
        field: ConfigField,
        label: &str,
        value: String,
        enabled: bool,
    ) -> Line<'static> {
        let is_selected = selected == field;
        let prefix = if is_selected { "▶ " } else { "  " };
        let checkbox = if field == ConfigField::Branches {
            "[→]"
        } else if enabled {
            "[✓]"
        } else {
            "[ ]"
        };

        let style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        } else {
            Style::default()
        };

        let value_style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::Yellow)
        } else if enabled {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        Line::from(vec![
            Span::styled(prefix.to_string(), style),
            Span::styled(checkbox.to_string(), value_style),
            Span::styled(format!(" {}: ", label), style),
            Span::styled(value, value_style),
        ])
    }

    // Build config display text
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Profile: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{:?}", config.profile.kind),
                Style::default().fg(Color::Cyan).bold(),
            ),
        ]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "─── Branches (Enter to edit) ───",
            Style::default().fg(Color::Yellow).bold(),
        )]),
    ];

    // Branch selection field
    let branch_value = format!(
        "{} → {}",
        if state.target_branch.is_empty() {
            "(auto)"
        } else {
            &state.target_branch
        },
        if state.base_branches.is_empty() {
            "(auto)".to_string()
        } else {
            state.base_branches.join(", ")
        }
    );
    lines.push(selectable_line(
        selected,
        ConfigField::Branches,
        "Branches",
        branch_value,
        true,
    ));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "─── Run Options (Space to toggle) ───",
        Style::default().fg(Color::Yellow).bold(),
    )]));

    // Editable toggle fields
    lines.push(selectable_line(
        selected,
        ConfigField::RunTests,
        "Run Tests",
        if config.run_tests { "Yes" } else { "No" }.to_string(),
        config.run_tests,
    ));

    lines.push(selectable_line(
        selected,
        ConfigField::RunLint,
        "Run Lint",
        if config.run_lint { "Yes" } else { "No" }.to_string(),
        config.run_lint,
    ));

    lines.push(selectable_line(
        selected,
        ConfigField::RunBundle,
        "Run Bundle",
        if config.run_bundle { "Yes" } else { "No" }.to_string(),
        config.run_bundle,
    ));

    lines.push(selectable_line(
        selected,
        ConfigField::RunSecurity,
        "Security (geiger)",
        if config.run_security { "Yes" } else { "No" }.to_string(),
        config.run_security,
    ));

    lines.push(selectable_line(
        selected,
        ConfigField::RunHeuristics,
        "Heuristics",
        if config.run_heuristics { "Yes" } else { "No" }.to_string(),
        config.run_heuristics,
    ));

    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "─── General Options ───",
        Style::default().fg(Color::Yellow).bold(),
    )]));

    lines.push(selectable_line(
        selected,
        ConfigField::UseCache,
        "Cache",
        if config.use_cache {
            "Enabled"
        } else {
            "Disabled"
        }
        .to_string(),
        config.use_cache,
    ));

    // Static info section
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "─── Paths (read-only) ───",
        Style::default().fg(Color::DarkGray),
    )]));

    lines.push(Line::from(vec![
        Span::raw("  Repository: "),
        Span::styled(
            config
                .repo_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| ".".to_string()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    lines.push(Line::from(vec![
        Span::raw("  Output: "),
        Span::styled(
            config
                .output_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "./artifacts".to_string()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    // Checks from profile
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "─── Available Checks ───",
        Style::default().fg(Color::DarkGray),
    )]));

    let check_names = config.profile.get_check_names();
    for name in check_names {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("●", Style::default().fg(Color::Cyan)),
            Span::raw(format!(" {}", name)),
        ]));
    }

    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Configuration [↑↓ navigate, Space toggle, Enter edit] "),
        )
        .wrap(Wrap { trim: true })
        .scroll((state.config_state.scroll_offset as u16, 0));

    f.render_widget(paragraph, area);
}
