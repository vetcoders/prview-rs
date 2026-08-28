//! prview - PR Review & Artifact Generator
//!
//! A cross-language PR analysis tool that generates diffs, quality reports,
//! and AI-ready artifact packs.

pub mod artifacts;
pub mod cache;
pub mod check_id;
pub mod checks;
pub mod cli;
pub mod config;
pub mod gate;
pub mod git;
pub mod governor;
pub mod heuristics;
pub mod ledger;
pub mod mcp;
pub mod mdrender;
pub mod output;
pub mod paths;
pub mod policy;
pub mod proc;
pub mod regression;
pub(crate) mod rust_source;
pub mod scope;
pub mod state;
pub mod storage;
pub mod tui;

pub use cli::{
    Cli, CliCommand, CompletionsArgs, GateArgs, OpenArgs, RunsArgs, ScopeArgs, StateArgs,
};
pub use config::Config;

use anyhow::Result;
use std::time::Instant;

/// Main application context holding all state
pub struct App {
    pub config: Config,
    pub repo: git::Repository,
    pub(crate) start_time: Instant,
    /// Working-tree cleanliness captured at construction — before ref refresh,
    /// checks, or artifact writes touch the tree (R4-19). Frozen so the
    /// pre-existing downgrade judges the scanned source state, not tool output
    /// (an in-repo `--output-dir` or an untracked check cache) that appears
    /// later in the run. `None` when the status could not be read at all.
    pub(crate) worktree_clean_at_start: Option<bool>,
    /// Fingerprint of what was uncommitted at that same moment, from the same
    /// status read. Recorded in `00_summary/PROVENANCE.json` so a reviewer can
    /// tell two dirty runs apart instead of only knowing "dirty".
    pub(crate) worktree_status_digest_at_start: Option<String>,
    /// The ONE machine-wide budget for this run, and the registry of the
    /// children it spawned.
    ///
    /// It lives here rather than inside `checks::run_all` because two callers
    /// need it and neither is that function: the checks stage and the artifact
    /// stage both put load on the same machine, so a governor scoped to one of
    /// them would be a budget per stage — the exact thing it exists to replace.
    /// The signal handler in `main` needs it too, and can only reach it through
    /// something that outlives the run future.
    governor: std::sync::Arc<governor::ResourceGovernor>,
}

impl App {
    fn should_emit_human_stdout(&self) -> bool {
        // Human progress is emitted only when neither machine mode is active:
        // --json keeps stdout machine-only (human progress would interleave with
        // the JSON payload and make stdout unparseable), and --quiet suppresses
        // the interactive banner/progress stream the CLI advertises it silences.
        !self.config.json && !self.config.quiet
    }

    /// Create new App instance from CLI arguments
    pub fn new(cli: &Cli) -> Result<Self> {
        let config = Config::from_cli(cli)?;
        Self::from_config(config)
    }

    /// Create new App instance from Config
    pub fn from_config(config: Config) -> Result<Self> {
        let repo = git::Repository::open(&config.repo_root)?;
        // Freeze cleanliness now — before any check runs or artifact is written
        // (R4-19).
        let worktree = artifacts::capture_worktree_provenance(&config.repo_root);
        let governor =
            std::sync::Arc::new(governor::ResourceGovernor::from_plan(config.resource_plan));

        Ok(Self {
            config,
            repo,
            start_time: Instant::now(),
            worktree_clean_at_start: worktree.clean,
            worktree_status_digest_at_start: worktree.status_digest,
            governor,
        })
    }

    /// This run's resource governor, for a supervisor that has to cancel it.
    #[must_use]
    pub fn governor(&self) -> std::sync::Arc<governor::ResourceGovernor> {
        std::sync::Arc::clone(&self.governor)
    }

    /// End the run here if a cancel has been asked for.
    ///
    /// The checks stage notices cancellation on its own — it has a `select!` arm
    /// for it — but it is one stage of several, and a cancel that lands anywhere
    /// else used to be ignored completely: the heuristics ran, the artifact
    /// stage wrote a pack whose context commands were all recorded `cancelled`,
    /// and `main` then computed an exit code from the verdict in it. The
    /// operator asked the run to stop and was handed an ACCEPT.
    ///
    /// So the contract is absolute, and this is what enforces it between the
    /// stages: a run in which cancellation was requested NEVER ends in a
    /// verdict. It ends in [`governor::Cancelled`], which `main` turns into
    /// [`governor::CANCELLED_EXIT_CODE`]. A partial pack may survive on disk —
    /// it is evidence of what got done — but nothing claims a verdict from it.
    fn ensure_not_cancelled(&self) -> Result<()> {
        if self.governor.is_cancelled() {
            return Err(governor::Cancelled.into());
        }
        Ok(())
    }

    /// Run the PR review process
    pub async fn run(&self) -> Result<output::Report> {
        use colored::Colorize;
        // The governor may already be closed before the run starts a single
        // stage — `--watch` shares one across iterations, and a cancel can land
        // between `App` construction and here. Asked first so the run does no
        // work it has already been told to abandon.
        self.ensure_not_cancelled()?;
        let emit_human_stdout = self.should_emit_human_stdout();

        if emit_human_stdout {
            println!(
                "{}",
                "=== prview - PR Review & Artifact Generator ==="
                    .cyan()
                    .bold()
            );
            println!();
        }

        // 1. Refresh refs when remote/default fetch behavior is enabled
        self.ensure_not_cancelled()?;
        self.repo.prepare_refs(&self.config)?;
        self.ensure_not_cancelled()?;

        // 2. Resolve target and base refs
        let target = self.repo.resolve_target(&self.config)?;
        let bases = if self.config.current_only {
            Vec::new()
        } else {
            self.repo.resolve_bases(&self.config)?
        };
        self.ensure_not_cancelled()?;

        if emit_human_stdout {
            output::print_config(&self.config, &target, &bases);
        }

        // 3. Check for update mode
        self.ensure_not_cancelled()?;
        if self.config.update_mode
            && let Some(prev_run) = self.find_previous_run()?
            && let Some(report) =
                self.reuse_unchanged_run(&target, &bases, prev_run, emit_human_stdout)?
        {
            return Ok(report);
        }

        // 4. Generate diffs
        let diff_bases = self
            .repo
            .resolve_diff_bases(&target, &bases, self.config.quiet);
        let diffs = self
            .repo
            .generate_diffs(&target, &diff_bases, self.config.quiet)?;
        self.ensure_not_cancelled()?;

        // 5. Run checks (reduced set in update mode).
        // The ledger is the run's record of what work was considered and how it
        // resolved. It also OWNS the run's shared target snapshot, so it must
        // outlive artifact generation (step 7), which reads that snapshot.
        let ledger = ledger::TaskLedger::new();
        let (check_results, skipped_checks) = if self.config.update_mode {
            // In update mode, skip heavy checks UNLESS user explicitly forced them
            // via --with-tests or --with-security (respect user intent over preset)
            let mut update_config = self.config.clone();
            let any_skipped = !self.config.run_tests || !self.config.run_security;
            if !self.config.run_tests {
                // Only disable if not already force-enabled by --with-tests
                update_config.run_tests = false;
            }
            update_config.run_bundle = false;
            if !self.config.run_security {
                update_config.run_security = false;
            }
            if emit_human_stdout && any_skipped {
                println!("{}", "  Skipping heavy checks (--update mode)".yellow());
            }
            checks::run_all(&update_config, &ledger, &self.governor).await?
        } else {
            checks::run_all(&self.config, &ledger, &self.governor).await?
        };
        // A cancel that arrived while nothing was running — a run whose gates all
        // replayed from the cache never builds the dispatcher's `select!` loop at
        // all — reaches the run here and nowhere earlier.
        self.ensure_not_cancelled()?;

        // 6. Run heuristics (loctree-suite)
        // In remote/remote-only mode, use git snapshots for deterministic analysis.
        // Feed the SAME merge-base range the artifact diff uses (`diff_bases`), not
        // the raw base tips: when the base branch has advanced with unrelated work,
        // snapshotting the tip would compute the regression delta against base-only
        // files the patch excludes, fabricating regressions/caveats. All signals
        // must share one range.
        self.ensure_not_cancelled()?;
        let heuristics_result = if self.config.remote_mode || self.config.remote_only {
            self.run_heuristics_with_snapshots(&target, &diff_bases)
                .await?
        } else {
            heuristics::run_all(&self.config, None).await?
        };
        self.ensure_not_cancelled()?;

        // 7. Generate artifacts. The ledger is still alive here, and with it the
        // run's shared target snapshot — that is what lets the context
        // generators read the reviewed tree instead of the local checkout.
        //
        // `blocking_stage`: everything below is synchronous and does not yield
        // for as long as it runs, so the runtime is told to keep a thread free
        // for the interrupt supervisor.
        self.ensure_not_cancelled()?;
        let artifacts_dir = governor::blocking_stage(|| {
            artifacts::generate(artifacts::GenerateInput {
                config: &self.config,
                ledger: &ledger,
                diffs: &diffs,
                checks: &check_results,
                heuristics: Some(&heuristics_result),
                resolved_target: &target,
                resolved_bases: &bases,
                run_start: self.start_time,
                skipped_checks,
                worktree_clean: self.worktree_clean_at_start,
                worktree_status_digest: self.worktree_status_digest_at_start.clone(),
                governor: &self.governor,
            })
        })?;
        // A cancel DURING the artifact stage leaves a pack whose context
        // commands are all recorded `cancelled`. It is a partial pack, not a
        // review, so the run must not go on to summarise it as one.
        self.ensure_not_cancelled()?;

        // 8. Build report
        let report = output::Report {
            target: target.name.clone(),
            bases: bases.iter().map(|b| b.name.clone()).collect(),
            diffs,
            checks: check_results,
            heuristics: Some(heuristics_result),
            artifacts_dir,
            duration: self.start_time.elapsed(),
            unchanged: false,
        };

        if emit_human_stdout {
            output::print_summary(&report);
        }

        Ok(report)
    }

    /// Run heuristics using git archive snapshots for deterministic results.
    ///
    /// Creates temporary snapshots of target (and optionally base) commits,
    /// runs heuristics against extracted trees instead of the working directory,
    /// and computes regression delta when both snapshots are available.
    pub(crate) async fn run_heuristics_with_snapshots(
        &self,
        target: &git::ResolvedRef,
        bases: &[git::ResolvedRef],
    ) -> Result<heuristics::HeuristicsResult> {
        if !self.config.run_heuristics {
            return Ok(heuristics::HeuristicsResult::default());
        }

        use colored::Colorize;
        let emit = self.should_emit_human_stdout();
        // Clone config so &self is not held across async await points,
        // keeping the future Send-compatible for tokio::spawn in TUI mode.
        let config = self.config.clone();

        // 1. Create target snapshot (required — fallback to cwd on failure)
        let target_snap = match self.repo.create_snapshot(&target.commit_id) {
            Ok(snap) => {
                if emit {
                    println!(
                        "  {} Snapshot (target): {} → {}",
                        "ℹ".blue(),
                        git::short_sha(&target.commit_id),
                        snap.path.display()
                    );
                }
                Some(snap)
            }
            Err(e) => {
                if emit {
                    eprintln!(
                        "  {} Snapshot failed for target {}: {} — falling back to working tree",
                        "⚠".yellow(),
                        git::short_sha(&target.commit_id),
                        e
                    );
                }
                None
            }
        };

        let analysis_root = target_snap.as_ref().map(|s| s.path.as_path());

        // 2. Run heuristics on target. `run_all` records the analysis-root
        //    provenance itself (see heuristics::run_all); the commit that root
        //    materialises is read off the snapshot here, since only this scope
        //    holds it.
        let mut result = heuristics::run_all(&config, analysis_root).await?;
        result.analysis_sha = target_snap.as_ref().map(|snap| snap.sha.clone());

        // 3. Try base snapshot for regression detection in heavier modes only.
        if should_compute_snapshot_regression(&self.config)
            && let Some(base) = bases.first()
        {
            match self.repo.create_snapshot(&base.commit_id) {
                Ok(base_snap) => {
                    if emit {
                        println!(
                            "  {} Snapshot (base): {} → {}",
                            "ℹ".blue(),
                            git::short_sha(&base.commit_id),
                            base_snap.path.display()
                        );
                    }

                    match heuristics::run_all(&config, Some(&base_snap.path)).await {
                        Ok(base_result) => {
                            match heuristics::compute_delta_checked(
                                &base_result,
                                &result,
                                &base.commit_id,
                                &target.commit_id,
                            ) {
                                Some(regression) => {
                                    if emit {
                                        let symbol = if regression.regression_detected {
                                            "⚠".yellow()
                                        } else if regression.improvement_detected {
                                            "✓".green()
                                        } else {
                                            "─".dimmed()
                                        };
                                        println!(
                                            "  {} Regression: dead_exports={:+}, cycles={:+}, unused_symbols={:+}",
                                            symbol,
                                            regression.dead_exports_delta,
                                            regression.cycles_delta,
                                            regression.unused_symbols_delta(),
                                        );
                                    }

                                    result.regression = Some(regression);
                                }
                                None => {
                                    // Loctree was blind on at least one side — no
                                    // honest delta exists, so emit no regression
                                    // rather than a fabricated one.
                                    if emit {
                                        println!(
                                            "  {} Regression: loctree signal unavailable — skipped",
                                            "○".dimmed(),
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if emit {
                                eprintln!(
                                    "  {} Base heuristics failed: {} — skipping regression",
                                    "⚠".yellow(),
                                    e
                                );
                            }
                        }
                    }
                    // base_snap dropped here → auto-cleanup
                }
                Err(e) => {
                    if emit {
                        eprintln!(
                            "  {} Snapshot failed for base {}: {} — skipping regression",
                            "⚠".yellow(),
                            git::short_sha(&base.commit_id),
                            e
                        );
                    }
                }
            }
        }

        // target_snap dropped here → auto-cleanup
        Ok(result)
    }

    /// Run in watch mode - monitor changes and regenerate
    pub async fn run_watch(&self) -> Result<()> {
        use colored::Colorize;
        use std::time::Duration;

        let emit_human_stdout = self.should_emit_human_stdout();
        let mut last_hash = String::new();

        if emit_human_stdout {
            println!("{}", "=== prview Watch Mode ===".cyan().bold());
            println!("{} Monitoring for changes (Ctrl+C to stop)...", "ℹ".blue());
            println!();
        }

        self.run_watch_iteration(&mut last_hash, emit_human_stdout)
            .await?;

        match self.init_repo_watcher() {
            Ok((_watcher, mut receiver)) => {
                if emit_human_stdout {
                    println!(
                        "{} Filesystem watcher active (hash fallback every 30s)",
                        "ℹ".blue()
                    );
                }

                let debounce_window = Duration::from_millis(350);
                let fallback_interval = Duration::from_secs(30);

                loop {
                    tokio::select! {
                        biased;

                        // A cancelled watcher has nothing left to run. One
                        // governor is shared by every iteration and closing it
                        // is one-way, so without this arm the first Ctrl-C left
                        // the watcher alive on a budget that could never grant
                        // work again: each later edit produced a pack with an
                        // empty context stage and printed "Regenerated
                        // artifacts" over it, until the operator interrupted a
                        // second time. Biased so the cancel wins a race with an
                        // edit that landed at the same moment.
                        () = self.governor.cancelled() => {
                            return Err(governor::Cancelled.into());
                        }

                        maybe_signal = receiver.recv() => match maybe_signal {
                            Some(WatchSignal::FilesChanged) => {
                                tokio::time::sleep(debounce_window).await;
                                self.drain_watch_queue(&mut receiver, emit_human_stdout);
                                self.run_watch_iteration(&mut last_hash, emit_human_stdout).await?;
                            }
                            Some(WatchSignal::WatchError(err)) => {
                                if emit_human_stdout {
                                    eprintln!(
                                        "{} Watcher error: {} — checking repo state anyway",
                                        "⚠".yellow(),
                                        err
                                    );
                                }
                                self.run_watch_iteration(&mut last_hash, emit_human_stdout).await?;
                            }
                            None => {
                                if emit_human_stdout {
                                    eprintln!(
                                        "{} Watcher channel closed — falling back to 5s polling",
                                        "⚠".yellow()
                                    );
                                }
                                return self
                                    .run_watch_polling(
                                        &mut last_hash,
                                        Duration::from_secs(5),
                                        emit_human_stdout,
                                    )
                                    .await;
                            }
                        },
                        _ = tokio::time::sleep(fallback_interval) => {
                            self.run_watch_iteration(&mut last_hash, emit_human_stdout).await?;
                        }
                    }
                }
            }
            Err(err) => {
                if emit_human_stdout {
                    eprintln!(
                        "{} Filesystem watcher unavailable: {} — falling back to 5s polling",
                        "⚠".yellow(),
                        err
                    );
                }
                self.run_watch_polling(&mut last_hash, Duration::from_secs(5), emit_human_stdout)
                    .await
            }
        }
    }

    /// Quick run for watch mode (skip heavy checks)
    async fn run_quick(&self) -> Result<output::Report> {
        let run_started_at = Instant::now();
        // `--watch` reuses one governor across every iteration, and closing it is
        // one-way, so a cancelled watcher must not start another pack.
        self.ensure_not_cancelled()?;
        // `--watch` builds ONE App and then produces a pack per detected edit,
        // so the state frozen at construction describes the tree as it was when
        // the watcher started — by definition not the tree that just changed.
        // Each iteration is its own run and freezes its own worktree state, at
        // the start of that run and before this pack is written.
        let worktree = artifacts::capture_worktree_provenance(&self.config.repo_root);
        let target = self.repo.resolve_target(&self.config)?;
        let bases = if self.config.current_only {
            Vec::new()
        } else {
            self.repo.resolve_bases(&self.config)?
        };
        let diff_bases = self
            .repo
            .resolve_diff_bases(&target, &bases, self.config.quiet);
        let diffs = self
            .repo
            .generate_diffs(&target, &diff_bases, self.config.quiet)?;

        // Skip checks and heuristics in quick mode. No checks run, so no shared
        // snapshot is ever materialised: an empty ledger is the honest input,
        // and the context generators read the working tree — which is exactly
        // what `--watch` is watching.
        let ledger = ledger::TaskLedger::new();
        let artifacts_dir = governor::blocking_stage(|| {
            artifacts::generate(artifacts::GenerateInput {
                config: &self.config,
                ledger: &ledger,
                diffs: &diffs,
                checks: &[],
                heuristics: None,
                resolved_target: &target,
                resolved_bases: &bases,
                run_start: run_started_at,
                skipped_checks: vec![],
                worktree_clean: worktree.clean,
                worktree_status_digest: worktree.status_digest.clone(),
                governor: &self.governor,
            })
        })?;
        self.ensure_not_cancelled()?;

        Ok(output::Report {
            target: target.name.clone(),
            bases: bases.iter().map(|b| b.name.clone()).collect(),
            diffs,
            checks: vec![],
            heuristics: None,
            artifacts_dir,
            duration: run_started_at.elapsed(),
            unchanged: false,
        })
    }

    fn get_repo_state_hash(&self) -> Result<String> {
        use crate::git::git_cmd;

        let head = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.config.repo_root)
            .output()?;

        let status = git_cmd()
            .args(["status", "--porcelain"])
            .current_dir(&self.config.repo_root)
            .output()?;

        let diff = git_cmd()
            .args(["diff", "--no-ext-diff", "--stat"])
            .current_dir(&self.config.repo_root)
            .output()?;

        let head_str = String::from_utf8_lossy(&head.stdout);
        let status_str = String::from_utf8_lossy(&status.stdout);
        let diff_str = String::from_utf8_lossy(&diff.stdout);

        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(status_str.as_bytes());
        hasher.update(diff_str.as_bytes());
        let status_hash = format!("{:x}", hasher.finalize());

        Ok(format!("{}:{}", head_str.trim(), &status_hash[..16]))
    }

    async fn run_watch_iteration(
        &self,
        last_hash: &mut String,
        emit_human_stdout: bool,
    ) -> Result<()> {
        use colored::Colorize;

        // Asked before the change is even measured: a cancelled watcher is over,
        // and reading the repo state for it would only delay saying so.
        self.ensure_not_cancelled()?;

        let current_hash = self.get_repo_state_hash()?;
        if current_hash == *last_hash {
            return Ok(());
        }

        if emit_human_stdout {
            println!(
                "\n{} Change detected at {}",
                "→".yellow(),
                chrono::Local::now().format("%H:%M:%S")
            );
        }

        match self.run_quick().await {
            Ok(_) => {
                if emit_human_stdout {
                    println!("{} Regenerated artifacts", "✓".green());
                }
            }
            // A cancelled run is not a failed iteration — it is the end of the
            // watch, and the only thing that can end it. Reporting it as an
            // ordinary error and carrying on is what kept `--watch` running
            // after the first Ctrl-C.
            Err(e) if governor::is_cancellation(&e) => return Err(e),
            Err(e) => {
                if emit_human_stdout {
                    println!("{} Error: {}", "✗".red(), e);
                }
            }
        }

        *last_hash = self.get_repo_state_hash().unwrap_or(current_hash);

        if emit_human_stdout {
            println!("\n{} Waiting for changes...", "ℹ".blue());
        }

        Ok(())
    }

    async fn run_watch_polling(
        &self,
        last_hash: &mut String,
        interval: std::time::Duration,
        emit_human_stdout: bool,
    ) -> Result<()> {
        loop {
            tokio::select! {
                biased;

                // Same reason as the watcher loop above: a cancel must end the
                // polling fallback too, not wait out its next interval.
                () = self.governor.cancelled() => {
                    return Err(governor::Cancelled.into());
                }

                () = tokio::time::sleep(interval) => {}
            }
            self.run_watch_iteration(last_hash, emit_human_stdout)
                .await?;
        }
    }

    fn init_repo_watcher(
        &self,
    ) -> Result<(
        notify::RecommendedWatcher,
        tokio::sync::mpsc::UnboundedReceiver<WatchSignal>,
    )> {
        use notify::{RecursiveMode, Watcher};

        let repo_root = self.config.repo_root.clone();
        let ignored_output_dir = self
            .config
            .output_dir
            .as_ref()
            .filter(|dir| dir.starts_with(&repo_root))
            .cloned();

        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let event_sender = sender.clone();

        let mut watcher = notify::recommended_watcher(
            move |result: notify::Result<notify::Event>| match result {
                Ok(event) => {
                    if should_ignore_watch_event(&repo_root, ignored_output_dir.as_deref(), &event)
                    {
                        return;
                    }
                    let _ = event_sender.send(WatchSignal::FilesChanged);
                }
                Err(err) => {
                    let _ = sender.send(WatchSignal::WatchError(err.to_string()));
                }
            },
        )?;

        watcher.watch(&self.config.repo_root, RecursiveMode::Recursive)?;
        Ok((watcher, receiver))
    }

    fn drain_watch_queue(
        &self,
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<WatchSignal>,
        emit_human_stdout: bool,
    ) {
        use colored::Colorize;

        while let Ok(signal) = receiver.try_recv() {
            if let WatchSignal::WatchError(err) = signal
                && emit_human_stdout
            {
                eprintln!("{} Watcher error: {}", "⚠".yellow(), err);
            }
        }
    }

    /// The `--update` short circuit: when HEAD has not moved since `prev_run`,
    /// hand that pack back instead of building a new one.
    ///
    /// `Ok(None)` means the previous run is stale and the caller should go on to
    /// review incrementally.
    ///
    /// This is the one early return in [`App::run`] that reaches `main` with a
    /// report without passing a single `ensure_not_cancelled` — every other gate
    /// sits after the checks stage. `prepare_refs` runs a `git fetch` that is not
    /// a registered child of the governor, so `cancel` cannot kill it; a Ctrl-C
    /// during that fetch on a repo with no new commits printed "^C stopping..."
    /// and then reused the previous pack, and `main` computed an ACCEPT or a
    /// BLOCK from it. The gate below is what makes the contract hold here too: a
    /// run in which cancellation was requested never ends in a verdict, not even
    /// one it is only quoting from last time.
    ///
    /// Split out of `run` so the seam can be driven with a fabricated previous
    /// run: `find_previous_run` resolves through `PRVIEW_HOME`, and a library
    /// test must not reach the operator's real one.
    fn reuse_unchanged_run(
        &self,
        target: &git::ResolvedRef,
        bases: &[git::ResolvedRef],
        prev_run: std::path::PathBuf,
        emit_human_stdout: bool,
    ) -> Result<Option<output::Report>> {
        use colored::Colorize;

        let prev_head = self.read_previous_head(&prev_run)?;
        let current_head = target.commit_id.clone();

        if commit_ids_match(&prev_head, &current_head) {
            self.ensure_not_cancelled()?;
            if emit_human_stdout {
                println!(
                    "{} No new commits since last run (HEAD: {})",
                    "ℹ".blue(),
                    git::short_sha(&current_head)
                );
                println!("{} Previous artifacts: {}", "ℹ".blue(), prev_run.display());
                println!("{} Nothing to update.", "ℹ".blue());
            }
            return Ok(Some(output::Report {
                target: target.name.clone(),
                bases: bases.iter().map(|b| b.name.clone()).collect(),
                diffs: vec![],
                checks: vec![],
                heuristics: None,
                artifacts_dir: prev_run,
                duration: self.start_time.elapsed(),
                unchanged: true,
            }));
        }

        if emit_human_stdout {
            println!(
                "{} Found previous run, updating incrementally...",
                "ℹ".blue()
            );
        }
        Ok(None)
    }

    fn find_previous_run(&self) -> Result<Option<std::path::PathBuf>> {
        let artifacts_base = self.config.artifacts_base();

        if !artifacts_base.exists() {
            return Ok(None);
        }

        let mut entries: Vec<_> = std::fs::read_dir(&artifacts_base)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir() && !e.path().is_symlink())
            .collect();

        entries.sort_by_key(|e| std::cmp::Reverse(e.path()));

        Ok(entries.first().map(|e| e.path()))
    }

    fn read_previous_head(&self, prev_run: &std::path::Path) -> Result<String> {
        // Try new layout first, fall back to legacy path
        let metadata_file = {
            let new_path = prev_run.join("00_summary/pr-metadata.txt");
            if new_path.exists() {
                std::path::PathBuf::from("00_summary/pr-metadata.txt")
            } else {
                std::path::PathBuf::from("ai-context/pr-metadata.txt")
            }
        };
        let content = crate::paths::read_to_string_within(prev_run, &metadata_file)?;

        for line in content.lines() {
            if line.starts_with("HEAD:")
                && let Some(start) = line.rfind('(')
                && let Some(end) = line.rfind(')')
            {
                return Ok(line[start + 1..end].to_string());
            }
        }

        anyhow::bail!("Could not parse HEAD from previous run metadata")
    }
}

fn should_compute_snapshot_regression(config: &Config) -> bool {
    matches!(
        config.execution_mode,
        crate::cli::ExecutionMode::Deep | crate::cli::ExecutionMode::Ci
    )
}

fn commit_ids_match(previous: &str, current: &str) -> bool {
    let previous = previous.trim();
    let current = current.trim();

    previous == current || current.starts_with(previous) || previous.starts_with(current)
}

enum WatchSignal {
    FilesChanged,
    WatchError(String),
}

fn should_ignore_watch_event(
    repo_root: &std::path::Path,
    ignored_output_dir: Option<&std::path::Path>,
    event: &notify::Event,
) -> bool {
    if matches!(event.kind, notify::EventKind::Access(_)) {
        return true;
    }

    ignored_output_dir.is_some_and(|output_dir| {
        output_dir.starts_with(repo_root)
            && !event.paths.is_empty()
            && event.paths.iter().all(|path| path.starts_with(output_dir))
    })
}

#[cfg(test)]
mod tests {
    use super::{commit_ids_match, should_compute_snapshot_regression, should_ignore_watch_event};
    use crate::cli::ExecutionMode;
    use crate::config::test_config;
    use notify::EventKind;
    use notify::event::{AccessKind, CreateKind, EventAttributes};
    use std::path::PathBuf;

    #[test]
    fn snapshot_regression_is_disabled_for_standard_mode() {
        let mut config = test_config();
        config.execution_mode = ExecutionMode::Standard;
        assert!(!should_compute_snapshot_regression(&config));
    }

    #[test]
    fn snapshot_regression_stays_enabled_for_deep_and_ci_modes() {
        let mut config = test_config();
        config.execution_mode = ExecutionMode::Deep;
        assert!(should_compute_snapshot_regression(&config));

        config.execution_mode = ExecutionMode::Ci;
        assert!(should_compute_snapshot_regression(&config));
    }

    #[test]
    fn watch_ignores_access_only_events() {
        let repo_root = PathBuf::from("/tmp/repo");
        let event = notify::Event {
            kind: EventKind::Access(AccessKind::Any),
            paths: vec![repo_root.join("src/lib.rs")],
            attrs: EventAttributes::default(),
        };

        assert!(should_ignore_watch_event(&repo_root, None, &event));
    }

    #[test]
    fn watch_ignores_output_dir_changes_inside_repo() {
        let repo_root = PathBuf::from("/tmp/repo");
        let output_dir = repo_root.join("tmp-artifacts");
        let event = notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![output_dir.join("report.json")],
            attrs: EventAttributes::default(),
        };

        assert!(should_ignore_watch_event(
            &repo_root,
            Some(output_dir.as_path()),
            &event
        ));
    }

    #[test]
    fn watch_keeps_source_file_changes() {
        let repo_root = PathBuf::from("/tmp/repo");
        let output_dir = repo_root.join("tmp-artifacts");
        let event = notify::Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![repo_root.join("src/lib.rs")],
            attrs: EventAttributes::default(),
        };

        assert!(!should_ignore_watch_event(
            &repo_root,
            Some(output_dir.as_path()),
            &event
        ));
    }

    #[test]
    fn commit_ids_match_accepts_legacy_short_sha_metadata() {
        assert!(commit_ids_match("abc1234", "abc1234def56789"));
        assert!(commit_ids_match("abc1234def56789", "abc1234"));
        assert!(commit_ids_match("abc1234def56789", "abc1234def56789"));
        assert!(!commit_ids_match("abc1234", "def5678"));
    }

    fn git_run(repo: &std::path::Path, args: &[&str]) {
        let status = crate::git::git_cmd()
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {:?} failed", args);
    }

    fn rev_parse(repo: &std::path::Path, rev: &str) -> String {
        let out = crate::git::git_cmd()
            .args(["rev-parse", rev])
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git rev-parse {} failed", rev);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn resolved(sha: &str) -> crate::git::ResolvedRef {
        crate::git::ResolvedRef {
            name: sha[..7.min(sha.len())].to_string(),
            commit_id: sha.to_string(),
            is_remote: false,
        }
    }

    /// `--watch` reuses ONE `App` for every pack it emits, so worktree state
    /// frozen at construction describes the tree as it was when the watcher
    /// started — never the edit that triggered this iteration. Each quick run
    /// must freeze its own.
    #[tokio::test]
    async fn watch_iterations_record_their_own_worktree_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_run(repo, &["init", "-q", "-b", "main"]);
        git_run(repo, &["config", "user.email", "t@t.t"]);
        git_run(repo, &["config", "user.name", "T"]);
        git_run(repo, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
        git_run(repo, &["add", "."]);
        git_run(repo, &["commit", "-q", "-m", "base"]);
        git_run(repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("src/lib.rs"), "pub fn keep() {}\n// edit\n").unwrap();
        git_run(repo, &["commit", "-qam", "feature"]);

        let out = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.repo_root = repo.to_path_buf();
        config.target = Some("feature".to_string());
        config.bases = vec!["main".to_string()];
        config.output_dir = Some(out.path().to_path_buf());
        config.run_heuristics = false;
        config.quiet = true;
        config.create_zip = false;

        // The App is built while the tree is clean — as a watcher's is.
        let app = crate::App::from_config(config).unwrap();
        assert_eq!(
            app.worktree_clean_at_start,
            Some(true),
            "fixture starts clean"
        );

        // A watched edit lands, then the iteration runs.
        std::fs::write(
            repo.join("src/lib.rs"),
            "pub fn keep() {}\n// watched edit\n",
        )
        .unwrap();
        let report = app.run_quick().await.unwrap();

        let provenance: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(report.artifacts_dir.join("00_summary/PROVENANCE.json"))
                .expect("PROVENANCE.json"),
        )
        .expect("parse PROVENANCE.json");

        assert_eq!(
            provenance["worktree"]["clean"], false,
            "the pack must describe the tree this iteration ran on, not the one \
             the watcher started with",
        );
        assert_ne!(
            provenance["worktree"]["status_digest"].as_str(),
            app.worktree_status_digest_at_start.as_deref(),
            "a re-frozen digest must differ from the watcher's start-of-process one",
        );
    }

    /// Build a two-commit repo and a config that reviews `feature` against
    /// `main`, writing its pack into `out`.
    fn reviewable_repo(repo: &std::path::Path, out: &std::path::Path) -> crate::Config {
        git_run(repo, &["init", "-q", "-b", "main"]);
        git_run(repo, &["config", "user.email", "t@t.t"]);
        git_run(repo, &["config", "user.name", "T"]);
        git_run(repo, &["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
        git_run(repo, &["add", "."]);
        git_run(repo, &["commit", "-q", "-m", "base"]);
        git_run(repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("src/lib.rs"), "pub fn keep() {}\n// edit\n").unwrap();
        git_run(repo, &["commit", "-qam", "feature"]);

        let mut config = test_config();
        config.repo_root = repo.to_path_buf();
        config.target = Some("feature".to_string());
        config.bases = vec!["main".to_string()];
        config.output_dir = Some(out.to_path_buf());
        config.run_heuristics = false;
        config.do_fetch = false;
        config.quiet = true;
        config.create_zip = false;
        config
    }

    /// The contract behind exit 130, pinned end to end: a run in which
    /// cancellation was requested NEVER ends in a verdict. The gates stage
    /// notices a cancel on its own, but it is one stage of several — a cancel
    /// landing between them used to be ignored outright, and the operator who
    /// asked the run to stop was handed an ACCEPT or a BLOCK computed from a
    /// pack the run had kept on building.
    ///
    /// Which stage catches it here depends on the machine (semgrep is runnable
    /// wherever the binary is on `PATH`, whatever the flags say), so this pins
    /// the outcome rather than the seam. The seam the checks stage cannot cover
    /// is pinned by `a_cancelled_watch_iteration_produces_no_pack` below, which
    /// runs no checks at all.
    #[tokio::test]
    async fn a_cancelled_run_never_reports_a_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let config = reviewable_repo(tmp.path(), out.path());

        let app = crate::App::from_config(config).unwrap();
        app.governor().cancel();

        let err = app
            .run()
            .await
            .expect_err("a cancelled run must not return a report to take a verdict from");
        assert!(
            crate::governor::is_cancellation(&err),
            "the run must end as cancelled, not as some other failure: {err:?}",
        );
    }

    /// `--watch` shares one governor across every iteration and closing it is
    /// one-way, so a cancelled watcher must refuse the next pack instead of
    /// quietly emitting one with an empty context stage.
    #[tokio::test]
    async fn a_cancelled_watch_iteration_produces_no_pack() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let config = reviewable_repo(tmp.path(), out.path());

        let app = crate::App::from_config(config).unwrap();
        app.governor().cancel();

        let err = app
            .run_quick()
            .await
            .expect_err("a cancelled watcher must not regenerate artifacts");
        assert!(crate::governor::is_cancellation(&err), "{err:?}");
        assert_eq!(
            std::fs::read_dir(out.path()).unwrap().count(),
            0,
            "nothing may be written for a run that was already cancelled",
        );
    }

    /// The first Ctrl-C must END `--watch`, not degrade it. The iteration used
    /// to report every failure from the quick run as an ordinary error and carry
    /// on, so a cancelled watcher stayed alive on a governor that can never
    /// grant work again: each later edit produced a pack with an empty context
    /// stage under a cheerful "Regenerated artifacts", until the operator
    /// interrupted a second time and took the cleanup with them.
    #[tokio::test]
    async fn a_cancelled_watch_iteration_ends_the_watch() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let config = reviewable_repo(tmp.path(), out.path());

        let app = crate::App::from_config(config).unwrap();
        app.governor().cancel();

        let mut last_hash = String::new();
        let err = app
            .run_watch_iteration(&mut last_hash, false)
            .await
            .expect_err("a cancelled iteration must end the watch, not be reported and skipped");
        assert!(crate::governor::is_cancellation(&err), "{err:?}");
        assert_eq!(
            std::fs::read_dir(out.path()).unwrap().count(),
            0,
            "a cancelled watcher regenerates nothing",
        );
    }

    /// `--update` is the one path that returns a report without running a
    /// single stage, and so the one that reached `main` with a verdict while
    /// the operator was pressing Ctrl-C. The interrupt lands during
    /// `prepare_refs` — a `git fetch` the governor holds no pid for, so `cancel`
    /// cannot cut it short — and on a HEAD with no new commits the run then
    /// handed back the previous pack, from which `main` computed an ACCEPT or a
    /// BLOCK. The pack itself is fine; claiming a verdict for a cancelled run
    /// from it is not.
    ///
    /// Driven at the seam with a fabricated previous run: `find_previous_run`
    /// resolves through `PRVIEW_HOME`, which a library test sharing a process
    /// with every other lib test must not reach for or mutate.
    #[tokio::test]
    async fn a_cancelled_update_run_does_not_reuse_a_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let config = reviewable_repo(tmp.path(), out.path());
        let head = rev_parse(tmp.path(), "HEAD");

        // A previous pack recorded at exactly this HEAD — the input that makes
        // `--update` short-circuit.
        let prev = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(prev.path().join("00_summary")).unwrap();
        std::fs::write(
            prev.path().join("00_summary/pr-metadata.txt"),
            format!("HEAD: feature ({head})\n"),
        )
        .unwrap();

        let app = crate::App::from_config(config).unwrap();
        let target = resolved(&head);

        // Uncancelled, the seam does what `--update` exists to do.
        let reused = app
            .reuse_unchanged_run(&target, &[], prev.path().to_path_buf(), false)
            .unwrap()
            .expect("HEAD has not moved, so the previous pack is reused");
        assert!(reused.unchanged, "the reused report is the unchanged one");
        assert_eq!(reused.artifacts_dir, prev.path());

        app.governor().cancel();
        let err = app
            .reuse_unchanged_run(&target, &[], prev.path().to_path_buf(), false)
            .expect_err("a cancelled run must not hand back a pack to take a verdict from");
        assert!(
            crate::governor::is_cancellation(&err),
            "the run must end as cancelled, not as some other failure: {err:?}",
        );
    }

    /// The snapshot-regression base must be exactly the base ref handed to
    /// `run_heuristics_with_snapshots`. `run()` now feeds it the merge-base
    /// (`diff_bases`), not the base tip, so that when the base branch advances
    /// with unrelated work the regression is computed over the same range as the
    /// artifact diff — not against base-only files the patch excludes.
    #[tokio::test]
    async fn snapshot_regression_is_anchored_to_the_base_ref_passed_in() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git_run(repo, &["init", "-q", "-b", "main"]);
        git_run(repo, &["config", "user.email", "t@t.t"]);
        git_run(repo, &["config", "user.name", "T"]);

        // Merge-base commit M: a valid, loctree-analysable crate.
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
        git_run(repo, &["add", "."]);
        git_run(repo, &["commit", "-q", "-m", "base"]);
        let merge_base = rev_parse(repo, "HEAD");

        // Target branch T forks from M with an unrelated source tweak.
        git_run(repo, &["checkout", "-q", "-b", "target"]);
        std::fs::write(repo.join("src/lib.rs"), "pub fn keep() {}\n// t\n").unwrap();
        git_run(repo, &["add", "."]);
        git_run(repo, &["commit", "-q", "-m", "target"]);
        let target_sha = rev_parse(repo, "HEAD");

        // Base branch B advances beyond the merge-base with its own unrelated file.
        git_run(repo, &["checkout", "-q", "main"]);
        git_run(repo, &["checkout", "-q", "-b", "advanced-base"]);
        std::fs::write(repo.join("src/extra.rs"), "pub fn other() {}\n").unwrap();
        git_run(repo, &["add", "."]);
        git_run(repo, &["commit", "-q", "-m", "advance base"]);
        let base_tip = rev_parse(repo, "HEAD");
        assert_ne!(merge_base, base_tip);

        let mut config = test_config();
        config.repo_root = repo.to_path_buf();
        config.run_heuristics = true;
        config.execution_mode = ExecutionMode::Deep;
        config.quiet = true;
        let app = crate::App::from_config(config).unwrap();

        let target_ref = resolved(&target_sha);

        // Handed the merge-base: regression anchors to it (what `run()` now does).
        let via_merge_base = app
            .run_heuristics_with_snapshots(&target_ref, &[resolved(&merge_base)])
            .await
            .unwrap();
        let reg_mb = via_merge_base
            .regression
            .expect("loctree signal available on both merge-base and target snapshots");
        assert_eq!(
            reg_mb.base_sha, merge_base,
            "regression base snapshot must be the merge-base commit"
        );

        // Handed the base tip: it would anchor there instead — the pre-fix bug.
        let via_tip = app
            .run_heuristics_with_snapshots(&target_ref, &[resolved(&base_tip)])
            .await
            .unwrap();
        let reg_tip = via_tip
            .regression
            .expect("loctree signal available on both base-tip and target snapshots");
        assert_eq!(reg_tip.base_sha, base_tip);
        assert_ne!(
            reg_mb.base_sha, reg_tip.base_sha,
            "the chosen base ref changes which tree the regression is computed against"
        );
    }
}
