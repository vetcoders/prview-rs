//! Keyboard event handlers for the TUI.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use super::types::{ConfigField, Panel, TuiEvent, TuiState, WizardMode};

/// Handle a key event
pub async fn handle_key(
    state: &mut TuiState,
    key: KeyEvent,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) -> Result<()> {
    // Wizard mode takes priority
    if state.wizard_mode != WizardMode::None {
        handle_wizard_keys(state, key);
        return Ok(());
    }

    // Help overlay takes precedence
    if state.show_help {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') => {
                state.show_help = false;
            }
            _ => {}
        }
        return Ok(());
    }

    // Global keys (work everywhere)
    match key.code {
        KeyCode::Char('q') => {
            state.should_quit = true;
            return Ok(());
        }
        KeyCode::Char('?') => {
            state.show_help = true;
            return Ok(());
        }
        KeyCode::Esc => {
            state.should_quit = true;
            return Ok(());
        }
        _ => {}
    }

    // Number keys for panel switching
    match key.code {
        KeyCode::Char('1') => {
            state.current_panel = Panel::Config;
            state.update_message();
            return Ok(());
        }
        KeyCode::Char('2') => {
            state.current_panel = Panel::Checks;
            state.update_message();
            return Ok(());
        }
        KeyCode::Char('3') => {
            state.current_panel = Panel::Diffs;
            state.update_message();
            return Ok(());
        }
        KeyCode::Char('4') => {
            state.current_panel = Panel::Artifacts;
            state.update_message();
            return Ok(());
        }
        KeyCode::Char('5') => {
            state.current_panel = Panel::Heuristics;
            state.update_message();
            return Ok(());
        }
        KeyCode::Char('6') => {
            state.current_panel = Panel::State;
            state.update_message();
            return Ok(());
        }
        _ => {}
    }

    // Tab navigation
    match key.code {
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                state.current_panel = state.current_panel.prev();
            } else {
                state.current_panel = state.current_panel.next();
            }
            state.update_message();
            return Ok(());
        }
        KeyCode::BackTab => {
            state.current_panel = state.current_panel.prev();
            state.update_message();
            return Ok(());
        }
        _ => {}
    }

    // Run analysis
    if key.code == KeyCode::Char('r') && !state.running {
        state.running = true;
        state.start_time = Some(std::time::Instant::now());
        state.message = "Starting analysis...".to_string();

        // Spawn analysis task.
        // run_analysis does all git2 (non-Send) work synchronously before
        // the first .await, so the future is Send-safe for tokio::spawn.
        let config = state.config.clone();
        let tx_err = tx.clone();
        let handle = tokio::spawn(super::run_analysis(config, tx.clone()));

        // Supervise the task so a panic (or cancellation) surfaces as an Error
        // event. Without this, a panicked analysis leaves checks stuck rendering
        // as "running" and the header stuck at "Running analysis... Ns" forever.
        tokio::spawn(async move {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = tx_err.send(TuiEvent::Error {
                        message: e.to_string(),
                    });
                }
                Err(join_err) => {
                    let _ = tx_err.send(TuiEvent::Error {
                        message: format!("analysis task aborted: {join_err}"),
                    });
                }
            }
        });

        return Ok(());
    }

    // Panel-specific key handling
    match state.current_panel {
        Panel::Config => handle_config_keys(state, key),
        Panel::Checks => handle_checks_keys(state, key),
        Panel::Diffs => handle_diffs_keys(state, key),
        Panel::Artifacts => handle_artifacts_keys(state, key),
        Panel::Heuristics => handle_heuristics_keys(state, key),
        Panel::State => handle_state_keys(state, key),
    }

    Ok(())
}

fn handle_config_keys(state: &mut TuiState, key: KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.config_state.selected_field = state.config_state.selected_field.prev();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.config_state.selected_field = state.config_state.selected_field.next();
        }
        KeyCode::Char('b') => {
            // Open branch selection wizard
            state.start_target_wizard();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            // Toggle selected field or open wizard
            match state.config_state.selected_field {
                ConfigField::Branches => {
                    state.start_target_wizard();
                }
                ConfigField::RunTests => {
                    state.config.run_tests = !state.config.run_tests;
                }
                ConfigField::RunLint => {
                    state.config.run_lint = !state.config.run_lint;
                }
                ConfigField::RunBundle => {
                    state.config.run_bundle = !state.config.run_bundle;
                }
                ConfigField::RunSecurity => {
                    state.config.run_security = !state.config.run_security;
                }
                ConfigField::RunHeuristics => {
                    state.config.run_heuristics = !state.config.run_heuristics;
                }
                ConfigField::UseCache => {
                    state.config.use_cache = !state.config.use_cache;
                }
            }
        }
        _ => {}
    }
}

/// Handle keys in wizard mode (branch selection)
fn handle_wizard_keys(state: &mut TuiState, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Cancel wizard
            state.wizard_mode = WizardMode::None;
            state.message = "Branch selection cancelled.".to_string();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.branch_select_up();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.branch_select_down();
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            if state.wizard_mode == WizardMode::SelectBase {
                // In base mode, space toggles selection, Enter confirms
                if key.code == KeyCode::Char(' ') {
                    state.select_branch();
                } else {
                    // Enter finishes base selection
                    state.finish_base_selection();
                }
            } else {
                // In target mode, both select and advance
                state.select_branch();
            }
        }
        KeyCode::Tab => {
            // Toggle between local and remote branches
            state.toggle_branch_source();
        }
        KeyCode::Char(c) if c.is_alphabetic() || c == '/' || c == '-' || c == '_' => {
            // Add to filter
            state.branch_selector.filter.push(c);
            state.branch_selector.selected = 0;
        }
        KeyCode::Backspace => {
            // Remove from filter
            state.branch_selector.filter.pop();
            state.branch_selector.selected = 0;
        }
        _ => {}
    }
}

fn handle_checks_keys(state: &mut TuiState, key: KeyEvent) {
    let len = state.checks_state.entries.len();
    if len == 0 {
        return;
    }

    match key.code {
        KeyCode::Up if state.checks_state.selected > 0 => {
            state.checks_state.selected -= 1;
        }
        KeyCode::Down if state.checks_state.selected < len - 1 => {
            state.checks_state.selected += 1;
        }
        _ => {}
    }
}

fn handle_diffs_keys(state: &mut TuiState, key: KeyEvent) {
    let len = state.diffs_state.files.len();
    if len == 0 {
        return;
    }

    match key.code {
        KeyCode::Up if state.diffs_state.selected > 0 => {
            state.diffs_state.selected -= 1;
            // Clear diff content when changing selection
            state.diffs_state.diff_content.clear();
            state.diffs_state.show_diff = false;
        }
        KeyCode::Down if state.diffs_state.selected < len - 1 => {
            state.diffs_state.selected += 1;
            // Clear diff content when changing selection
            state.diffs_state.diff_content.clear();
            state.diffs_state.show_diff = false;
        }
        KeyCode::Enter => {
            // Toggle diff view - load content if needed
            state.diffs_state.show_diff = !state.diffs_state.show_diff;

            if state.diffs_state.show_diff && state.diffs_state.diff_content.is_empty() {
                // Load diff content for selected file
                if let Some(file) = state.diffs_state.files.get(state.diffs_state.selected) {
                    let base = file
                        .base_ref
                        .as_deref()
                        .or_else(|| state.base_branches.first().map(|s| s.as_str()))
                        .unwrap_or("main");
                    let target = file.target_ref.as_deref().unwrap_or_else(|| {
                        if state.target_branch.is_empty() {
                            "HEAD"
                        } else {
                            &state.target_branch
                        }
                    });

                    if let Ok(repo) = crate::git::Repository::open(&state.config.repo_root) {
                        match repo.file_diff(base, target, &file.path) {
                            Ok(diff) => {
                                state.diffs_state.diff_content = diff;
                                state.diffs_state.diff_scroll = 0;
                            }
                            Err(e) => {
                                state.diffs_state.diff_content =
                                    format!("Error loading diff: {}", e);
                            }
                        }
                    } else {
                        state.diffs_state.diff_content =
                            "Error: Could not open repository".to_string();
                    }
                }
            }
        }
        KeyCode::Char('d') => {
            // Load full diff (all files)
            let base = state
                .base_branches
                .first()
                .map(|s| s.as_str())
                .unwrap_or("main");
            let target = if state.target_branch.is_empty() {
                "HEAD"
            } else {
                &state.target_branch
            };

            if let Ok(repo) = crate::git::Repository::open(&state.config.repo_root) {
                match repo.full_diff(base, target) {
                    Ok(diff) => {
                        state.diffs_state.diff_content = diff;
                        state.diffs_state.diff_scroll = 0;
                        state.diffs_state.show_diff = true;
                        state.message = "Showing full diff".to_string();
                    }
                    Err(e) => {
                        state.message = format!("Error loading diff: {}", e);
                    }
                }
            }
        }
        // Scroll diff preview
        KeyCode::Char('j') | KeyCode::PageDown if state.diffs_state.show_diff => {
            let lines = state.diffs_state.diff_content.lines().count() as u16;
            if state.diffs_state.diff_scroll < lines.saturating_sub(10) {
                state.diffs_state.diff_scroll += 5;
            }
        }
        KeyCode::Char('k') | KeyCode::PageUp if state.diffs_state.show_diff => {
            state.diffs_state.diff_scroll = state.diffs_state.diff_scroll.saturating_sub(5);
        }
        KeyCode::Home => {
            state.diffs_state.diff_scroll = 0;
        }
        KeyCode::End if state.diffs_state.show_diff => {
            let lines = state.diffs_state.diff_content.lines().count() as u16;
            state.diffs_state.diff_scroll = lines.saturating_sub(10);
        }
        _ => {}
    }
}

fn handle_artifacts_keys(state: &mut TuiState, key: KeyEvent) {
    let len = state.artifacts_state.entries.len();
    if len == 0 {
        return;
    }

    match key.code {
        KeyCode::Up if state.artifacts_state.selected > 0 => {
            state.artifacts_state.selected -= 1;
        }
        KeyCode::Down if state.artifacts_state.selected < len - 1 => {
            state.artifacts_state.selected += 1;
        }
        KeyCode::Enter | KeyCode::Char('o') => {
            // Open selected artifact
            if let Some(entry) = state
                .artifacts_state
                .entries
                .get(state.artifacts_state.selected)
            {
                // .status() instead of bare .spawn(): a forgotten child is
                // never reaped (zombie until prview exits). open/xdg-open
                // return quickly by design. Stdio fully detached so the
                // opener cannot read keys or scribble over the TUI frame.
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open")
                    .arg(&entry.path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("xdg-open")
                    .arg(&entry.path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                #[cfg(target_os = "windows")]
                let _ = std::process::Command::new("explorer")
                    .arg(&entry.path)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        KeyCode::Char('c') => {
            // Show the full path in the status line (no clipboard integration).
            if let Some(entry) = state
                .artifacts_state
                .entries
                .get(state.artifacts_state.selected)
            {
                state.message = format!("Path: {}", entry.path.display());
            }
        }
        _ => {}
    }
}

fn handle_state_keys(state: &mut TuiState, key: KeyEvent) {
    // Content is clamped at render time, so we cap at a reasonable max here
    // to prevent usize overflow on repeated PageDown.
    const MAX_SCROLL: usize = 10_000;
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            state.state_panel.scroll_offset = state.state_panel.scroll_offset.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            state.state_panel.scroll_offset = (state.state_panel.scroll_offset + 1).min(MAX_SCROLL);
        }
        KeyCode::PageUp => {
            state.state_panel.scroll_offset = state.state_panel.scroll_offset.saturating_sub(10);
        }
        KeyCode::PageDown => {
            state.state_panel.scroll_offset =
                (state.state_panel.scroll_offset + 10).min(MAX_SCROLL);
        }
        KeyCode::Home => {
            state.state_panel.scroll_offset = 0;
        }
        _ => {}
    }
}

fn handle_heuristics_keys(state: &mut TuiState, key: KeyEvent) {
    match key.code {
        KeyCode::Up if state.heuristics_state.selected_section > 0 => {
            state.heuristics_state.selected_section -= 1;
        }
        KeyCode::Down if state.heuristics_state.selected_section < 2 => {
            state.heuristics_state.selected_section += 1;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, test_config};
    use crate::tui::types::{CheckEntry, FileEntry};
    use std::path::PathBuf;

    fn create_test_config() -> Config {
        let mut config = test_config();
        config.target = Some("main".to_string());
        config.bases = vec!["develop".to_string()];
        config.tui_mode = true;
        config.do_fetch = false;
        config.use_cache = false;
        config.create_zip = false;
        config
    }

    fn create_key_event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn test_handle_config_keys_navigate_down() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        assert_eq!(state.config_state.selected_field, ConfigField::Branches);

        handle_config_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.config_state.selected_field, ConfigField::RunTests);

        handle_config_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.config_state.selected_field, ConfigField::RunLint);
    }

    #[test]
    fn test_handle_config_keys_navigate_up() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        state.config_state.selected_field = ConfigField::RunHeuristics;

        handle_config_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.config_state.selected_field, ConfigField::RunSecurity);

        handle_config_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.config_state.selected_field, ConfigField::RunBundle);
    }

    #[test]
    fn test_handle_config_keys_navigate_wraps() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        assert_eq!(state.config_state.selected_field, ConfigField::Branches);

        // Up from first field wraps to last
        handle_config_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.config_state.selected_field, ConfigField::UseCache);
    }

    #[test]
    fn test_handle_config_keys_toggle() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        state.config_state.selected_field = ConfigField::RunTests;
        assert!(!state.config.run_tests);

        handle_config_keys(&mut state, create_key_event(KeyCode::Char(' ')));
        assert!(state.config.run_tests);

        handle_config_keys(&mut state, create_key_event(KeyCode::Char(' ')));
        assert!(!state.config.run_tests);
    }

    #[test]
    fn test_handle_checks_keys_empty() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        assert!(state.checks_state.entries.is_empty());

        handle_checks_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.checks_state.selected, 0);
    }

    #[test]
    fn test_handle_checks_keys_navigate() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state.checks_state.entries.push(CheckEntry::new("Check1"));
        state.checks_state.entries.push(CheckEntry::new("Check2"));
        state.checks_state.entries.push(CheckEntry::new("Check3"));

        assert_eq!(state.checks_state.selected, 0);

        handle_checks_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.checks_state.selected, 1);

        handle_checks_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.checks_state.selected, 2);

        handle_checks_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.checks_state.selected, 2);

        handle_checks_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.checks_state.selected, 1);
    }

    #[test]
    fn test_handle_diffs_keys_empty() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        assert!(state.diffs_state.files.is_empty());

        handle_diffs_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.diffs_state.selected, 0);
    }

    #[test]
    fn test_handle_diffs_keys_navigate() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state.diffs_state.files.push(FileEntry {
            path: "file1.rs".to_string(),
            kind: crate::tui::types::FileChangeKind::Modified,
            additions: 10,
            deletions: 5,
            base_ref: None,
            target_ref: None,
        });
        state.diffs_state.files.push(FileEntry {
            path: "file2.rs".to_string(),
            kind: crate::tui::types::FileChangeKind::Added,
            additions: 50,
            deletions: 0,
            base_ref: None,
            target_ref: None,
        });

        handle_diffs_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.diffs_state.selected, 1);

        handle_diffs_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.diffs_state.selected, 0);
    }

    #[test]
    fn test_handle_diffs_keys_scroll() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        // Add file entry so function doesn't return early
        state.diffs_state.files.push(FileEntry {
            path: "file.rs".to_string(),
            kind: crate::tui::types::FileChangeKind::Modified,
            additions: 10,
            deletions: 5,
            base_ref: None,
            target_ref: None,
        });

        state.diffs_state.show_diff = true;
        state.diffs_state.diff_content = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12\nline13\nline14\nline15".to_string();

        assert_eq!(state.diffs_state.diff_scroll, 0);

        handle_diffs_keys(&mut state, create_key_event(KeyCode::Char('j')));
        assert_eq!(state.diffs_state.diff_scroll, 5);

        handle_diffs_keys(&mut state, create_key_event(KeyCode::Char('k')));
        assert_eq!(state.diffs_state.diff_scroll, 0);

        handle_diffs_keys(&mut state, create_key_event(KeyCode::Home));
        assert_eq!(state.diffs_state.diff_scroll, 0);
    }

    #[test]
    fn test_handle_artifacts_keys_empty() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        assert!(state.artifacts_state.entries.is_empty());

        handle_artifacts_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.artifacts_state.selected, 0);
    }

    #[test]
    fn test_handle_artifacts_keys_navigate() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state
            .artifacts_state
            .entries
            .push(crate::tui::types::ArtifactEntry {
                name: "file1.txt".to_string(),
                path: PathBuf::from("/tmp/file1.txt"),
                size: 100,
                is_dir: false,
            });
        state
            .artifacts_state
            .entries
            .push(crate::tui::types::ArtifactEntry {
                name: "file2.txt".to_string(),
                path: PathBuf::from("/tmp/file2.txt"),
                size: 200,
                is_dir: false,
            });

        handle_artifacts_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.artifacts_state.selected, 1);

        handle_artifacts_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.artifacts_state.selected, 0);
    }

    #[test]
    fn test_handle_heuristics_keys_navigate() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        assert_eq!(state.heuristics_state.selected_section, 0);

        handle_heuristics_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.heuristics_state.selected_section, 1);

        handle_heuristics_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.heuristics_state.selected_section, 2);

        handle_heuristics_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.heuristics_state.selected_section, 2);

        handle_heuristics_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.heuristics_state.selected_section, 1);

        handle_heuristics_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.heuristics_state.selected_section, 0);

        handle_heuristics_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.heuristics_state.selected_section, 0);
    }

    #[test]
    fn test_handle_config_keys_other_key() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let initial_field = state.config_state.selected_field;

        // 'x' key does nothing
        handle_config_keys(&mut state, create_key_event(KeyCode::Char('x')));
        assert_eq!(state.config_state.selected_field, initial_field);
    }

    #[test]
    fn test_handle_checks_keys_other_key() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        state.checks_state.entries.push(CheckEntry::new("Check"));

        handle_checks_keys(&mut state, create_key_event(KeyCode::Char('x')));
        assert_eq!(state.checks_state.selected, 0);
    }

    #[test]
    fn test_handle_heuristics_keys_other_key() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        handle_heuristics_keys(&mut state, create_key_event(KeyCode::Enter));
        assert_eq!(state.heuristics_state.selected_section, 0);
    }

    #[tokio::test]
    async fn test_handle_key_quit_q() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        assert!(!state.should_quit);
        handle_key(&mut state, create_key_event(KeyCode::Char('q')), &tx)
            .await
            .unwrap();
        assert!(state.should_quit);
    }

    #[tokio::test]
    async fn test_handle_key_quit_esc() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        assert!(!state.should_quit);
        handle_key(&mut state, create_key_event(KeyCode::Esc), &tx)
            .await
            .unwrap();
        assert!(state.should_quit);
    }

    #[tokio::test]
    async fn test_handle_key_show_help() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        assert!(!state.show_help);
        handle_key(&mut state, create_key_event(KeyCode::Char('?')), &tx)
            .await
            .unwrap();
        assert!(state.show_help);
    }

    #[tokio::test]
    async fn test_handle_key_close_help_esc() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        state.show_help = true;
        handle_key(&mut state, create_key_event(KeyCode::Esc), &tx)
            .await
            .unwrap();
        assert!(!state.show_help);
        // Should NOT quit when closing help
        assert!(!state.should_quit);
    }

    #[tokio::test]
    async fn test_handle_key_close_help_q() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        state.show_help = true;
        handle_key(&mut state, create_key_event(KeyCode::Char('q')), &tx)
            .await
            .unwrap();
        assert!(!state.show_help);
        assert!(!state.should_quit);
    }

    #[tokio::test]
    async fn test_handle_key_close_help_question_mark() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        state.show_help = true;
        handle_key(&mut state, create_key_event(KeyCode::Char('?')), &tx)
            .await
            .unwrap();
        assert!(!state.show_help);
    }

    #[tokio::test]
    async fn test_handle_key_panel_switch_1() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        state.current_panel = Panel::Diffs;
        handle_key(&mut state, create_key_event(KeyCode::Char('1')), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Config);
    }

    #[tokio::test]
    async fn test_handle_key_panel_switch_2() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_key(&mut state, create_key_event(KeyCode::Char('2')), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Checks);
    }

    #[tokio::test]
    async fn test_handle_key_panel_switch_3() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_key(&mut state, create_key_event(KeyCode::Char('3')), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Diffs);
    }

    #[tokio::test]
    async fn test_handle_key_panel_switch_4() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_key(&mut state, create_key_event(KeyCode::Char('4')), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Artifacts);
    }

    #[tokio::test]
    async fn test_handle_key_panel_switch_5() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_key(&mut state, create_key_event(KeyCode::Char('5')), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Heuristics);
    }

    #[tokio::test]
    async fn test_handle_key_panel_switch_6() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_key(&mut state, create_key_event(KeyCode::Char('6')), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::State);
    }

    #[test]
    fn test_handle_state_keys_scroll() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        assert_eq!(state.state_panel.scroll_offset, 0);

        // Down
        handle_state_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.state_panel.scroll_offset, 1);

        // Up back to 0
        handle_state_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.state_panel.scroll_offset, 0);

        // Up at 0 stays 0 (saturating)
        handle_state_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.state_panel.scroll_offset, 0);

        // PageDown
        handle_state_keys(&mut state, create_key_event(KeyCode::PageDown));
        assert_eq!(state.state_panel.scroll_offset, 10);

        // PageUp
        handle_state_keys(&mut state, create_key_event(KeyCode::PageUp));
        assert_eq!(state.state_panel.scroll_offset, 0);

        // Home resets
        state.state_panel.scroll_offset = 42;
        handle_state_keys(&mut state, create_key_event(KeyCode::Home));
        assert_eq!(state.state_panel.scroll_offset, 0);

        // j/k vim keys
        handle_state_keys(&mut state, create_key_event(KeyCode::Char('j')));
        assert_eq!(state.state_panel.scroll_offset, 1);
        handle_state_keys(&mut state, create_key_event(KeyCode::Char('k')));
        assert_eq!(state.state_panel.scroll_offset, 0);
    }

    #[test]
    fn test_handle_state_keys_scroll_capped_at_max() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        // Set near MAX_SCROLL (10_000), verify cap
        state.state_panel.scroll_offset = 9_999;
        handle_state_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.state_panel.scroll_offset, 10_000);

        // At cap, Down stays at cap
        handle_state_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.state_panel.scroll_offset, 10_000);

        // PageDown at cap stays at cap
        handle_state_keys(&mut state, create_key_event(KeyCode::PageDown));
        assert_eq!(state.state_panel.scroll_offset, 10_000);

        // PageDown near cap clamps
        state.state_panel.scroll_offset = 9_995;
        handle_state_keys(&mut state, create_key_event(KeyCode::PageDown));
        assert_eq!(state.state_panel.scroll_offset, 10_000);
    }

    #[test]
    fn test_handle_state_keys_other_key() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        state.state_panel.scroll_offset = 5;

        handle_state_keys(&mut state, create_key_event(KeyCode::Char('x')));
        assert_eq!(state.state_panel.scroll_offset, 5);
    }

    #[tokio::test]
    async fn test_handle_key_tab_navigation() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        assert_eq!(state.current_panel, Panel::Config);
        handle_key(&mut state, create_key_event(KeyCode::Tab), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Checks);

        handle_key(&mut state, create_key_event(KeyCode::Tab), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Diffs);
    }

    #[tokio::test]
    async fn test_handle_key_backtab_navigation() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        state.current_panel = Panel::Checks;
        handle_key(&mut state, create_key_event(KeyCode::BackTab), &tx)
            .await
            .unwrap();
        assert_eq!(state.current_panel, Panel::Config);
    }

    #[tokio::test]
    async fn test_handle_key_shift_tab_navigation() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        state.current_panel = Panel::Diffs;
        let shift_tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
        handle_key(&mut state, shift_tab, &tx).await.unwrap();
        assert_eq!(state.current_panel, Panel::Checks);
    }

    #[tokio::test]
    async fn test_handle_key_help_ignores_panel_keys() {
        let config = create_test_config();
        let mut state = TuiState::new(config);
        let (tx, _rx) = mpsc::unbounded_channel();

        state.show_help = true;
        state.current_panel = Panel::Config;

        // Panel switch should be ignored when help is shown
        handle_key(&mut state, create_key_event(KeyCode::Char('2')), &tx)
            .await
            .unwrap();
        // Panel should not change
        assert_eq!(state.current_panel, Panel::Config);
        // Help should still be shown
        assert!(state.show_help);
    }

    #[test]
    fn test_handle_artifacts_keys_copy_path() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state
            .artifacts_state
            .entries
            .push(crate::tui::types::ArtifactEntry {
                name: "file.txt".to_string(),
                path: PathBuf::from("/tmp/file.txt"),
                size: 100,
                is_dir: false,
            });

        handle_artifacts_keys(&mut state, create_key_event(KeyCode::Char('c')));
        assert!(state.message.contains("/tmp/file.txt"));
    }

    #[test]
    fn test_handle_diffs_keys_home() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state.diffs_state.files.push(FileEntry {
            path: "file.rs".to_string(),
            kind: crate::tui::types::FileChangeKind::Modified,
            additions: 10,
            deletions: 5,
            base_ref: None,
            target_ref: None,
        });

        state.diffs_state.diff_scroll = 50;
        handle_diffs_keys(&mut state, create_key_event(KeyCode::Home));
        assert_eq!(state.diffs_state.diff_scroll, 0);
    }

    #[test]
    fn test_handle_diffs_keys_end() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state.diffs_state.files.push(FileEntry {
            path: "file.rs".to_string(),
            kind: crate::tui::types::FileChangeKind::Modified,
            additions: 10,
            deletions: 5,
            base_ref: None,
            target_ref: None,
        });

        state.diffs_state.show_diff = true;
        state.diffs_state.diff_content = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12\nline13\nline14\nline15\nline16\nline17\nline18\nline19\nline20".to_string();

        handle_diffs_keys(&mut state, create_key_event(KeyCode::End));
        assert!(state.diffs_state.diff_scroll > 0);
    }

    #[test]
    fn test_handle_diffs_keys_page_up_down() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state.diffs_state.files.push(FileEntry {
            path: "file.rs".to_string(),
            kind: crate::tui::types::FileChangeKind::Modified,
            additions: 10,
            deletions: 5,
            base_ref: None,
            target_ref: None,
        });

        state.diffs_state.show_diff = true;
        state.diffs_state.diff_content = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\nline11\nline12\nline13\nline14\nline15\nline16\nline17\nline18\nline19\nline20".to_string();

        handle_diffs_keys(&mut state, create_key_event(KeyCode::PageDown));
        let scroll_after_down = state.diffs_state.diff_scroll;
        assert!(scroll_after_down > 0);

        handle_diffs_keys(&mut state, create_key_event(KeyCode::PageUp));
        assert!(state.diffs_state.diff_scroll < scroll_after_down);
    }

    #[test]
    fn test_handle_diffs_keys_other() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state.diffs_state.files.push(FileEntry {
            path: "file.rs".to_string(),
            kind: crate::tui::types::FileChangeKind::Modified,
            additions: 10,
            deletions: 5,
            base_ref: None,
            target_ref: None,
        });

        let initial_selected = state.diffs_state.selected;
        handle_diffs_keys(&mut state, create_key_event(KeyCode::Char('x')));
        assert_eq!(state.diffs_state.selected, initial_selected);
    }

    #[test]
    fn test_handle_artifacts_keys_other() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state
            .artifacts_state
            .entries
            .push(crate::tui::types::ArtifactEntry {
                name: "file.txt".to_string(),
                path: PathBuf::from("/tmp/file.txt"),
                size: 100,
                is_dir: false,
            });

        let initial_selected = state.artifacts_state.selected;
        handle_artifacts_keys(&mut state, create_key_event(KeyCode::Char('x')));
        assert_eq!(state.artifacts_state.selected, initial_selected);
    }

    #[test]
    fn test_handle_checks_keys_navigate_at_boundary() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state.checks_state.entries.push(CheckEntry::new("Check1"));
        state.checks_state.entries.push(CheckEntry::new("Check2"));

        // Try to go up at 0
        handle_checks_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.checks_state.selected, 0);
    }

    #[test]
    fn test_handle_artifacts_keys_navigate_at_boundary() {
        let config = create_test_config();
        let mut state = TuiState::new(config);

        state
            .artifacts_state
            .entries
            .push(crate::tui::types::ArtifactEntry {
                name: "file.txt".to_string(),
                path: PathBuf::from("/tmp/file.txt"),
                size: 100,
                is_dir: false,
            });

        // Try to go up at 0
        handle_artifacts_keys(&mut state, create_key_event(KeyCode::Up));
        assert_eq!(state.artifacts_state.selected, 0);

        // Try to go down at last item
        handle_artifacts_keys(&mut state, create_key_event(KeyCode::Down));
        assert_eq!(state.artifacts_state.selected, 0);
    }
}
