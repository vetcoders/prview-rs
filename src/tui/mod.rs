//! TUI module for prview - Ratatui-based interactive interface.
//!
//! Provides a beautiful terminal UI for PR review and artifact generation,
//! inspired by rmcp_mux wizard patterns.

pub mod keys;
pub mod panels;
pub mod types;
pub mod ui;
pub mod widgets;

use std::io::stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use tokio::sync::mpsc;

use crate::checks::{CheckEvent, CheckResult, CheckStatus as CrateCheckStatus};
use crate::{App, Config};
use types::{TuiEvent, TuiState};

/// Run the TUI application
pub async fn run_tui(config: Config) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut out = stdout();
    if let Err(err) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(err.into());
    }
    let backend = CrosstermBackend::new(out);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = disable_raw_mode();
            let mut rollback_out = stdout();
            let _ = execute!(rollback_out, LeaveAlternateScreen);
            return Err(err.into());
        }
    };
    if let Err(err) = terminal.hide_cursor() {
        let _ = cleanup_terminal(&mut terminal);
        return Err(err.into());
    }

    let run_result = async {
        // Create app state and initialize
        let mut state = TuiState::new(config);
        initialize_state(&mut state)?;

        // Create event channel. Unbounded so a burst of check events can never
        // fill a fixed buffer and drop a CheckCompleted, which would leave a
        // check stuck rendering as "running" forever.
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();

        // Run event loop
        run_event_loop(&mut terminal, &mut state, &tx, &mut rx).await
    }
    .await;

    let cleanup_result = cleanup_terminal(&mut terminal);
    match (run_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(run_err), Ok(())) => Err(run_err),
        (Ok(()), Err(cleanup_err)) => Err(cleanup_err),
        (Err(run_err), Err(cleanup_err)) => Err(anyhow::anyhow!(
            "{run_err}; terminal cleanup failed: {cleanup_err}"
        )),
    }
}

fn cleanup_terminal(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Main event loop
async fn run_event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    tx: &mpsc::UnboundedSender<TuiEvent>,
    rx: &mut mpsc::UnboundedReceiver<TuiEvent>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(100);

    loop {
        // Draw UI (clear first to handle any stdout pollution from subprocesses)
        terminal.clear()?;
        terminal.draw(|f| ui::draw(f, state))?;

        // Check for quit
        if state.should_quit {
            break;
        }

        // Poll for events with timeout
        if event::poll(tick_rate)?
            && let Event::Key(key) = event::read()?
        {
            // Skip key release events
            if key.kind == KeyEventKind::Release {
                continue;
            }
            // Handle key event
            keys::handle_key(state, key, tx).await?;
        }

        // Process any pending async events
        while let Ok(evt) = rx.try_recv() {
            handle_tui_event(state, evt);
        }

        // Update message if running
        if state.running {
            state.update_message();
        }
    }

    Ok(())
}

/// Handle async TUI events
fn handle_tui_event(state: &mut TuiState, event: TuiEvent) {
    match event {
        TuiEvent::Tick => {
            state.update_message();
        }
        TuiEvent::Key(_) => {
            // Handled in event loop
        }
        TuiEvent::CheckStarted { name } => {
            state.update_check(&name, types::CheckLifecycle::Running);
        }
        TuiEvent::CheckCompleted { result } => {
            state.set_check_result(&result);
        }
        TuiEvent::DiffsReady { diffs } => {
            state.set_diffs(&diffs);
        }
        TuiEvent::HeuristicsReady { result } => {
            state.set_heuristics(&result);
        }
        TuiEvent::ArtifactsReady { dir } => {
            state.set_artifacts(&dir);
        }
        TuiEvent::AnalysisComplete { report } => {
            state.running = false;
            state.report = Some(report);
            // No dedicated report view exists; point at the panels that do render.
            state.message =
                "Analysis complete! [1-6] browse panels  [4] artifacts  [q]uit".to_string();
        }
        TuiEvent::Error { message } => {
            state.running = false;
            state.message = format!("Error: {}", message);
        }
    }
}

fn map_check_event(event: CheckEvent) -> TuiEvent {
    match event {
        CheckEvent::Started { name } => TuiEvent::CheckStarted { name },
        CheckEvent::Completed { result } => TuiEvent::CheckCompleted { result },
        CheckEvent::Skipped { name } => TuiEvent::CheckCompleted {
            result: Box::new(CheckResult {
                name,
                status: CrateCheckStatus::Skipped,
                duration: Duration::ZERO,
                output: "Skipped in current context.".to_string(),
                cached: false,
                provenance: None,
            }),
        },
    }
}

/// Initialize TUI state with branch info and check list
fn initialize_state(state: &mut TuiState) -> Result<()> {
    // Try to resolve branches
    let repo = crate::git::Repository::open(&state.config.repo_root)?;

    // Refresh refs (honours --no-fetch / local-only / remote-only)
    repo.prepare_refs(&state.config)?;

    // Populate branch list for wizard
    if let Ok(branch_list) = repo.list_branches() {
        state.branch_selector.local_branches =
            branch_list.local.iter().map(|b| b.name.clone()).collect();
        state.branch_selector.remote_branches =
            branch_list.remote.iter().map(|b| b.name.clone()).collect();
        state.branch_selector.current_branch = branch_list.current.clone();
    }

    if let Ok(target) = repo.resolve_target(&state.config) {
        state.target_branch = target.name;
    }

    if let Ok(bases) = repo.resolve_bases(&state.config) {
        state.base_branches = bases.iter().map(|b| b.name.clone()).collect();
    }

    // Initialize checks list from profile
    let check_names = state.config.profile.get_check_names();
    state.init_checks(&check_names);

    // Update message to include branch selection hint
    state.message = "[b]ranch wizard [r]un [1-6]panel [?]help [q]uit".to_string();

    Ok(())
}

/// Run the TUI in state-only mode (`prview state --tui`)
pub async fn run_tui_state(config: Config, repo_state: crate::state::RepoState) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut out = stdout();
    if let Err(err) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(err.into());
    }
    let backend = CrosstermBackend::new(out);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(err) => {
            let _ = disable_raw_mode();
            let mut rollback_out = stdout();
            let _ = execute!(rollback_out, LeaveAlternateScreen);
            return Err(err.into());
        }
    };
    if let Err(err) = terminal.hide_cursor() {
        let _ = cleanup_terminal(&mut terminal);
        return Err(err.into());
    }

    let run_result = async {
        let mut state = TuiState::new_state_view(config, repo_state);
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
        run_event_loop(&mut terminal, &mut state, &tx, &mut rx).await
    }
    .await;

    let cleanup_result = cleanup_terminal(&mut terminal);
    match (run_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(run_err), Ok(())) => Err(run_err),
        (Ok(()), Err(cleanup_err)) => Err(cleanup_err),
        (Err(run_err), Err(cleanup_err)) => Err(anyhow::anyhow!(
            "{run_err}; terminal cleanup failed: {cleanup_err}"
        )),
    }
}

/// Run the full analysis pipeline asynchronously.
///
/// All `git2::Repository` (non-Send) access is done synchronously before
/// the first `.await`, so the resulting future is `Send` and can be used
/// with `tokio::spawn` on a multi-threaded runtime.
pub async fn run_analysis(config: Config, tx: mpsc::UnboundedSender<TuiEvent>) -> Result<()> {
    let t_start = std::time::Instant::now();

    // --- Sync phase: all git2 (non-Send) work happens here ---
    let (config, diffs, target, bases, target_snap, base_snap, worktree_clean) = {
        let app = App::from_config(config)?;
        // Freeze cleanliness before any check runs or artifact is written (R4-19).
        let worktree_clean = app.worktree_clean_at_start;
        app.repo.prepare_refs(&app.config)?;
        let target = app.repo.resolve_target(&app.config)?;
        let bases = app.repo.resolve_bases(&app.config)?;
        let diff_bases = app
            .repo
            .resolve_diff_bases(&target, &bases, app.config.quiet);
        let diffs = app
            .repo
            .generate_diffs(&target, &diff_bases, app.config.quiet)?;

        // Create snapshots synchronously (for remote/remote-only mode)
        let target_snap = if app.config.remote_mode || app.config.remote_only {
            app.repo.create_snapshot(&target.commit_id).ok()
        } else {
            None
        };
        let base_snap = if app.config.remote_mode || app.config.remote_only {
            bases
                .first()
                .and_then(|b| app.repo.create_snapshot(&b.commit_id).ok())
        } else {
            None
        };

        let config = app.config.clone();
        // app (with git2::Repository) is dropped here
        (
            config,
            diffs,
            target,
            bases,
            target_snap,
            base_snap,
            worktree_clean,
        )
    };

    // --- Async phase: all work below is Send-safe ---

    let _ = tx.send(TuiEvent::DiffsReady {
        diffs: diffs.clone(),
    });

    // Run all checks with event callbacks for real-time updates
    let tx_checks = tx.clone();
    let (check_results, skipped_checks) =
        crate::checks::run_all_with_events(&config, move |event| {
            let tx = tx_checks.clone();
            let _ = tx.send(map_check_event(event));
        })
        .await?;

    // Run heuristics
    let heuristics = if let Some(ref snap) = target_snap {
        let analysis_root = snap.path.clone();
        // `run_all` records analysis-root provenance itself, so no
        // post-assignment is needed here.
        let mut result = crate::heuristics::run_all(&config, Some(analysis_root.as_path())).await?;

        // Base snapshot regression
        if let Some(ref base_snap) = base_snap
            && let Ok(base_result) =
                crate::heuristics::run_all(&config, Some(&base_snap.path)).await
        {
            let base_sha = bases.first().map(|b| b.commit_id.as_str()).unwrap_or("");
            // compute_delta_checked returns None when loctree was blind on a
            // side; assigning directly keeps a fabricated delta out of the TUI.
            result.regression = crate::heuristics::compute_delta_checked(
                &base_result,
                &result,
                base_sha,
                &target.commit_id,
            );
        }
        result
    } else {
        crate::heuristics::run_all(&config, None).await?
    };
    let _ = tx.send(TuiEvent::HeuristicsReady {
        result: heuristics.clone(),
    });

    // Generate artifacts
    let artifacts_dir = crate::artifacts::generate(crate::artifacts::GenerateInput {
        config: &config,
        diffs: &diffs,
        checks: &check_results,
        heuristics: Some(&heuristics),
        resolved_target: &target,
        resolved_bases: &bases,
        run_start: t_start,
        skipped_checks,
        worktree_clean,
    })?;
    let _ = tx.send(TuiEvent::ArtifactsReady {
        dir: artifacts_dir.clone(),
    });

    // Build final report
    let report = crate::output::Report {
        target: target.name,
        bases: bases.iter().map(|b| b.name.clone()).collect(),
        diffs,
        checks: check_results,
        heuristics: Some(heuristics),
        artifacts_dir,
        duration: t_start.elapsed(),
        unchanged: false,
    };

    let _ = tx.send(TuiEvent::AnalysisComplete { report });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::tui::types::CheckLifecycle;

    fn default_config() -> Config {
        test_config()
    }

    #[test]
    fn skipped_check_events_map_to_skipped_results() {
        let event = map_check_event(CheckEvent::Skipped {
            name: "TypeScript".to_string(),
        });

        match event {
            TuiEvent::CheckCompleted { result } => {
                assert_eq!(result.name, "TypeScript");
                assert_eq!(result.status, CrateCheckStatus::Skipped);
                assert_eq!(result.duration, Duration::ZERO);
            }
            other => panic!("expected skipped check to map to CheckCompleted, got {other:?}"),
        }
    }

    #[test]
    fn skipped_check_events_do_not_leave_entries_running() {
        let mut state = TuiState::new(default_config());
        state.init_checks(&["TypeScript"]);

        handle_tui_event(
            &mut state,
            map_check_event(CheckEvent::Skipped {
                name: "TypeScript".to_string(),
            }),
        );

        assert_eq!(
            state.checks_state.entries[0].status,
            CheckLifecycle::Skipped
        );
    }
}
