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
use std::sync::Arc;
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
use types::{TuiEvent, TuiState, WizardMode};

struct AnalysisTask {
    governor: Arc<crate::governor::ResourceGovernor>,
    handle: tokio::task::JoinHandle<Result<()>>,
}

#[cfg(test)]
tokio::task_local! {
    static TEST_INPUT_EVENTS: std::cell::RefCell<mpsc::UnboundedReceiver<Event>>;
}

impl AnalysisTask {
    fn spawn(config: Config, tx: mpsc::UnboundedSender<TuiEvent>) -> Self {
        let governor = Arc::new(crate::governor::ResourceGovernor::from_plan(
            config.resource_plan,
        ));
        let run_governor = Arc::clone(&governor);
        let analysis_governor = Arc::clone(&governor);
        let handle = tokio::spawn(async move {
            crate::governor::with_run_scope(
                run_governor,
                run_analysis(config, tx, analysis_governor),
            )
            .await
        });
        Self { governor, handle }
    }
}

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
        let mut analysis = None;

        // Run event loop
        run_event_loop(&mut terminal, &mut state, &tx, &mut rx, &mut analysis).await
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

async fn next_terminal_event(tick_rate: Duration) -> Result<Option<Event>> {
    #[cfg(test)]
    match TEST_INPUT_EVENTS.try_with(|events| events.borrow_mut().try_recv()) {
        Ok(Ok(event)) => return Ok(Some(event)),
        Ok(Err(mpsc::error::TryRecvError::Empty)) => {
            tokio::time::sleep(tick_rate).await;
            return Ok(None);
        }
        Ok(Err(mpsc::error::TryRecvError::Disconnected)) => return Ok(None),
        Err(_) => {}
    }

    if event::poll(tick_rate)? {
        Ok(Some(event::read()?))
    } else {
        Ok(None)
    }
}

/// Main event loop
async fn run_event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    tx: &mpsc::UnboundedSender<TuiEvent>,
    rx: &mut mpsc::UnboundedReceiver<TuiEvent>,
    analysis: &mut Option<AnalysisTask>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let result = run_event_loop_inner(terminal, state, tx, rx, analysis).await;
    join_cancelled_analysis(result, analysis).await
}

async fn run_event_loop_inner<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    tx: &mpsc::UnboundedSender<TuiEvent>,
    rx: &mut mpsc::UnboundedReceiver<TuiEvent>,
    analysis: &mut Option<AnalysisTask>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(100);

    loop {
        reap_finished_analysis(analysis, tx).await;

        // Draw UI (clear first to handle any stdout pollution from subprocesses)
        terminal.clear()?;
        terminal.draw(|f| ui::draw(f, state))?;

        // Check for quit
        if state.should_quit {
            break;
        }

        // Poll for events with timeout
        if let Some(Event::Key(key)) = next_terminal_event(tick_rate).await? {
            // Skip key release events
            if key.kind == KeyEventKind::Release {
                continue;
            }
            // Handle key event. `r` starts a run only in idle normal mode;
            // wizard filter typing and the help overlay must still reach
            // `keys::handle_key`.
            if key.code == crossterm::event::KeyCode::Char('r')
                && intercepts_run_hotkey(state, analysis)
            {
                state.running = true;
                state.start_time = Some(std::time::Instant::now());
                state.message = "Starting analysis...".to_string();
                *analysis = Some(AnalysisTask::spawn(state.config.clone(), tx.clone()));
            } else {
                keys::handle_key(state, key, tx).await?;
            }
            if state.should_quit {
                break;
            }
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

fn intercepts_run_hotkey(state: &TuiState, analysis: &Option<AnalysisTask>) -> bool {
    state.can_run_analysis
        && state.wizard_mode == WizardMode::None
        && !state.show_help
        && !state.running
        && analysis.is_none()
}

async fn join_cancelled_analysis(
    result: Result<()>,
    analysis: &mut Option<AnalysisTask>,
) -> Result<()> {
    let cancel = cancel_analysis(analysis).await;
    match (result, cancel) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
    }
}

async fn reap_finished_analysis(
    analysis: &mut Option<AnalysisTask>,
    tx: &mpsc::UnboundedSender<TuiEvent>,
) {
    if !analysis
        .as_ref()
        .is_some_and(|task| task.handle.is_finished())
    {
        return;
    }

    let task = analysis.take().expect("finished task was present");
    match task.handle.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if crate::governor::is_cancellation(&err) => {}
        Ok(Err(err)) => {
            let _ = tx.send(TuiEvent::Error {
                message: err.to_string(),
            });
        }
        Err(join_err) => {
            let _ = tx.send(TuiEvent::Error {
                message: format!("analysis task aborted: {join_err}"),
            });
        }
    }
}

async fn cancel_analysis(analysis: &mut Option<AnalysisTask>) -> Result<()> {
    let Some(task) = analysis.take() else {
        return Ok(());
    };

    task.governor.cancel();
    match task.handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(err)) if crate::governor::is_cancellation(&err) => Ok(()),
        Ok(Err(err)) => Err(err),
        Err(join_err) => Err(anyhow::anyhow!("analysis task aborted: {join_err}")),
    }
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
        TuiEvent::CheckQueued { name } => {
            state.update_check(&name, types::CheckLifecycle::Pending);
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
        // `Started` is the run considering the check; the governor may hold it
        // in the queue for minutes yet. `Running` is the one that means a
        // process began, so it is the one that lights the spinner.
        CheckEvent::Started { name } => TuiEvent::CheckQueued { name },
        CheckEvent::Running { name } => TuiEvent::CheckStarted { name },
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
        let mut analysis = None;
        run_event_loop(&mut terminal, &mut state, &tx, &mut rx, &mut analysis).await
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
pub async fn run_analysis(
    config: Config,
    tx: mpsc::UnboundedSender<TuiEvent>,
    governor: Arc<crate::governor::ResourceGovernor>,
) -> Result<()> {
    let t_start = std::time::Instant::now();

    // --- Sync phase: all git2 (non-Send) work happens here ---
    // `blocking_stage` keeps a one-worker runtime able to poll q/Escape while
    // this closure occupies the thread; git2 stays on this thread and is
    // dropped before the first `.await`.
    let (
        config,
        diffs,
        target,
        bases,
        target_snap,
        base_snap,
        worktree_clean,
        worktree_status_digest,
    ) = crate::governor::blocking_stage(|| -> Result<_> {
        let app = App::from_config(config)?;
        // Freeze cleanliness before any check runs or artifact is written (R4-19).
        let worktree_clean = app.worktree_clean_at_start;
        let worktree_status_digest = app.worktree_status_digest_at_start.clone();
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
        Ok((
            config,
            diffs,
            target,
            bases,
            target_snap,
            base_snap,
            worktree_clean,
            worktree_status_digest,
        ))
    })?;

    // --- Async phase: all work below is Send-safe ---

    ensure_analysis_active(&governor)?;

    let _ = tx.send(TuiEvent::DiffsReady {
        diffs: diffs.clone(),
    });

    // Run all checks with event callbacks for real-time updates
    let tx_checks = tx.clone();
    let ledger = crate::ledger::TaskLedger::new();
    let (check_results, skipped_checks) =
        crate::checks::run_all_with_events(&config, &ledger, &governor, move |event| {
            let tx = tx_checks.clone();
            let _ = tx.send(map_check_event(event));
        })
        .await?;
    ensure_analysis_active(&governor)?;

    // Run heuristics
    let heuristics = if let Some(ref snap) = target_snap {
        let analysis_root = snap.path.clone();
        // `run_all` records analysis-root provenance itself; only this scope
        // knows which commit that root was extracted from.
        let mut result = crate::heuristics::run_all(&config, Some(analysis_root.as_path())).await?;
        result.analysis_sha = Some(snap.sha.clone());

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
    ensure_analysis_active(&governor)?;
    let _ = tx.send(TuiEvent::HeuristicsReady {
        result: heuristics.clone(),
    });

    // Generate artifacts. Same `blocking_stage` as headless `App::run`: the
    // pipeline is synchronous and does not yield, so a single-worker runtime
    // must keep a thread free to poll the event loop (q/Escape → cancel).
    ensure_analysis_active(&governor)?;
    let artifacts_dir = crate::governor::blocking_stage(|| {
        crate::artifacts::generate(crate::artifacts::GenerateInput {
            config: &config,
            ledger: &ledger,
            diffs: &diffs,
            checks: &check_results,
            heuristics: Some(&heuristics),
            resolved_target: &target,
            resolved_bases: &bases,
            run_start: t_start,
            skipped_checks,
            worktree_clean,
            worktree_status_digest,
            governor: &governor,
        })
    })?;
    ensure_analysis_active(&governor)?;
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

    ensure_analysis_active(&governor)?;
    let _ = tx.send(TuiEvent::AnalysisComplete { report });

    Ok(())
}

fn ensure_analysis_active(governor: &crate::governor::ResourceGovernor) -> Result<()> {
    if governor.is_cancelled() {
        return Err(crate::governor::Cancelled.into());
    }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn tui_ctrl_c_cancels_joins_and_reaps_a_real_process_tree() {
        use std::io::Read;
        use std::os::unix::process::CommandExt;

        let tmp = tempfile::tempdir().expect("process-tree tempdir");
        let pidfile = tmp.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg(script).process_group(0);
        let mut child = command.spawn().expect("spawn TUI process tree");
        let root_pid = child.id();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !pidfile.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild pid was not published"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut pid = String::new();
        std::fs::File::open(&pidfile)
            .expect("open grandchild pid")
            .read_to_string(&mut pid)
            .expect("read grandchild pid");
        let grandchild_pid: i32 = pid.trim().parse().expect("numeric grandchild pid");

        let governor = Arc::new(crate::governor::ResourceGovernor::new());
        assert!(governor.register_child("tui-test", root_pid));
        let oracle_governor = Arc::clone(&governor);
        let wait_governor = Arc::clone(&governor);
        let (cancel_seen_tx, mut cancel_seen_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut release_tx = Some(release_tx);
        let handle = tokio::spawn(async move {
            wait_governor.cancelled().await;
            let _ = cancel_seen_tx.send(());
            release_rx.await.expect("release cancelled analysis join");
            Err(crate::governor::Cancelled.into())
        });
        let mut analysis = Some(AnalysisTask { governor, handle });
        let mut state = TuiState::new(default_config());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        input_tx
            .send(Event::Key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            )))
            .expect("inject quit key");

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        {
            let event_loop = TEST_INPUT_EVENTS.scope(
                std::cell::RefCell::new(input_rx),
                run_event_loop(&mut terminal, &mut state, &tx, &mut rx, &mut analysis),
            );
            tokio::pin!(event_loop);

            tokio::select! {
                biased;
                seen = &mut cancel_seen_rx => {
                    seen.expect("analysis task observed production cancellation");
                }
                result = &mut event_loop => {
                    let cancelled = oracle_governor.is_cancelled();
                    oracle_governor.cancel();
                    let _ = release_tx.take().expect("barrier sender").send(());
                    let _ = child.wait();
                    if cancelled {
                        panic!("production event loop returned without awaiting analysis join: {result:?}");
                    }
                    panic!("production event loop returned before cancelling analysis: {result:?}");
                }
            }

            if let Ok(result) =
                tokio::time::timeout(Duration::from_millis(100), &mut event_loop).await
            {
                oracle_governor.cancel();
                let _ = release_tx.take().expect("barrier sender").send(());
                let _ = child.wait();
                panic!("production event loop returned without awaiting analysis join: {result:?}");
            }

            release_tx
                .take()
                .expect("barrier sender")
                .send(())
                .expect("release analysis join");
            event_loop.await.expect("production event loop quit");
        }

        assert!(state.should_quit);
        assert!(analysis.is_none());
        let _ = child.wait();

        let gone_deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: signal 0 only probes the PID published by this fixture.
            if unsafe { libc::kill(grandchild_pid, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if matches!(errno, Some(libc::ESRCH) | Some(libc::EPERM)) {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < gone_deadline,
                "TUI quit left grandchild {grandchild_pid} alive"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, TuiEvent::AnalysisComplete { .. }),
                "cancelled analysis must not publish a completion/verdict event"
            );
        }
    }

    fn press(code: crossterm::event::KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    async fn drive_event_loop(
        state: &mut TuiState,
        analysis: &mut Option<AnalysisTask>,
        keys: impl IntoIterator<Item = Event>,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        for key in keys {
            input_tx.send(key).expect("inject key");
        }
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let run = TEST_INPUT_EVENTS.scope(
            std::cell::RefCell::new(input_rx),
            run_event_loop(&mut terminal, state, &tx, &mut rx, analysis),
        );
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("event loop timed out")
            .expect("event loop");
    }

    #[test]
    fn run_hotkey_is_not_intercepted_in_wizard_or_help() {
        let mut state = TuiState::new(default_config());
        let analysis = None;
        assert!(intercepts_run_hotkey(&state, &analysis));

        state.wizard_mode = WizardMode::SelectTarget;
        assert!(!intercepts_run_hotkey(&state, &analysis));

        state.wizard_mode = WizardMode::None;
        state.show_help = true;
        assert!(!intercepts_run_hotkey(&state, &analysis));
    }

    #[test]
    fn run_hotkey_is_not_intercepted_in_state_view() {
        let state = TuiState::new_state_view(
            default_config(),
            crate::state::RepoState {
                repo: "fixture".to_string(),
                branch: "main".to_string(),
                head: "abc1234".to_string(),
                files_changed: 0,
                insertions: 0,
                deletions: 0,
                untracked_files: 0,
                hot_files: Vec::new(),
            },
        );
        let analysis = None;
        assert!(!intercepts_run_hotkey(&state, &analysis));
    }

    #[tokio::test]
    async fn r_in_state_view_does_not_start_analysis() {
        let mut state = TuiState::new_state_view(
            default_config(),
            crate::state::RepoState {
                repo: "fixture".to_string(),
                branch: "main".to_string(),
                head: "abc1234".to_string(),
                files_changed: 0,
                insertions: 0,
                deletions: 0,
                untracked_files: 0,
                hot_files: Vec::new(),
            },
        );
        let mut analysis = None;

        drive_event_loop(
            &mut state,
            &mut analysis,
            [
                press(crossterm::event::KeyCode::Char('r')),
                press(crossterm::event::KeyCode::Char('q')),
            ],
        )
        .await;

        assert!(analysis.is_none(), "state-view r must not spawn analysis");
        assert!(!state.running);
        assert!(state.should_quit);
    }

    #[tokio::test]
    async fn r_in_wizard_types_filter_instead_of_starting_analysis() {
        let mut state = TuiState::new(default_config());
        state.wizard_mode = WizardMode::SelectTarget;
        state.branch_selector.local_branches = vec!["release".to_string(), "main".to_string()];
        let mut analysis = None;

        drive_event_loop(
            &mut state,
            &mut analysis,
            [
                press(crossterm::event::KeyCode::Char('r')),
                press(crossterm::event::KeyCode::Esc),
                press(crossterm::event::KeyCode::Char('q')),
            ],
        )
        .await;

        assert!(analysis.is_none(), "wizard r must not spawn analysis");
        assert!(!state.running);
        assert_eq!(state.branch_selector.filter, "r");
    }

    #[tokio::test]
    async fn r_in_help_overlay_does_not_start_analysis() {
        let mut state = TuiState::new(default_config());
        state.show_help = true;
        let mut analysis = None;

        drive_event_loop(
            &mut state,
            &mut analysis,
            [
                press(crossterm::event::KeyCode::Char('r')),
                press(crossterm::event::KeyCode::Char('q')),
                press(crossterm::event::KeyCode::Char('q')),
            ],
        )
        .await;

        assert!(analysis.is_none(), "help r must not spawn analysis");
        assert!(!state.running);
        assert!(state.should_quit);
    }

    #[tokio::test]
    async fn event_loop_error_cancels_and_joins_analysis() {
        let governor = Arc::new(crate::governor::ResourceGovernor::new());
        let wait_governor = Arc::clone(&governor);
        let oracle = Arc::clone(&governor);
        let handle = tokio::spawn(async move {
            wait_governor.cancelled().await;
            Err(crate::governor::Cancelled.into())
        });
        let mut analysis = Some(AnalysisTask { governor, handle });

        let result =
            join_cancelled_analysis(Err(anyhow::anyhow!("backend clear failed")), &mut analysis)
                .await;

        assert!(
            result.is_err(),
            "backend errors must still surface after cancellation"
        );
        assert!(analysis.is_none(), "error exit must join the analysis task");
        assert!(
            oracle.is_cancelled(),
            "error exit must cancel the analysis governor"
        );
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
