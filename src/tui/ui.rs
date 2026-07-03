//! Main UI drawing functions for the TUI.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap};

use super::panels;
use super::types::{Panel, TuiState, WizardMode};

/// Main draw function - entry point for all rendering
pub fn draw(f: &mut Frame, state: &TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Length(3), // Tabs
            Constraint::Min(10),   // Main content
            Constraint::Length(3), // Footer/status
        ])
        .split(f.area());

    draw_header(f, state, chunks[0]);
    draw_tabs(f, state, chunks[1]);
    draw_content(f, state, chunks[2]);
    draw_footer(f, state, chunks[3]);

    // Help overlay (on top of everything)
    if state.show_help {
        draw_help_overlay(f);
    }

    // Branch wizard overlay
    if state.wizard_mode != WizardMode::None {
        draw_branch_wizard(f, state);
    }
}

/// Draw the header with branch info
fn draw_header(f: &mut Frame, state: &TuiState, area: Rect) {
    let target = if state.target_branch.is_empty() {
        "..."
    } else {
        &state.target_branch
    };

    let bases = if state.base_branches.is_empty() {
        "...".to_string()
    } else {
        state.base_branches.join(", ")
    };

    let profile = format!("{:?}", state.config.profile.kind);

    let status = if state.running {
        Span::styled(" [Running] ", Style::default().fg(Color::Yellow).bold())
    } else if state.report.is_some() {
        Span::styled(" [Complete] ", Style::default().fg(Color::Green).bold())
    } else {
        Span::styled(" [Ready] ", Style::default().fg(Color::Cyan))
    };

    let header = Line::from(vec![
        Span::styled(" prview ", Style::default().fg(Color::Cyan).bold()),
        Span::raw("| Target: "),
        Span::styled(target, Style::default().fg(Color::Yellow)),
        Span::raw(" | Base: "),
        Span::styled(&bases, Style::default().fg(Color::Green)),
        Span::raw(" | Profile: "),
        Span::styled(&profile, Style::default().fg(Color::Magenta)),
        status,
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let paragraph = Paragraph::new(header).block(block);
    f.render_widget(paragraph, area);
}

/// Draw the tab bar
fn draw_tabs(f: &mut Frame, state: &TuiState, area: Rect) {
    let titles: Vec<Line> = [
        Panel::Config,
        Panel::Checks,
        Panel::Diffs,
        Panel::Artifacts,
        Panel::Heuristics,
        Panel::State,
    ]
    .iter()
    .enumerate()
    .map(|(i, p)| {
        let num = format!("{}:", i + 1);
        let label = p.label();
        Line::from(vec![
            Span::styled(num, Style::default().fg(Color::DarkGray)),
            Span::raw(label),
        ])
    })
    .collect();

    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title("Panels"))
        .select(state.current_panel.index())
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider(" | ");

    f.render_widget(tabs, area);
}

/// Draw the main content area based on current panel
fn draw_content(f: &mut Frame, state: &TuiState, area: Rect) {
    match state.current_panel {
        Panel::Config => panels::config::draw(f, state, area),
        Panel::Checks => panels::checks::draw(f, state, area),
        Panel::Diffs => panels::diffs::draw(f, state, area),
        Panel::Artifacts => panels::artifacts::draw(f, state, area),
        Panel::Heuristics => panels::heuristics::draw(f, state, area),
        Panel::State => panels::state::draw(f, state, area),
    }
}

/// Draw the footer/status bar
fn draw_footer(f: &mut Frame, state: &TuiState, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![Span::raw(&state.message)]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray))
                .title("Status"),
        )
        .wrap(Wrap { trim: true });

    f.render_widget(footer, area);
}

/// Draw the help overlay
fn draw_help_overlay(f: &mut Frame) {
    let area = centered_rect(60, 70, f.area());

    // Clear the area first
    f.render_widget(Clear, area);

    let help_text = vec![
        Line::from(vec![Span::styled(
            "prview Keyboard Shortcuts",
            Style::default().fg(Color::Cyan).bold(),
        )]),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Navigation",
            Style::default().fg(Color::Yellow),
        )]),
        Line::from("  1-6         Switch to panel"),
        Line::from("  Tab         Next panel"),
        Line::from("  Shift+Tab   Previous panel"),
        Line::from("  Up/Down     Navigate list"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Actions",
            Style::default().fg(Color::Yellow),
        )]),
        Line::from("  r           Run analysis"),
        Line::from("  b           Branch selection wizard"),
        Line::from("  Enter       Select/expand item"),
        Line::from("  Space       Toggle selection"),
        Line::from("  d           Show full diff (Diffs panel)"),
        Line::from("  j/k         Scroll diff up/down"),
        Line::from("  PgUp/PgDn   Scroll diff"),
        Line::from("  o           Open file (Artifacts)"),
        Line::from("  c           Copy path (Artifacts)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "General",
            Style::default().fg(Color::Yellow),
        )]),
        Line::from("  ?           Show/hide help"),
        Line::from("  q / Esc     Quit"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Press ? or Esc to close",
            Style::default().fg(Color::DarkGray),
        )]),
    ];

    let help = Paragraph::new(help_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Help "),
    );

    f.render_widget(help, area);
}

/// Create a centered rect of given percentage size
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Draw the branch selection wizard overlay
fn draw_branch_wizard(f: &mut Frame, state: &TuiState) {
    let area = centered_rect(60, 80, f.area());

    // Clear the area first
    f.render_widget(Clear, area);

    // Split into sections
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title + instructions
            Constraint::Length(3), // Filter / tab toggle
            Constraint::Min(5),    // Branch list
            Constraint::Length(3), // Footer / selected bases
        ])
        .split(area);

    // Title based on wizard mode
    let title = match state.wizard_mode {
        WizardMode::SelectTarget => " Select Target Branch ",
        WizardMode::SelectBase => " Select Base Branch(es) ",
        WizardMode::None => " Branch Wizard ",
    };

    let instructions = match state.wizard_mode {
        WizardMode::SelectTarget => {
            "Use ↑/↓ to navigate, Enter to select, Tab for remote, Esc to cancel"
        }
        WizardMode::SelectBase => "Space to toggle, Enter when done, Tab for remote, Esc to cancel",
        WizardMode::None => "",
    };

    let title_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title);

    let title_para = Paragraph::new(Line::from(vec![Span::styled(
        instructions,
        Style::default().fg(Color::DarkGray),
    )]))
    .block(title_block);
    f.render_widget(title_para, chunks[0]);

    // Filter and tab toggle
    let source_label = if state.branch_selector.show_local {
        "Local"
    } else {
        "Remote"
    };

    let filter_text = if state.branch_selector.filter.is_empty() {
        format!("[{}] Type to filter...", source_label)
    } else {
        format!(
            "[{}] Filter: {}",
            source_label, state.branch_selector.filter
        )
    };

    let filter_block = Block::default().borders(Borders::ALL).title(" Filter ");
    let filter_para = Paragraph::new(Line::from(vec![
        Span::styled(&filter_text, Style::default().fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled("[Tab] toggle source", Style::default().fg(Color::DarkGray)),
    ]))
    .block(filter_block);
    f.render_widget(filter_para, chunks[1]);

    // Branch list
    let filtered_branches = state.get_filtered_branches();
    let items: Vec<ListItem> = filtered_branches
        .iter()
        .enumerate()
        .map(|(i, branch)| {
            let is_selected = i == state.branch_selector.selected;
            let is_current = state
                .branch_selector
                .current_branch
                .as_ref()
                .map(|c| c == *branch)
                .unwrap_or(false);
            let is_base_selected = state.branch_selector.selected_bases.contains(branch);

            let prefix = if is_base_selected {
                "[x] "
            } else if is_current {
                " * "
            } else {
                "   "
            };

            let style = if is_selected {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else if is_current {
                Style::default().fg(Color::Green)
            } else if is_base_selected {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };

            ListItem::new(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(*branch, style),
            ]))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Branches ({}) ", filtered_branches.len())),
    );
    f.render_widget(list, chunks[2]);

    // Footer with selected bases (for base mode)
    let footer_text = if state.wizard_mode == WizardMode::SelectBase
        && !state.branch_selector.selected_bases.is_empty()
    {
        format!(
            "Selected: {}",
            state.branch_selector.selected_bases.join(", ")
        )
    } else {
        String::new()
    };

    let footer_block = Block::default().borders(Borders::ALL);
    let footer_para = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().fg(Color::Green),
    )]))
    .block(footer_block);
    f.render_widget(footer_para, chunks[3]);
}
