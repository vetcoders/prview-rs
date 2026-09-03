//! Context generator planning and parallel execution (loctree/tsc-trace/tauri info).

use super::*;
use crate::governor::{GovernorPermit, ResourceGovernor, Weight};
use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};

/// The substrate a context command reading `scan_root` would report, resolved
/// exactly the way the matching check resolves its own provenance.
///
/// Same function, same consumable set: a `tsc` trace reads `node_modules` for
/// the same reason the TypeScript gate does, so both must land on one
/// `TreeState`. Resolving it any other way (say, with an empty consumable set)
/// would make the artifact claim `snapshot` where the gate reported
/// `snapshot-borrowed-deps` — two ledger tasks for one piece of work, and the
/// dedup would miss exactly the case it exists for.
fn context_substrate(check_name: &str, scan_root: &Path, repo_root: &Path) -> SubstrateKey {
    crate::checks::resolve_scan_substrate(
        scan_root,
        repo_root,
        crate::checks::consumable_scaffolding(check_name),
    )
    .into()
}

/// What the checks stage already resolved for a tool the context stage is about
/// to run itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateCoverage {
    /// A gate for this tool executed in this run on this substrate.
    CoveredLive { origin: SubstrateKey },
    /// A gate for this tool replayed a stored result. `origin` is the tree the
    /// ORIGINAL execution read; `cache_age_secs` is how old that entry was.
    CoveredCached {
        origin: SubstrateKey,
        cache_age_secs: Option<u64>,
    },
    /// A gate for this tool was configured and deliberately ruled out — a preset
    /// that excludes it, a disabled flag, a tool the environment lacks. The
    /// missing signal is a decision, not a gap.
    RuledOut { reason: String },
    /// This run holds no gate for the tool at all, so nothing was decided about
    /// it and the context artifact is the only place its signal can come from.
    Uncovered,
}

impl GateCoverage {
    fn is_covered(&self) -> bool {
        matches!(self, Self::CoveredLive { .. } | Self::CoveredCached { .. })
    }

    fn into_context_state(self) -> Option<TaskState> {
        match self {
            Self::CoveredLive { origin } => Some(TaskState::Reused { origin }),
            Self::CoveredCached {
                origin,
                cache_age_secs,
            } => Some(TaskState::Cached {
                cache_age_secs,
                origin,
            }),
            Self::RuledOut { .. } | Self::Uncovered => None,
        }
    }
}

fn gate_coverage(ledger: &TaskLedger, check_name: &str, substrate: &SubstrateKey) -> GateCoverage {
    let Some(entry) = ledger.lookup_tool(check_name, substrate) else {
        return GateCoverage::Uncovered;
    };
    match entry.state {
        // A gate that executed paid the cost already, whatever it concluded:
        // a failing or erroring run is still a run, and repeating it here would
        // buy the same answer twice.
        TaskState::Run { .. } => GateCoverage::CoveredLive {
            origin: entry.key.substrate,
        },
        TaskState::Reused { origin } => GateCoverage::CoveredLive { origin },
        TaskState::Cached {
            origin,
            cache_age_secs,
        } => GateCoverage::CoveredCached {
            origin,
            cache_age_secs,
        },
        TaskState::Skipped { reason } | TaskState::NotApplicable { reason } => {
            GateCoverage::RuledOut { reason }
        }
    }
}

/// Whether the context stage runs a tool itself, and — when it does not — the
/// reason, already recorded in the ledger.
enum ContextToolPlan {
    Run,
    Skip { reason: String },
}

/// Decide whether the context stage compensates for a missing gate result, and
/// record WHY when it does not.
///
/// The old rule was "the checks list holds no result for this tool, so run it",
/// which reads a deliberate exclusion as a gap: a fast remote-only preset rules
/// ESLint out precisely to avoid a full-tree lint, and the context stage then
/// spent 23 s doing exactly that (`PRV-CONTEXT-WORK-DEDUP`). Absence of a result
/// is not absence of a decision — the ledger holds the decision, so it decides.
///
/// `runnable` is the context stage's own answer, on the tree it would actually
/// read, to "could this tool run here at all". It picks the ledger state for a
/// tool that will not run: `Skipped` says this run chose not to, which another
/// preset would undo; `NotApplicable` says this environment could not, which no
/// switch would. The gate states its reason but not its class, and re-deriving
/// the class from the reason text would just couple two modules through a
/// string.
fn plan_context_tool(
    ledger: &TaskLedger,
    check_name: &str,
    substrate: &SubstrateKey,
    runnable: bool,
) -> ContextToolPlan {
    let coverage = gate_coverage(ledger, check_name, substrate);
    let state = if let Some(state) = coverage.clone().into_context_state() {
        state
    } else {
        match coverage {
            GateCoverage::RuledOut { reason } if runnable => TaskState::Skipped { reason },
            GateCoverage::RuledOut { reason } => TaskState::NotApplicable { reason },
            GateCoverage::Uncovered if runnable => return ContextToolPlan::Run,
            GateCoverage::Uncovered => TaskState::NotApplicable {
                reason: format!(
                    "no {check_name} gate in this run and no runnable tool in the reviewed tree"
                ),
            },
            GateCoverage::CoveredLive { .. } | GateCoverage::CoveredCached { .. } => {
                unreachable!("covered coverage always has a context state")
            }
        }
    };

    let reason = match &state {
        TaskState::Cached { .. } | TaskState::Reused { .. } => {
            format!("the {check_name} gate already produced this signal for the reviewed tree")
        }
        TaskState::Skipped { reason } | TaskState::NotApplicable { reason } => reason.clone(),
        TaskState::Run { .. } => unreachable!("a plan that runs returns before recording"),
    };

    record_context_decision(ledger, check_name, substrate, state);

    ContextToolPlan::Skip { reason }
}

/// Record why a context artifact was NOT produced.
///
/// Only the non-run decisions land here. A context command that DOES run is
/// recorded by [`record_context_runs`] once the runtime knows how long it took;
/// the planner does not, and a `Run` entry carrying a duration it invented would
/// be worse than no entry at all.
fn record_context_decision(
    ledger: &TaskLedger,
    check_name: &str,
    substrate: &SubstrateKey,
    state: TaskState,
) {
    debug_assert!(
        !matches!(state, TaskState::Run { .. }),
        "the planner records decisions not to run, never runs",
    );
    ledger.record(TaskEntry {
        key: TaskKey::new(check_name, substrate.clone()),
        kind: TaskKind::ContextArtifact,
        state,
        queued_at: None,
        started_at: None,
    });
}

/// One-line note that a context artifact was not produced, and why.
fn announce_skip(emit: bool, artifact: &str, reason: &str) {
    if emit {
        use colored::Colorize;
        println!("  {} {artifact}: skipped ({reason})", "ℹ".blue());
    }
}

/// `scan_root` is the reviewed tree — see [`plan_context_cmds`]. A decision
/// recorded here says whether an artifact WILL be produced, so it must be taken
/// against the same tree the generator will read; deciding from the local
/// checkout would let `RUN.json` promise (or excuse) an artifact the reviewed
/// snapshot never had the shape for.
pub(super) fn plan_context_artifacts(
    config: &Config,
    scan_root: &Path,
    diffs: &[Diff],
    checks: &[CheckResult],
    ledger: &TaskLedger,
) -> Vec<ContextArtifactDecision> {
    let mut decisions = Vec::new();

    if config.profile.has_tsconfig {
        decisions.push(plan_tsc_trace_artifact(
            config, scan_root, diffs, checks, ledger,
        ));
    }
    if has_tauri_context(config, scan_root) {
        decisions.push(plan_tauri_info_artifact(config, scan_root, diffs, ledger));
    }

    decisions
}

pub(super) fn plan_tsc_trace_artifact(
    config: &Config,
    scan_root: &Path,
    diffs: &[Diff],
    checks: &[CheckResult],
    ledger: &TaskLedger,
) -> ContextArtifactDecision {
    let resolution_failure = detect_typescript_resolution_signal(checks);

    if !config.is_fast_remote_only_standard() {
        // "Generated by default for this run mode" used to be unconditional, so
        // a deep run compiled the reviewed tree twice: once as the TypeScript
        // gate (8 s) and again as `tsc --noEmit --traceResolution` (8 s), with
        // the second compile producing the same diagnostics the first already
        // had (PRV-CONTEXT-WORK-DEDUP).
        //
        // A resolution FAILURE is the exception, and it stays: there the trace
        // answers a question the gate's own output cannot — which candidate
        // paths the compiler tried before giving up — so the second compile
        // buys something the first did not.
        let substrate = context_substrate("TypeScript", scan_root, &config.repo_root);
        if resolution_failure.is_none()
            && let Some(state) =
                gate_coverage(ledger, "TypeScript", &substrate).into_context_state()
        {
            record_context_decision(ledger, "TypeScript", &substrate, state);
            return ContextArtifactDecision {
                key: "tsc_trace",
                path: "30_context/tsc-trace.log",
                generated: false,
                recommended: false,
                reason: "skipped: the TypeScript gate already compiled this tree and reported no \
                     module-resolution failure"
                    .to_string(),
            };
        }

        return ContextArtifactDecision {
            key: "tsc_trace",
            path: "30_context/tsc-trace.log",
            generated: true,
            recommended: false,
            reason: match &resolution_failure {
                Some(signal) => format!("generated for this run mode; {signal}"),
                None => "generated by default for this run mode".to_string(),
            },
        };
    }

    let mut reasons = Vec::new();
    if let Some(reason) = resolution_failure {
        reasons.push(reason);
    }

    let changed_resolution_files = find_ts_resolution_related_changes(diffs);
    if !changed_resolution_files.is_empty() {
        reasons.push(format!(
            "resolution-related files changed ({})",
            changed_resolution_files.join(", ")
        ));
    }

    if reasons.is_empty() {
        ContextArtifactDecision {
            key: "tsc_trace",
            path: "30_context/tsc-trace.log",
            generated: false,
            recommended: false,
            reason: "skipped by default in fast remote-only runs; no TypeScript resolution signals detected"
                .to_string(),
        }
    } else {
        ContextArtifactDecision {
            key: "tsc_trace",
            path: "30_context/tsc-trace.log",
            generated: false,
            recommended: true,
            reason: format!(
                "skipped by default in fast remote-only runs; generate when investigating because {}",
                reasons.join("; ")
            ),
        }
    }
}

pub(super) fn detect_typescript_resolution_signal(checks: &[CheckResult]) -> Option<String> {
    let typescript = checks
        .iter()
        .find(|check| check.name.eq_ignore_ascii_case("TypeScript"))?;
    if !typescript.is_failure() {
        return None;
    }

    let output = typescript.output.to_lowercase();
    const RESOLUTION_NEEDLES: &[&str] = &[
        "cannot find module",
        "cannot resolve module",
        "module resolution",
        "did you mean to set the module resolution option",
        "paths option",
        "baseurl",
    ];
    if RESOLUTION_NEEDLES
        .iter()
        .any(|needle| output.contains(needle))
    {
        Some("TypeScript check failed with module-resolution-style errors".to_string())
    } else {
        None
    }
}

pub(super) fn find_ts_resolution_related_changes(diffs: &[Diff]) -> Vec<String> {
    let mut matches = Vec::new();

    for file in diffs.iter().flat_map(|diff| &diff.files) {
        let path = file.path.as_str();
        let file_name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        let is_tsconfig = file_name.starts_with("tsconfig") && file_name.ends_with(".json");
        let is_resolution_config = matches!(
            file_name,
            "package.json"
                | "pnpm-lock.yaml"
                | "package-lock.json"
                | "yarn.lock"
                | "vite.config.ts"
                | "vite.config.js"
                | "vite.config.mjs"
                | "vite.config.cjs"
                | "webpack.config.js"
                | "webpack.config.ts"
                | "tsup.config.ts"
                | "tsup.config.js"
        );

        if is_tsconfig || is_resolution_config {
            matches.push(path.to_string());
        }
    }

    matches.sort();
    matches.dedup();
    matches.truncate(3);
    matches
}

pub(super) fn has_tauri_context(config: &Config, scan_root: &Path) -> bool {
    if !config.profile.has_cargo {
        return false;
    }
    is_tauri_project(scan_root)
}

/// Detect whether the repository is actually a Tauri project.
///
/// Checks (in order):
/// 1. `tauri.conf.json` or `tauri.conf.toml` in the repo root or `src-tauri/`
/// 2. `src-tauri/Cargo.toml` exists (the canonical Tauri crate location)
/// 3. Root or workspace `Cargo.toml` lists `tauri` as a dependency
pub(super) fn is_tauri_project(repo_root: &Path) -> bool {
    // tauri.conf.json / tauri.conf.toml in standard locations
    let conf_files = [
        repo_root.join("tauri.conf.json"),
        repo_root.join("tauri.conf.toml"),
        repo_root.join("src-tauri").join("tauri.conf.json"),
        repo_root.join("src-tauri").join("tauri.conf.toml"),
    ];
    if conf_files.iter().any(|p| p.exists()) {
        return true;
    }

    // src-tauri/Cargo.toml — canonical Tauri structure
    if repo_root.join("src-tauri").join("Cargo.toml").exists() {
        return true;
    }

    // Cargo.toml contains tauri as a dependency
    let cargo_toml_path = repo_root.join("Cargo.toml");
    if let Ok(content) = fs::read_to_string(&cargo_toml_path)
        && content.contains("tauri")
    {
        return true;
    }

    false
}

/// `tauri info` has no gate to dedup against — no check runs it, so nothing in
/// this run could already have paid its 17 s. Its behaviour is therefore
/// unchanged; what changes is that its decision stops being invisible. When the
/// artifact is not produced, the reason is written to the ledger the same way a
/// ruled-out tool's is, so a reader of the run sees one accounting of what was
/// and was not done rather than two.
pub(super) fn plan_tauri_info_artifact(
    config: &Config,
    scan_root: &Path,
    diffs: &[Diff],
    ledger: &TaskLedger,
) -> ContextArtifactDecision {
    if !config.is_fast_remote_only_standard() {
        return ContextArtifactDecision {
            key: "tauri_info",
            path: "30_context/tauri-info.log",
            generated: true,
            recommended: false,
            reason: "generated by default for this run mode".to_string(),
        };
    }

    let changed_tauri_files = find_tauri_diagnostic_changes(diffs);
    let decision = if changed_tauri_files.is_empty() {
        ContextArtifactDecision {
            key: "tauri_info",
            path: "30_context/tauri-info.log",
            generated: false,
            recommended: false,
            reason:
                "skipped by default in fast remote-only runs; no Tauri config/build signals detected"
                    .to_string(),
        }
    } else {
        ContextArtifactDecision {
            key: "tauri_info",
            path: "30_context/tauri-info.log",
            generated: false,
            recommended: true,
            reason: format!(
                "skipped by default in fast remote-only runs; generate when investigating because Tauri config/build files changed ({})",
                changed_tauri_files.join(", ")
            ),
        }
    };

    // The preset deferred it: this run chose not to, and another preset would
    // undo that — a `Skipped`, not a `NotApplicable`.
    record_context_decision(
        ledger,
        "tauri info",
        &context_substrate("tauri info", scan_root, &config.repo_root),
        TaskState::Skipped {
            reason: decision.reason.clone(),
        },
    );

    decision
}

pub(super) fn find_tauri_diagnostic_changes(diffs: &[Diff]) -> Vec<String> {
    let mut matches = Vec::new();

    for file in diffs.iter().flat_map(|diff| &diff.files) {
        let path = file.path.as_str();
        let file_name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        let is_tauri_config = matches!(
            file_name,
            "tauri.conf.json"
                | "tauri.linux.conf.json"
                | "tauri.macos.conf.json"
                | "tauri.windows.conf.json"
                | "build.rs"
                | "Cargo.toml"
        );
        let is_tauri_capability = path.starts_with("src-tauri/capabilities/");

        if is_tauri_config || is_tauri_capability {
            matches.push(path.to_string());
        }
    }

    matches.sort();
    matches.dedup();
    matches.truncate(3);
    matches
}

/// Descriptor for an external context command to run in parallel.
pub(super) struct ContextCmd {
    pub(super) label: String,
    /// The GATE this command stands in for, when one exists — the same check
    /// name the planner asked the ledger about before deciding to run it.
    ///
    /// It is the identity, not the label, that the ledger is keyed on: `eslint
    /// json` executed here and the `ESLint` gate are one tool on one tree, and
    /// recording the executed one under `eslint_json` would file the two halves
    /// of the same task under two ids — the drift class `check_id` exists to
    /// close. `None` is the honest answer for a command no gate covers
    /// (`cargo tree`, `tauri info`, `npm sbom`), which is then recorded under its
    /// own label. No per-command alias table: the plan site already knows.
    pub(super) gate: Option<&'static str>,
    pub(super) cmd: String,
    pub(super) args: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) out_dir: PathBuf,
    pub(super) out_file: String,
}

impl ContextCmd {
    /// The name this command is recorded under in the task ledger.
    pub(super) fn tool(&self) -> &str {
        self.gate.unwrap_or(&self.label)
    }
}

fn command_identity(cmd: &ContextCmd) -> String {
    std::iter::once(cmd.cmd.as_str())
        .chain(cmd.args.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve the command for the optional `tauri info` context artifact.
///
/// Prefers a locally-installed tauri binary (a direct exec with no npm consult
/// and no interactive prompt), then `pnpm`. Returns `None` when neither is
/// available so the caller never falls through to `npx --no-install`, which
/// still consults npm and, for a missing CLI, can hit the network and hang
/// until the context timeout instead of recording the artifact as unavailable
/// (PR #12 review). Mirrors the local-bin-first pattern in `checks` (38c4b01).
fn tauri_info_cmd(repo_root: &Path, has_pnpm: bool) -> Option<(String, Vec<String>)> {
    if let Some(bin) = crate::checks::local_js_bin("tauri", repo_root) {
        Some((bin.to_string_lossy().into_owned(), vec!["info".into()]))
    } else if has_pnpm {
        Some(("pnpm".to_string(), vec!["tauri".into(), "info".into()]))
    } else {
        None
    }
}

/// Resolve the command for an optional `pnpm exec`-class JS-tool context
/// artifact (tsc, eslint, esbuild, ...).
///
/// Prefers a locally-installed `node_modules/.bin/<tool>` binary (a direct exec
/// with no npm consult and no interactive prompt), then `pnpm exec <tool>`.
/// Returns `None` when neither is available so the caller skips the optional
/// artifact rather than falling through to `npx --no-install`, which still
/// consults npm and, for a missing CLI, can hit the network and hang until the
/// context timeout instead of recording the artifact as unavailable
/// (PR #12 review). Mirrors `tauri_info_cmd` and the local-bin-first pattern in
/// `checks` (38c4b01).
fn js_exec_cmd(
    tool: &str,
    tool_args: Vec<String>,
    repo_root: &Path,
    has_pnpm: bool,
) -> Option<(String, Vec<String>)> {
    if let Some(bin) = crate::checks::local_js_bin(tool, repo_root) {
        Some((bin.to_string_lossy().into_owned(), tool_args))
    } else if has_pnpm {
        let mut args = vec!["exec".to_string(), tool.to_string()];
        args.extend(tool_args);
        Some(("pnpm".to_string(), args))
    } else {
        None
    }
}

/// Run profile-specific context generators with timeouts.
/// All commands run in parallel with a shared deadline.
/// Skips tools already executed by the checks system.
pub(super) fn generate_context_artifacts(
    config: &Config,
    scan_root: &Path,
    ledger: &TaskLedger,
    context_dir: &Path,
    emit_human_stdout: bool,
    decisions: &[ContextArtifactDecision],
    governor: &ResourceGovernor,
) -> Result<Vec<ContextCommandTiming>> {
    let cmds = plan_context_cmds(
        config,
        scan_root,
        ledger,
        context_dir,
        emit_human_stdout,
        decisions,
    );

    if cmds.is_empty() {
        return Ok(Vec::new());
    }

    // Run all commands in parallel with a shared timeout
    let timings =
        run_context_cmds_parallel(&cmds, CONTEXT_GEN_TIMEOUT_SECS, emit_human_stdout, governor)?;
    record_context_runs(ledger, &config.repo_root, &cmds, &timings);
    Ok(timings)
}

/// Replace pre-run intent with the context stage's observable result. A
/// decision may say an artifact was planned, but `generated` is true only when
/// its command ran and the expected file is present in this pack.
pub(super) fn reconcile_context_artifacts(
    decisions: &mut [ContextArtifactDecision],
    timings: &[ContextCommandTiming],
    artifacts_root: &Path,
) {
    for decision in decisions.iter_mut().filter(|decision| decision.generated) {
        let label = match decision.key {
            "tsc_trace" => "tsc trace",
            "tauri_info" => "tauri info",
            _ => continue,
        };
        let timing = timings.iter().find(|timing| timing.label == label);
        let output_exists = artifacts_root.join(decision.path).is_file();
        let output_reported = timing
            .and_then(|timing| timing.artifact.as_deref())
            .is_some_and(|artifact| artifact == decision.path);

        match timing {
            Some(timing) if output_reported && output_exists => {
                decision.reason = format!(
                    "generated: context command outcome was {} and output exists",
                    timing.status
                );
            }
            Some(timing) => {
                decision.generated = false;
                decision.reason = format!(
                    "not generated: context command outcome was {}; expected output was not produced{}",
                    timing.status,
                    timing
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                );
            }
            None => {
                decision.generated = false;
                decision.reason =
                    "not generated: no runnable context command produced the expected output"
                        .to_owned();
            }
        }
    }
}

/// Record the context commands that actually executed.
///
/// The planner records only what it decided NOT to run, so a run's account of
/// itself was half a ledger: the work the context stage skipped was auditable and
/// the work it performed was not — and "did this tool already read this tree?"
/// is a question the executed half answers best. The runtime is the first place
/// that holds both the identity of the command and its duration, so it is where
/// the entry is written.
///
/// Timings join back to their command by label, which is unique within a plan.
/// A command that never started (spawn failure, cancellation, or shared deadline
/// while still queued) is a `Skipped`, not a `Run` of zero seconds: nothing read
/// the tree, and a zero-duration run would claim otherwise. A command that
/// started and then failed, timed out or errored IS a run — the tool read the
/// tree and the run paid for it.
fn record_context_runs(
    ledger: &TaskLedger,
    repo_root: &Path,
    cmds: &[ContextCmd],
    timings: &[ContextCommandTiming],
) {
    for timing in timings {
        let Some(cmd) = cmds.iter().find(|cmd| cmd.label == timing.label) else {
            continue;
        };
        let state = if !timing.started {
            TaskState::Skipped {
                reason: timing
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("`{}` did not start in the reviewed tree", cmd.cmd)),
            }
        } else {
            match timing.status {
                // Neither of these delivered an answer about the reviewed tree, so
                // neither is a Run: one never started, the other was stopped.
                "spawn_failed" => TaskState::Skipped {
                    reason: timing.reason.clone().unwrap_or_else(|| {
                        format!("`{}` could not be spawned in the reviewed tree", cmd.cmd)
                    }),
                },
                "cancelled" => TaskState::Skipped {
                    reason: format!("`{}` did not run: the review was cancelled", cmd.cmd),
                },
                _ => TaskState::Run {
                    duration: std::time::Duration::from_secs_f32(timing.duration_secs),
                },
            }
        };
        ledger.record(TaskEntry {
            // The command's own cwd, not the scan root: a cargo context command
            // runs in a workspace member below it, and the substrate must name
            // the tree the command actually read.
            key: TaskKey::new(
                cmd.tool(),
                context_substrate(cmd.tool(), &cmd.cwd, repo_root),
            ),
            kind: TaskKind::ContextArtifact,
            state,
            queued_at: None,
            started_at: None,
        });
    }
}

/// Decide WHICH context commands this run needs and WHERE each one runs.
///
/// Split out of [`generate_context_artifacts`] so the decision can be asserted
/// on without spawning a single process — the execution half is a separate,
/// already-tested concern.
///
/// `scan_root` is the reviewed tree: the run-wide target snapshot when there is
/// one, the repo root otherwise. It is NOT `config.repo_root`, and the
/// difference is the whole point. Every command's cwd and every filesystem
/// probe below reads it, because a `--pr` run's gates judge the PR's snapshot
/// while these artifacts used to be produced from whatever the operator had
/// checked out locally — the same pack then carried two different revisions
/// under one provenance (`PRV-CONTEXT-SNAPSHOT-PROVENANCE`). Deciding tool
/// availability from the local tree while running the tool against the snapshot
/// would reintroduce the same mixing in a smaller form, so the probes move with
/// the cwd.
///
/// `ledger` is the single source of truth for "did a gate already do this work",
/// replacing the `checks_ran_*` booleans this function used to derive from the
/// results list. The results list can only report what SUCCEEDED in reaching a
/// result; the ledger also reports what was deliberately ruled out, which is the
/// half that decides whether compensating here is help or duplicated cost
/// (`PRV-CONTEXT-WORK-DEDUP`).
fn plan_context_cmds(
    config: &Config,
    scan_root: &Path,
    ledger: &TaskLedger,
    context_dir: &Path,
    emit_human_stdout: bool,
    decisions: &[ContextArtifactDecision],
) -> Vec<ContextCmd> {
    let ctx = context_dir.to_path_buf();
    let scan_root = scan_root.to_path_buf();
    let has_pnpm = which::which("pnpm").is_ok();

    let mut cmds: Vec<ContextCmd> = Vec::new();

    let tauri_info = decisions
        .iter()
        .find(|decision| decision.key == "tauri_info");

    // Cargo profile
    if config.profile.has_cargo {
        // `config.profile.cargo_root` names the LOCAL checkout's cargo root; a
        // workspace member sits below the scan root, and the reviewed commit may
        // have moved it. Resolve it the way the cargo gates do so `cargo tree`
        // reports the crate they judged, not a sibling.
        let cwd = crate::checks::planned_cargo_cwd(config, &scan_root);
        cmds.push(ContextCmd {
            label: "cargo tree".into(),
            gate: None,
            cmd: "cargo".into(),
            args: vec!["tree".into(), "--depth".into(), "2".into()],
            cwd: cwd.clone(),
            out_dir: ctx.clone(),
            out_file: "cargo-tree.txt".into(),
        });

        cmds.push(ContextCmd {
            label: "cargo sbom".into(),
            gate: None,
            cmd: "cargo".into(),
            args: vec!["tree".into(), "--format".into(), "{p} {l}".into()],
            cwd: cwd.clone(),
            out_dir: ctx.clone(),
            out_file: "cargo-sbom.txt".into(),
        });

        if config.profile.cargo_root.is_some() {
            let tauri_dir = if cwd.ends_with("src-tauri") {
                cwd.clone()
            } else {
                scan_root.join("src-tauri")
            };
            // Only generate tauri artifacts for actual Tauri projects.
            // Checking the directory alone is insufficient (leftover fixtures,
            // partial scaffolds). Require tauri.conf.json/toml, src-tauri/Cargo.toml,
            // or a "tauri" entry in the root Cargo.toml.
            if is_tauri_project(&scan_root) && tauri_dir.exists() {
                if tauri_info.is_none_or(|decision| decision.generated) {
                    // Resolve a directly-runnable tauri binary; skip the artifact
                    // when none is available rather than reaching npx --no-install,
                    // which still consults npm and can hang until timeout on a
                    // missing CLI (PR #12 review).
                    if let Some((cmd, args)) = tauri_info_cmd(&scan_root, has_pnpm) {
                        cmds.push(ContextCmd {
                            label: "tauri info".into(),
                            gate: None,
                            cmd,
                            args,
                            cwd: scan_root.clone(),
                            out_dir: ctx.clone(),
                            out_file: "tauri-info.log".into(),
                        });
                    }
                } else if let Some(decision) = tauri_info
                    && emit_human_stdout
                {
                    use colored::Colorize;
                    let marker = if decision.recommended {
                        "ℹ".yellow()
                    } else {
                        "ℹ".blue()
                    };
                    println!("  {} tauri-info.log: skipped ({})", marker, decision.reason);
                }
            }
        }
    }

    // TypeScript trace
    let tsc_trace = decisions
        .iter()
        .find(|decision| decision.key == "tsc_trace");

    if config.profile.has_tsconfig && tsc_trace.is_none_or(|decision| decision.generated) {
        // Resolve a directly-runnable tsc binary; skip the artifact when none is
        // available rather than reaching npx --no-install, which can hang on a
        // missing CLI (PR #12 review).
        if let Some((cmd, args)) = js_exec_cmd(
            "tsc",
            vec!["--noEmit".into(), "--traceResolution".into()],
            &scan_root,
            has_pnpm,
        ) {
            cmds.push(ContextCmd {
                label: "tsc trace".into(),
                gate: Some("TypeScript"),
                cmd,
                args,
                cwd: scan_root.clone(),
                out_dir: ctx.clone(),
                out_file: "tsc-trace.log".into(),
            });
        }
    } else if let Some(decision) = tsc_trace
        && emit_human_stdout
    {
        use colored::Colorize;
        let marker = if decision.recommended {
            "ℹ".yellow()
        } else {
            "ℹ".blue()
        };
        println!("  {} tsc-trace.log: skipped ({})", marker, decision.reason);
    }

    // JS/TS package.json tools
    if config.profile.has_package_json {
        // `npx list` would run the npm package "list" from the registry —
        // the dependency listing lives in npm/pnpm themselves.
        let (sbom_cmd, sbom_args) = if has_pnpm {
            ("pnpm", vec!["list".into(), "--all".into()])
        } else {
            ("npm", vec!["ls".into(), "--all".into()])
        };
        cmds.push(ContextCmd {
            label: "npm sbom".into(),
            gate: None,
            cmd: sbom_cmd.into(),
            args: sbom_args,
            cwd: scan_root.clone(),
            out_dir: ctx.clone(),
            out_file: "npm-sbom.txt".into(),
        });

        // Resolve a directly-runnable eslint binary; skip the artifact when
        // none is available rather than reaching npx --no-install, which can
        // hang on a missing CLI (PR #12 review).
        let eslint = js_exec_cmd(
            "eslint",
            vec![
                ".".into(),
                "--ext".into(),
                ".ts,.tsx,.js,.jsx".into(),
                "-f".into(),
                "json".into(),
            ],
            &scan_root,
            has_pnpm,
        );
        match plan_context_tool(
            ledger,
            "ESLint",
            &context_substrate("ESLint", &scan_root, &config.repo_root),
            eslint.is_some(),
        ) {
            ContextToolPlan::Run => {
                let (cmd, args) = eslint.expect("a runnable plan resolved a command");
                cmds.push(ContextCmd {
                    label: "eslint json".into(),
                    gate: Some("ESLint"),
                    cmd,
                    args,
                    cwd: scan_root.clone(),
                    out_dir: ctx.clone(),
                    out_file: "eslint.json".into(),
                });
            }
            ContextToolPlan::Skip { reason } => {
                announce_skip(emit_human_stdout, "eslint.json", &reason);
            }
        }

        // Stylelint resolves through a shell so its glob is expanded by the
        // tool, so its availability probe is the local binary itself rather
        // than js_exec_cmd's.
        let stylelint_available = scan_root.join("node_modules/.bin/stylelint").exists();
        match plan_context_tool(
            ledger,
            "Stylelint",
            &context_substrate("Stylelint", &scan_root, &config.repo_root),
            stylelint_available,
        ) {
            ContextToolPlan::Run => {
                cmds.push(ContextCmd {
                    label: "stylelint json".into(),
                    gate: Some("Stylelint"),
                    cmd: "sh".into(),
                    args: vec![
                        "-c".into(),
                        "pnpm exec stylelint 'src/**/*.css' -f json --allow-empty-input".into(),
                    ],
                    cwd: scan_root.clone(),
                    out_dir: ctx.clone(),
                    out_file: "stylelint.json".into(),
                });
            }
            ContextToolPlan::Skip { reason } => {
                announce_skip(emit_human_stdout, "stylelint.json", &reason);
            }
        }

        // Vitest is the one tool the context stage never compensates for: test
        // results come from the gate or not at all. There is no command to plan
        // and so no decision to record — only a note when the gate did not
        // deliver them.
        if !gate_coverage(
            ledger,
            "Vitest",
            &context_substrate("Vitest", &scan_root, &config.repo_root),
        )
        .is_covered()
        {
            announce_skip(
                emit_human_stdout,
                "vitest-report.json",
                "use checks for test results",
            );
        }

        // esbuild meta
        if scan_root.join("node_modules/.bin/esbuild").exists() {
            let entry = if scan_root.join("src/main.tsx").exists() {
                Some("src/main.tsx")
            } else if scan_root.join("src/main.ts").exists() {
                Some("src/main.ts")
            } else {
                None
            };
            if let Some(entry) = entry {
                let meta_path = ctx.join("esbuild-meta.json");
                let meta_arg = format!("--metafile={}", meta_path.display());
                // This branch is gated on a local esbuild binary existing, so
                // js_exec_cmd resolves it to a direct exec (never npx --no-install)
                // (PR #12 review).
                if let Some((cmd, args)) = js_exec_cmd(
                    "esbuild",
                    vec![
                        entry.into(),
                        "--bundle".into(),
                        meta_arg,
                        "--log-level=error".into(),
                    ],
                    &scan_root,
                    has_pnpm,
                ) {
                    cmds.push(ContextCmd {
                        label: "esbuild meta".into(),
                        gate: None,
                        cmd,
                        args,
                        cwd: scan_root.clone(),
                        out_dir: ctx.clone(),
                        out_file: String::new(),
                    });
                }
            }
        }
    }

    cmds
}

/// What a context command costs the machine, for the run's resource governor.
///
/// Commands without a supported descendant cap are `Exclusive`. Metadata-only
/// commands remain light; Cargo still receives `CARGO_BUILD_JOBS` defensively.
///
/// The rest read metadata and are `Light`: `cargo tree` (and the sbom variant it
/// shares a binary with) resolve the dependency graph from the lockfile without
/// compiling anything, `npm`/`pnpm list` walks `node_modules`, and `tauri info`
/// probes the environment. Getting one of these wrong only wastes budget, which
/// is why the default falls this way.
fn context_cmd_weight(cmd: &ContextCmd) -> Weight {
    match cmd.label.as_str() {
        "tsc trace" | "eslint json" | "stylelint json" | "esbuild meta" => Weight::Exclusive,
        // "cargo tree", "cargo sbom", "npm sbom", "tauri info"
        _ => Weight::Light,
    }
}

/// Spawn context commands under the run's budget and poll them with a shared
/// timeout.
///
/// `timeout_secs` is one deadline for the whole stage, computed before admission.
/// Commands still queued when it expires are timed out without a fresh per-command
/// clock. Results are written to the specified output files.
///
/// "In parallel" now means "as parallel as the machine allows": a command waits
/// for its share of the governor's budget before it is spawned, so the context
/// stage can no longer put a bundler and a whole-project type check on a box
/// that the checks stage has already filled. The stages do not overlap in time
/// today — step 5 is fully awaited before step 7 — so this is one budget being
/// honoured rather than a measured collision being fixed.
pub(super) fn run_context_cmds_parallel(
    cmds: &[ContextCmd],
    timeout_secs: u64,
    emit: bool,
    governor: &ResourceGovernor,
) -> Result<Vec<ContextCommandTiming>> {
    run_context_cmds_parallel_after_spawn(cmds, timeout_secs, emit, governor, |_| {})
}

fn run_context_cmds_parallel_after_spawn(
    cmds: &[ContextCmd],
    timeout_secs: u64,
    emit: bool,
    governor: &ResourceGovernor,
    mut after_spawn: impl FnMut(u32),
) -> Result<Vec<ContextCommandTiming>> {
    use std::collections::VecDeque;
    use std::time::Duration;

    struct RunningCmd {
        label: String,
        child: Box<dyn process_wrap::std::ChildWrapper>,
        /// The key this child is registered under, so cancellation can reach its
        /// process group. Dropped from the registry the moment it exits: a pid
        /// the governor still believes in is a pid it may signal, and pids are
        /// reused.
        registry_key: String,
        /// This command's slice of the machine, returned when it finishes rather
        /// than when the whole stage does.
        budget: Option<GovernorPermit>,
        started_at: Instant,
        deadline: Instant,
        out_dir: PathBuf,
        out_file: String,
        stdout_path: PathBuf,
        stderr_path: PathBuf,
        done: bool,
    }

    let mut running: Vec<RunningCmd> = Vec::new();
    let mut timings = Vec::new();
    let mut scheduled: Vec<(usize, &ContextCmd)> = cmds.iter().enumerate().collect();
    scheduled.sort_by_key(|(_, cmd)| match context_cmd_weight(cmd) {
        Weight::Light => 0,
        Weight::Heavy => 1,
        Weight::Exclusive => 2,
    });
    let mut pending: VecDeque<(usize, &ContextCmd)> = scheduled.into();
    let poll_interval = Duration::from_millis(200);
    let stage_deadline = Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        // Admit as many queued commands as the budget currently allows, in plan
        // order. `try_acquire` rather than a blocking wait because this loop is
        // also the one that reaps finished commands — blocking here would stop
        // the budget from ever coming back.
        while let Some(&(idx, cmd)) = pending.front() {
            // A cancelled run must not start new work. `try_acquire` refuses
            // anyway once the budget is closed, but silently and identically to
            // "the machine is busy" — this is the branch that tells them apart,
            // and it is the one that stops the loop instead of waiting for a
            // budget that is never coming back.
            if governor.is_cancelled() {
                break;
            }
            if Instant::now() >= stage_deadline {
                break;
            }
            let Some(budget) = governor.try_acquire(context_cmd_weight(cmd)) else {
                break;
            };
            pending.pop_front();

            let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
            let stdout_path = cmd.out_dir.join(format!(".context-cmd-{idx}.stdout.tmp"));
            let stderr_path = cmd.out_dir.join(format!(".context-cmd-{idx}.stderr.tmp"));
            let stdout_file = match File::create(&stdout_path) {
                Ok(file) => file,
                Err(error) => {
                    timings.push(ContextCommandTiming {
                        label: cmd.label.clone(),
                        artifact: None,
                        status: "spawn_failed",
                        started: false,
                        duration_secs: 0.0,
                        reason: Some(format!(
                            "{}: could not create stdout capture {}: {error}",
                            command_identity(cmd),
                            stdout_path.display()
                        )),
                    });
                    continue;
                }
            };
            let stderr_file = match File::create(&stderr_path) {
                Ok(file) => file,
                Err(error) => {
                    let _ = fs::remove_file(&stdout_path);
                    timings.push(ContextCommandTiming {
                        label: cmd.label.clone(),
                        artifact: None,
                        status: "spawn_failed",
                        started: false,
                        duration_secs: 0.0,
                        reason: Some(format!(
                            "{}: could not create stderr capture {}: {error}",
                            command_identity(cmd),
                            stderr_path.display()
                        )),
                    });
                    continue;
                }
            };

            let mut command = Command::new(&cmd.cmd);
            command.args(&args).current_dir(&cmd.cwd);
            if Path::new(&cmd.cmd)
                .file_name()
                .is_some_and(|name| name == "cargo" || name == "cargo.exe")
            {
                command.env("CARGO_BUILD_JOBS", governor.worker_limit().to_string());
            }
            // Shared rails: stdin detached, so a context tool can never read the
            // operator's terminal (an interactive prompt with stdout redirected
            // to a file is invisible and steals keystrokes), and its own process
            // group, so one signal reaches the tree under an `sh -c` wrapper.
            // The checks stage has always spawned this way; the context stage
            // not doing so was an omission, not a decision.
            crate::proc::harden_std(&mut command);
            command
                .stdout(std::process::Stdio::from(stdout_file))
                .stderr(std::process::Stdio::from(stderr_file));
            match crate::proc::spawn_owned_std_child(command) {
                Ok(mut child) => {
                    let started_at = Instant::now();
                    let registry_key = format!("context:{idx}:{}", cmd.label);
                    after_spawn(child.id());
                    if !governor.register_child(registry_key.clone(), child.id()) {
                        let tree_reaped =
                            crate::proc::terminate_and_reap_owned_std_child(child.as_mut());
                        let _ = fs::remove_file(&stdout_path);
                        let _ = fs::remove_file(&stderr_path);
                        timings.push(ContextCommandTiming {
                            label: cmd.label.clone(),
                            artifact: None,
                            status: "cancelled",
                            started: true,
                            duration_secs: started_at.elapsed().as_secs_f32(),
                            reason: Some(if tree_reaped {
                                format!(
                                    "{} was refused during late registration and terminated because the review was cancelled",
                                    command_identity(cmd)
                                )
                            } else {
                                format!(
                                    "{} was refused during late registration, but process-tree termination could not be confirmed",
                                    command_identity(cmd)
                                )
                            }),
                        });
                        continue;
                    }
                    running.push(RunningCmd {
                        label: cmd.label.clone(),
                        child,
                        registry_key,
                        budget: Some(budget),
                        started_at,
                        deadline: stage_deadline,
                        out_dir: cmd.out_dir.clone(),
                        out_file: cmd.out_file.clone(),
                        stdout_path,
                        stderr_path,
                        done: false,
                    });
                }
                Err(error) => {
                    let _ = fs::remove_file(&stdout_path);
                    let _ = fs::remove_file(&stderr_path);
                    // Command not available: record it instead of skipping
                    // silently, so the timings tell the truth about the pack.
                    timings.push(ContextCommandTiming {
                        label: cmd.label.clone(),
                        artifact: None,
                        status: "spawn_failed",
                        started: false,
                        duration_secs: 0.0,
                        reason: Some(format!("{}: spawn failed: {error}", command_identity(cmd))),
                    });
                }
            }
        }

        if governor.is_cancelled() {
            // Say so for every command that will now never run, rather than
            // leaving it out of the account entirely.
            for (_, cmd) in pending.drain(..) {
                timings.push(ContextCommandTiming {
                    label: cmd.label.clone(),
                    artifact: None,
                    status: "cancelled",
                    started: false,
                    duration_secs: 0.0,
                    reason: Some(format!(
                        "{} did not start because the review was cancelled",
                        command_identity(cmd)
                    )),
                });
            }
            // PID-based cancellation is only the first line of defence on
            // Windows: if a short-lived root exited before taskkill walked its
            // descendants, the owned Job Object is the authority that remains.
            // Clean every live wrapper immediately instead of waiting for the
            // shared stage deadline before reaching the ordinary timeout arm.
            for r in running.iter_mut().filter(|r| !r.done) {
                let tree_reaped = crate::proc::terminate_and_reap_owned_std_child(r.child.as_mut());
                governor.unregister_child(&r.registry_key);
                let _ = fs::remove_file(&r.stdout_path);
                let _ = fs::remove_file(&r.stderr_path);
                r.done = true;
                timings.push(ContextCommandTiming {
                    label: r.label.clone(),
                    artifact: None,
                    status: "cancelled",
                    started: true,
                    duration_secs: r.started_at.elapsed().as_secs_f32(),
                    reason: Some(if tree_reaped {
                        "terminated because the review was cancelled".to_string()
                    } else {
                        "review cancellation requested, but process-tree termination could not be confirmed"
                            .to_string()
                    }),
                });
            }
        } else if Instant::now() >= stage_deadline {
            for (_, cmd) in pending.drain(..) {
                timings.push(ContextCommandTiming {
                    label: cmd.label.clone(),
                    artifact: None,
                    status: "timed_out",
                    started: false,
                    duration_secs: 0.0,
                    reason: Some(format!(
                        "{} did not start because the shared context-stage timeout elapsed",
                        command_identity(cmd)
                    )),
                });
            }
        }

        if running.is_empty() {
            if pending.is_empty() {
                break;
            }
            // Nothing running and nothing admitted: the budget is held
            // elsewhere. Wait for it rather than spinning.
            std::thread::sleep(poll_interval);
            continue;
        }

        // Poll everything that is running
        let mut fatal_ownership_error = None;
        for r in running.iter_mut().filter(|r| !r.done) {
            match r.child.try_wait() {
                Ok(Some(exit)) => {
                    // A wrapper may exit successfully after starting a
                    // background descendant. Keep ownership until the whole
                    // Unix process group / Windows Job Object is empty, then
                    // unregister the PID. Otherwise the pack can finish while
                    // a former context command still writes in the background.
                    let tree_reaped =
                        crate::proc::terminate_and_reap_owned_std_child(r.child.as_mut());
                    r.done = true;
                    governor.unregister_child(&r.registry_key);
                    if !tree_reaped {
                        let _ = fs::remove_file(&r.stdout_path);
                        let _ = fs::remove_file(&r.stderr_path);
                        timings.push(ContextCommandTiming {
                            label: r.label.clone(),
                            artifact: None,
                            status: "error",
                            started: true,
                            duration_secs: r.started_at.elapsed().as_secs_f32(),
                            reason: Some(
                                "direct wrapper exited but its owned process tree could not be terminated"
                                    .to_owned(),
                            ),
                        });
                        fatal_ownership_error = Some(format!(
                            "{} direct wrapper exited but its owned process tree could not be terminated",
                            r.label
                        ));
                        break;
                    }
                    // Collect output
                    collect_cmd_output(&r.stdout_path, &r.stderr_path, &r.out_dir, &r.out_file);
                    timings.push(ContextCommandTiming {
                        label: r.label.clone(),
                        artifact: (!r.out_file.is_empty())
                            .then(|| context_artifact_wire_path(&r.out_file)),
                        // A command the cancel SIGKILLed exits non-zero. That
                        // is not the tool failing, and the pack must not read
                        // as though it were.
                        status: if governor.is_cancelled() {
                            "cancelled"
                        } else if exit.success() {
                            "completed"
                        } else {
                            "failed"
                        },
                        started: true,
                        duration_secs: r.started_at.elapsed().as_secs_f32(),
                        reason: None,
                    });
                }
                Ok(None) => {
                    if Instant::now() >= r.deadline {
                        // The child leads its own Unix group or owns a Windows
                        // Job Object, so reach the whole tree even if a wrapper
                        // has already handed work to a descendant.
                        let tree_reaped =
                            crate::proc::terminate_and_reap_owned_std_child(r.child.as_mut());
                        r.done = true;
                        governor.unregister_child(&r.registry_key);
                        let _ = fs::remove_file(&r.stdout_path);
                        let _ = fs::remove_file(&r.stderr_path);
                        if !tree_reaped {
                            timings.push(ContextCommandTiming {
                                label: r.label.clone(),
                                artifact: None,
                                status: "error",
                                started: true,
                                duration_secs: r.started_at.elapsed().as_secs_f32(),
                                reason: Some(
                                    "context timeout elapsed but the owned process tree could not be terminated"
                                        .to_owned(),
                                ),
                            });
                            fatal_ownership_error = Some(format!(
                                "{} timed out but its owned process tree could not be terminated",
                                r.label
                            ));
                            break;
                        }
                        timings.push(ContextCommandTiming {
                            label: r.label.clone(),
                            artifact: None,
                            status: "timed_out",
                            started: true,
                            duration_secs: r.started_at.elapsed().as_secs_f32(),
                            reason: Some(format!("exceeded {timeout_secs}s context timeout")),
                        });
                        if emit {
                            use colored::Colorize;
                            eprintln!(
                                "  {} {}: killed (>{}s timeout)",
                                "○".dimmed(),
                                r.label,
                                timeout_secs,
                            );
                        }
                    }
                }
                Err(error) => {
                    let tree_reaped =
                        crate::proc::terminate_and_reap_owned_std_child(r.child.as_mut());
                    r.done = true;
                    governor.unregister_child(&r.registry_key);
                    let _ = fs::remove_file(&r.stdout_path);
                    let _ = fs::remove_file(&r.stderr_path);
                    if !tree_reaped {
                        timings.push(ContextCommandTiming {
                            label: r.label.clone(),
                            artifact: None,
                            status: "error",
                            started: true,
                            duration_secs: r.started_at.elapsed().as_secs_f32(),
                            reason: Some(format!(
                                "failed to query child exit status ({error}) and the owned process tree could not be terminated"
                            )),
                        });
                        fatal_ownership_error = Some(format!(
                            "{} exit status failed and its owned process tree could not be terminated",
                            r.label
                        ));
                        break;
                    }
                    timings.push(ContextCommandTiming {
                        label: r.label.clone(),
                        artifact: None,
                        status: "error",
                        started: true,
                        duration_secs: r.started_at.elapsed().as_secs_f32(),
                        reason: Some(format!("failed to query child exit status: {error}")),
                    });
                }
            }
        }

        if let Some(error) = fatal_ownership_error {
            for running in running.iter_mut().filter(|running| !running.done) {
                crate::proc::terminate_and_reap_owned_std_child(running.child.as_mut());
                governor.unregister_child(&running.registry_key);
                let _ = fs::remove_file(&running.stdout_path);
                let _ = fs::remove_file(&running.stderr_path);
                running.done = true;
            }
            return Err(anyhow::anyhow!(error));
        }

        // A finished command gives its permit and its registry slot back before
        // the next admission pass, so the budget it held goes to whatever is
        // still queued.
        running.retain_mut(|r| {
            if r.done {
                governor.unregister_child(&r.registry_key);
                drop(r.budget.take());
            }
            !r.done
        });

        if running.is_empty() && pending.is_empty() {
            break;
        }
        if !running.is_empty() {
            std::thread::sleep(poll_interval);
        }
    }

    timings.sort_by(|a, b| b.duration_secs.total_cmp(&a.duration_secs));
    Ok(timings)
}

fn context_artifact_wire_path(out_file: &str) -> String {
    format!(
        "30_context/{}",
        out_file.replace('\\', "/").trim_start_matches('/')
    )
}

/// Collect stdout+stderr from completed command temp files and write to artifact file.
pub(super) fn collect_cmd_output(
    stdout_path: &Path,
    stderr_path: &Path,
    out_dir: &Path,
    out_file: &str,
) {
    let stdout = fs::read_to_string(stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(stderr_path).unwrap_or_default();
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);

    if out_file.is_empty() {
        return;
    }

    let combined = format!("{}\n{}", stdout, stderr);

    // Truncate large outputs
    let content = if combined.len() > MAX_TSC_TRACE_BYTES {
        let truncated = truncate_on_char_boundary(&combined, MAX_TSC_TRACE_BYTES);
        format!(
            "{}\n\n... (truncated, {} total bytes)",
            truncated,
            combined.len()
        )
    } else {
        combined
    };

    // For JSON outputs, only save if valid
    if out_file.ends_with(".json") {
        if let Some(json) = extract_valid_json(&stdout, &content)
            && let Err(e) = fs::write(out_dir.join(out_file), json)
        {
            eprintln!("  warning: failed to write artifact {out_file}: {e}");
        }
    } else if let Err(e) = fs::write(out_dir.join(out_file), &content) {
        eprintln!("  warning: failed to write artifact {out_file}: {e}");
    }
}

pub(super) fn extract_valid_json(stdout: &str, combined: &str) -> Option<String> {
    [stdout.trim(), combined.trim()]
        .into_iter()
        .find(|candidate| {
            !candidate.is_empty()
                && (candidate.starts_with('[') || candidate.starts_with('{'))
                && serde_json::from_str::<serde_json::Value>(candidate).is_ok()
        })
        .map(str::to_owned)
}

pub(super) fn build_regression_patch_text(patch_texts: &[String]) -> Option<String> {
    if patch_texts.is_empty() {
        return None;
    }

    let joined = patch_texts.join("\n");
    if joined.len() <= MAX_PATCH_TEXT_BYTES {
        return Some(joined);
    }

    // Keep the in-memory regression input bounded without risking invalid UTF-8.
    let mut truncated = truncate_on_char_boundary(&joined, MAX_PATCH_TEXT_BYTES).to_string();
    truncated
        .push_str("\n\n# [prview] Patch text truncated (>2 MB), some findings may be incomplete\n");
    Some(truncated)
}

pub(super) fn truncate_on_char_boundary(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }

    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}

#[cfg(test)]
mod tests {
    use super::{
        ContextCmd, context_artifact_wire_path, context_substrate, js_exec_cmd, plan_context_cmds,
        plan_tsc_trace_artifact, reconcile_context_artifacts, tauri_info_cmd,
    };
    use crate::artifacts::{ContextArtifactDecision, ContextCommandTiming};
    use crate::checks::{CheckProvenance, CheckResult, CheckStatus};
    use crate::config::{Config, test_config, test_js_profile};
    use crate::git::cmd::git_cmd;
    use crate::governor::{ResourceGovernor, Weight};
    use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use std::time::Instant;

    /// A JS repo whose checked-out `HEAD` is NOT the reviewed target.
    ///
    /// `feature` (the target) entries through `src/main.tsx`; `main` (checked
    /// out locally) entries through `src/main.ts`. The two revisions disagree on
    /// a fact the context planner reads from disk, which is what makes "which
    /// tree did this artifact come from" observable at all.
    fn js_repo_with_off_head_target() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let run_git = |args: &[&str]| {
            let out = git_cmd()
                .args(args)
                .current_dir(root)
                .output()
                .expect("git command");
            assert!(out.status.success(), "git {args:?} failed");
        };

        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.email", "prview@example.test"]);
        run_git(&["config", "user.name", "prview test"]);
        run_git(&["config", "commit.gpgsign", "false"]);
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(root.join("package.json"), "{}\n").expect("package.json");
        fs::write(root.join("tsconfig.json"), "{}\n").expect("tsconfig.json");
        fs::write(root.join("src/main.ts"), "export {};\n").expect("main.ts");
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "main entry"]);

        run_git(&["checkout", "-q", "-b", "feature"]);
        run_git(&["rm", "-q", "src/main.ts"]);
        // `git rm` takes the now-empty directory with it.
        fs::create_dir_all(root.join("src")).expect("src");
        fs::write(root.join("src/main.tsx"), "export {};\n").expect("main.tsx");
        run_git(&["add", "."]);
        run_git(&["commit", "-q", "-m", "tsx entry"]);
        let out = git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .expect("rev-parse");
        assert!(out.status.success());
        let target = String::from_utf8(out.stdout).unwrap().trim().to_string();
        run_git(&["checkout", "-q", "main"]);

        // Untracked local tooling, the way a real checkout carries it. A
        // worktree snapshot symlinks this in, so the same binaries resolve on
        // both sides and only the SOURCE differs between the two trees.
        let bin = root.join("node_modules/.bin");
        fs::create_dir_all(&bin).expect("node_modules/.bin");
        for tool in ["tsc", "eslint", "esbuild", "stylelint"] {
            fs::write(bin.join(tool), "#!/bin/sh\n").expect("tool stub");
        }

        (tmp, target)
    }

    fn js_config(repo_root: &Path) -> Config {
        let mut config = test_config();
        config.repo_root = repo_root.to_path_buf();
        config.profile = test_js_profile(true);
        config
    }

    fn plan(config: &Config, scan_root: &Path, ledger: &TaskLedger, ctx: &Path) -> Vec<ContextCmd> {
        plan_context_cmds(config, scan_root, ledger, ctx, false, &[])
    }

    /// A gate outcome recorded the way `checks::run_all` records it: an
    /// execution lands under the substrate its own provenance named, an
    /// eligibility skip under the run's substrate once it is resolved (or an
    /// unknown one in a run that never resolves one).
    fn record_gate(ledger: &TaskLedger, check: &str, substrate: SubstrateKey, state: TaskState) {
        ledger.record(TaskEntry {
            key: TaskKey::new(check, substrate),
            kind: TaskKind::Check,
            state,
            queued_at: None,
            started_at: None,
        });
    }

    fn planned(cmds: &[ContextCmd], label: &str) -> bool {
        cmds.iter().any(|cmd| cmd.label == label)
    }

    fn context_entry(ledger: &TaskLedger, check: &str, root: &Path) -> TaskEntry {
        let entry = ledger
            .lookup_tool(check, &context_substrate(check, root, root))
            .unwrap_or_else(|| panic!("{check} must leave a decision in the ledger"));
        assert_eq!(
            entry.kind,
            TaskKind::ContextArtifact,
            "the context stage's own decision must be the latest word on {check}",
        );
        entry
    }

    fn reason_of(state: &TaskState) -> &str {
        match state {
            TaskState::Skipped { reason } | TaskState::NotApplicable { reason } => reason,
            other => panic!("expected a ruled-out state, got {other:?}"),
        }
    }

    fn esbuild_entry(cmds: &[ContextCmd]) -> String {
        let esbuild = cmds
            .iter()
            .find(|cmd| cmd.label == "esbuild meta")
            .expect("the esbuild artifact is planned in both modes");
        esbuild.args.first().expect("entry point").clone()
    }

    fn planned_tsc_trace() -> ContextArtifactDecision {
        ContextArtifactDecision {
            key: "tsc_trace",
            path: "30_context/tsc-trace.log",
            generated: true,
            recommended: false,
            reason: "generated by default for this run mode".to_owned(),
        }
    }

    #[test]
    fn context_artifact_wire_paths_always_use_canonical_slashes() {
        assert_eq!(
            context_artifact_wire_path(r"nested\trace.log"),
            "30_context/nested/trace.log"
        );
        assert_eq!(
            context_artifact_wire_path("trace.log"),
            "30_context/trace.log"
        );
    }

    #[test]
    fn timed_out_planned_artifact_reconciles_to_not_generated() {
        let root = tempfile::tempdir().expect("artifact root");
        let mut decisions = vec![planned_tsc_trace()];
        let timings = vec![ContextCommandTiming {
            label: "tsc trace".to_owned(),
            artifact: None,
            status: "timed_out",
            started: true,
            duration_secs: 30.0,
            reason: Some("exceeded 30s context timeout".to_owned()),
        }];

        reconcile_context_artifacts(&mut decisions, &timings, root.path());

        assert!(!decisions[0].generated);
        assert!(decisions[0].reason.contains("timed_out"));
        assert!(decisions[0].reason.contains("exceeded 30s"));
    }

    #[test]
    fn completed_existing_context_output_reconciles_to_generated() {
        let root = tempfile::tempdir().expect("artifact root");
        fs::create_dir_all(root.path().join("30_context")).expect("context directory");
        fs::write(root.path().join("30_context/tsc-trace.log"), "trace").expect("context output");
        let mut decisions = vec![planned_tsc_trace()];
        let timings = vec![ContextCommandTiming {
            label: "tsc trace".to_owned(),
            artifact: Some("30_context/tsc-trace.log".to_owned()),
            status: "completed",
            started: true,
            duration_secs: 0.1,
            reason: None,
        }];

        reconcile_context_artifacts(&mut decisions, &timings, root.path());

        assert!(decisions[0].generated);
        assert!(decisions[0].reason.contains("completed"));
        assert!(decisions[0].reason.contains("output exists"));
    }

    #[test]
    fn planned_artifact_without_command_or_output_reconciles_to_not_generated() {
        let root = tempfile::tempdir().expect("artifact root");
        let mut decisions = vec![planned_tsc_trace()];

        reconcile_context_artifacts(&mut decisions, &[], root.path());

        assert!(!decisions[0].generated);
        assert!(decisions[0].reason.contains("no runnable context command"));
    }

    /// PRV-CONTEXT-SNAPSHOT-PROVENANCE, half two: in a `--pr`-style run the
    /// gates judge a snapshot of the reviewed commit, but every context command
    /// used to be pinned to `config.repo_root` — so one pack described two
    /// revisions at once. Every cwd and every filesystem probe must follow the
    /// reviewed tree instead.
    #[test]
    fn context_commands_run_in_the_reviewed_snapshot_not_the_local_checkout() {
        let (repo, target) = js_repo_with_off_head_target();
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let snapshot = crate::git::create_worktree_snapshot(repo.path(), &target)
            .expect("worktree snapshot of the reviewed commit");
        let ledger = TaskLedger::new();
        ledger.set_shared_snapshot(Some(snapshot));

        let scan_root = ledger.scan_dir().expect("the ledger owns the snapshot");
        let cmds = plan(&config, &scan_root, &ledger, ctx.path());

        assert!(
            !cmds.is_empty(),
            "a JS profile must plan context commands at all"
        );
        for cmd in &cmds {
            assert_eq!(
                cmd.cwd,
                scan_root,
                "{} must run in the reviewed snapshot, not in {}",
                cmd.label,
                config.repo_root.display(),
            );
            // A tool resolved out of the local checkout would be a second,
            // quieter way for the local tree to leak back in.
            if cmd.cmd.contains("node_modules") {
                assert!(
                    PathBuf::from(&cmd.cmd).starts_with(&scan_root),
                    "{} resolved its binary outside the snapshot: {}",
                    cmd.label,
                    cmd.cmd,
                );
            }
        }
        assert_eq!(
            esbuild_entry(&cmds),
            "src/main.tsx",
            "the entry point must be read from the reviewed tree; src/main.ts is \
             what the LOCAL checkout has",
        );
    }

    /// The other half of the same contract: with no shared snapshot — an
    /// ordinary local review — the reviewed tree IS the repo root, and nothing
    /// about the previous behaviour may change.
    #[test]
    fn context_commands_stay_on_the_repo_root_without_a_shared_snapshot() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let ledger = TaskLedger::new();
        assert!(ledger.scan_dir().is_none(), "no snapshot was materialised");
        let scan_root = ledger
            .scan_dir()
            .unwrap_or_else(|| config.repo_root.clone());

        let cmds = plan(&config, &scan_root, &ledger, ctx.path());

        assert_eq!(scan_root, config.repo_root);
        for cmd in &cmds {
            assert_eq!(
                cmd.cwd, config.repo_root,
                "{} moved off the repo root",
                cmd.label
            );
        }
        assert_eq!(
            esbuild_entry(&cmds),
            "src/main.ts",
            "a local review reads the working tree, which entries through main.ts",
        );
    }

    /// PRV-CONTEXT-WORK-DEDUP, the inverted compensation. A fast remote-only
    /// preset rules the lint gate out ON PURPOSE — to avoid a full-tree lint —
    /// and the context stage used to read the resulting hole in the results list
    /// as "nobody linted this, better do it myself", spending 23 s on exactly
    /// the work the preset had excluded.
    #[test]
    fn a_gate_the_preset_ruled_out_is_not_compensated_for() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "ESLint",
            // An eligibility skip is recorded before the run resolves its
            // substrate, so it really does land under an unknown one.
            SubstrateKey::default(),
            TaskState::Skipped {
                reason: "fast remote-only preset".to_string(),
            },
        );

        let cmds = plan(&config, repo.path(), &ledger, ctx.path());

        assert!(
            !planned(&cmds, "eslint json"),
            "the preset excluded the lint; the context stage must not reinstate it",
        );
        let entry = context_entry(&ledger, "ESLint", repo.path());
        assert!(
            matches!(entry.state, TaskState::Skipped { .. }),
            "the tree can run eslint; this run chose not to, got {:?}",
            entry.state,
        );
        assert!(
            reason_of(&entry.state).contains("fast remote-only preset"),
            "the ledger must carry the gate's own reason, got {:?}",
            entry.state,
        );
    }

    /// The environmental half of the same decision: a tool the reviewed tree
    /// cannot run is `NotApplicable` — no preset would produce this artifact
    /// here — while a configuration exclusion stays `Skipped`.
    #[test]
    fn a_tool_missing_from_the_reviewed_tree_is_not_applicable() {
        let (repo, _target) = js_repo_with_off_head_target();
        fs::remove_file(repo.path().join("node_modules/.bin/stylelint")).expect("drop stylelint");
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "Stylelint",
            SubstrateKey::default(),
            TaskState::Skipped {
                reason: "tool not installed (node_modules/.bin/stylelint is missing)".to_string(),
            },
        );

        let cmds = plan(&config, repo.path(), &ledger, ctx.path());

        assert!(!planned(&cmds, "stylelint json"));
        let entry = context_entry(&ledger, "Stylelint", repo.path());
        assert!(
            matches!(entry.state, TaskState::NotApplicable { .. }),
            "an absent tool is not a choice this run made, got {:?}",
            entry.state,
        );
    }

    /// The behaviour that already worked must keep working, and now says so in
    /// the ledger: a gate that ran leaves the artifact deduped against the
    /// substrate that gate actually read.
    #[test]
    fn a_gate_that_ran_leaves_the_artifact_deduped() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let substrate = context_substrate("ESLint", repo.path(), repo.path());
        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "ESLint",
            substrate.clone(),
            TaskState::Run {
                duration: Duration::from_secs(23),
            },
        );

        let cmds = plan(&config, repo.path(), &ledger, ctx.path());

        assert!(
            !planned(&cmds, "eslint json"),
            "the gate already linted this tree",
        );
        let entry = context_entry(&ledger, "ESLint", repo.path());
        assert_eq!(
            entry.state,
            TaskState::Reused { origin: substrate },
            "same-run coverage is reuse, not a cache replay",
        );
    }

    /// A cache replay is still a replay: the context row must keep `cached` and
    /// the age of the stored entry, not pretend the same-run gate just ran.
    #[test]
    fn a_gate_that_replayed_cache_stays_cached() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let origin = SubstrateKey {
            target_sha: Some("older".to_string()),
            tree_state: Some(crate::checks::TreeState::LocalDirty),
        };
        let substrate = context_substrate("ESLint", repo.path(), repo.path());
        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "ESLint",
            substrate,
            TaskState::Cached {
                cache_age_secs: Some(42),
                origin: origin.clone(),
            },
        );

        let cmds = plan(&config, repo.path(), &ledger, ctx.path());
        assert!(!planned(&cmds, "eslint json"));
        let entry = context_entry(&ledger, "ESLint", repo.path());
        assert_eq!(
            entry.state,
            TaskState::Cached {
                cache_age_secs: Some(42),
                origin,
            },
            "a stored replay stays cached, with the original entry's age",
        );
    }

    /// A gate that executed and FAILED still executed. The tool read the tree
    /// and reported on it; re-running it in the context stage would buy the
    /// same answer at the same price, so a failing gate dedups exactly like a
    /// passing one.
    #[test]
    fn a_gate_that_ran_and_failed_still_dedups() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let ledger = TaskLedger::new();
        // `run_all` records an execution as `Run` whatever the check concluded
        // — passed, failed or errored — so a failing gate looks like this.
        record_gate(
            &ledger,
            "ESLint",
            context_substrate("ESLint", repo.path(), repo.path()),
            TaskState::Run {
                duration: Duration::from_secs(1),
            },
        );

        assert!(!planned(
            &plan(&config, repo.path(), &ledger, ctx.path()),
            "eslint json",
        ));
    }

    /// The one case where compensating is still right: no gate for this tool
    /// resolved at all, so nothing was decided about it and the context artifact
    /// is the only place its signal can come from. Unchanged old behaviour.
    #[test]
    fn a_tool_no_gate_decided_on_is_still_generated() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = js_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let ledger = TaskLedger::new();
        let cmds = plan(&config, repo.path(), &ledger, ctx.path());

        assert!(
            planned(&cmds, "eslint json"),
            "an empty ledger is a gap, not a decision",
        );
        assert!(
            ledger
                .lookup_tool(
                    "ESLint",
                    &context_substrate("ESLint", repo.path(), repo.path())
                )
                .is_none(),
            "a planned command records no decision NOT to run",
        );
    }

    fn deep_ts_config(repo_root: &Path) -> Config {
        let mut config = js_config(repo_root);
        // Not a fast remote-only run: the branch where the trace used to be
        // generated unconditionally.
        config.remote_only = false;
        assert!(!config.is_fast_remote_only_standard());
        config
    }

    fn typescript_result(status: CheckStatus, output: &str) -> CheckResult {
        CheckResult {
            name: "TypeScript".to_string(),
            status,
            duration: Duration::from_secs(8),
            output: output.to_string(),
            cached: false,
            provenance: None::<CheckProvenance>,
        }
    }

    /// PRV-CONTEXT-WORK-DEDUP, the second compile. A deep run compiled the
    /// reviewed tree as the TypeScript gate and then compiled it again as a
    /// `tsc` trace, because "generated by default for this run mode" never asked
    /// whether the work had already been done.
    #[test]
    fn a_tsc_trace_is_deduped_against_the_typescript_gate() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = deep_ts_config(repo.path());

        let substrate = context_substrate("TypeScript", repo.path(), repo.path());
        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "TypeScript",
            substrate.clone(),
            TaskState::Run {
                duration: Duration::from_secs(8),
            },
        );

        let decision = plan_tsc_trace_artifact(
            &config,
            repo.path(),
            &[],
            &[typescript_result(CheckStatus::Passed, "")],
            &ledger,
        );

        assert!(
            !decision.generated,
            "the gate compiled this tree already: {}",
            decision.reason,
        );
        assert!(!decision.recommended);
        let entry = context_entry(&ledger, "TypeScript", repo.path());
        assert_eq!(
            entry.state,
            TaskState::Reused { origin: substrate },
            "the deduped artifact must name the live gate it stands on",
        );
    }

    /// The decision has to reach the command planner, or the dedup is a claim in
    /// `RUN.json` and a second 8 s compile on the machine.
    #[test]
    fn a_deduped_tsc_trace_plans_no_command() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = deep_ts_config(repo.path());
        let ctx = tempfile::tempdir().expect("context dir");

        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "TypeScript",
            context_substrate("TypeScript", repo.path(), repo.path()),
            TaskState::Run {
                duration: Duration::from_secs(8),
            },
        );
        let decisions = vec![plan_tsc_trace_artifact(
            &config,
            repo.path(),
            &[],
            &[],
            &ledger,
        )];

        let cmds = plan_context_cmds(&config, repo.path(), &ledger, ctx.path(), false, &decisions);

        assert!(
            !planned(&cmds, "tsc trace"),
            "the trace was deduped, so nothing may spawn tsc",
        );
        // Control: the same fixture DOES plan the trace when no gate covered it,
        // so the assertion above is about the dedup and not about a missing tsc.
        let fresh = TaskLedger::new();
        let undeduped = vec![plan_tsc_trace_artifact(
            &config,
            repo.path(),
            &[],
            &[],
            &fresh,
        )];
        assert!(planned(
            &plan_context_cmds(&config, repo.path(), &fresh, ctx.path(), false, &undeduped),
            "tsc trace",
        ));
    }

    /// The exception that stays: a module-resolution failure is the one case
    /// where the trace answers something the gate's output cannot, so the second
    /// compile buys something and the dedup steps aside.
    #[test]
    fn a_resolution_failure_still_forces_the_trace() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = deep_ts_config(repo.path());

        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "TypeScript",
            context_substrate("TypeScript", repo.path(), repo.path()),
            TaskState::Run {
                duration: Duration::from_secs(8),
            },
        );

        let decision = plan_tsc_trace_artifact(
            &config,
            repo.path(),
            &[],
            &[typescript_result(
                CheckStatus::Failed,
                "src/app.ts(3,20): error TS2307: Cannot find module '@/lib/x'",
            )],
            &ledger,
        );

        assert!(
            decision.generated,
            "a resolution failure is exactly what a trace is for: {}",
            decision.reason,
        );
        assert!(
            ledger
                .lookup_tool(
                    "TypeScript",
                    &context_substrate("TypeScript", repo.path(), repo.path())
                )
                .map(|entry| entry.kind)
                == Some(TaskKind::Check),
            "a generated artifact records no decision NOT to run",
        );
    }

    /// No gate compiled this tree, so the trace is the only compile there is —
    /// unchanged behaviour.
    #[test]
    fn a_tsc_trace_without_a_typescript_gate_is_still_generated() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = deep_ts_config(repo.path());
        let ledger = TaskLedger::new();

        let decision = plan_tsc_trace_artifact(&config, repo.path(), &[], &[], &ledger);

        assert!(decision.generated, "{}", decision.reason);
    }

    /// A gate that ran on ANOTHER tree is not this tree's compile, so it cannot
    /// stand in for one. The lookup's one-directional fallback is what keeps a
    /// deduped artifact honest about which bytes it describes.
    #[test]
    fn a_tsc_trace_is_not_deduped_against_another_tree() {
        let (repo, _target) = js_repo_with_off_head_target();
        let config = deep_ts_config(repo.path());

        let ledger = TaskLedger::new();
        record_gate(
            &ledger,
            "TypeScript",
            SubstrateKey {
                target_sha: Some("0000000000000000000000000000000000000000".to_string()),
                tree_state: Some(crate::checks::TreeState::Snapshot),
            },
            TaskState::Run {
                duration: Duration::from_secs(8),
            },
        );

        let decision = plan_tsc_trace_artifact(&config, repo.path(), &[], &[], &ledger);

        assert!(
            decision.generated,
            "another commit's compile says nothing about this one: {}",
            decision.reason,
        );
    }

    /// `tauri info` has no gate to dedup against, so its behaviour is untouched
    /// — but a deferred artifact now says so in the ledger instead of only in
    /// `RUN.json`.
    #[test]
    fn a_deferred_tauri_info_records_its_reason() {
        let (repo, _target) = js_repo_with_off_head_target();
        let mut config = js_config(repo.path());
        config.remote_only = true;
        assert!(config.is_fast_remote_only_standard());

        let ledger = TaskLedger::new();
        let decision = super::plan_tauri_info_artifact(&config, repo.path(), &[], &ledger);

        assert!(!decision.generated);
        let entry = context_entry(&ledger, "tauri info", repo.path());
        assert_eq!(
            reason_of(&entry.state),
            decision.reason,
            "the ledger and RUN.json must give one account of the same decision",
        );
    }

    #[test]
    fn tauri_info_prefers_local_binary_over_npx() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("node_modules/.bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("tauri"), "#!/bin/sh\n").unwrap();

        // has_pnpm=true must NOT win over a resolved local binary.
        let (cmd, args) = tauri_info_cmd(tmp.path(), true).expect("local binary resolves");
        assert!(
            cmd.ends_with("node_modules/.bin/tauri"),
            "expected a direct local exec, got {cmd}"
        );
        assert_eq!(args, vec!["info".to_string()]);
        assert!(
            !args.iter().any(|a| a == "--no-install"),
            "a resolved binary must never carry npx flags"
        );
    }

    #[test]
    fn tauri_info_falls_back_to_pnpm_without_local_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let (cmd, args) = tauri_info_cmd(tmp.path(), true).expect("pnpm fallback");
        assert_eq!(cmd, "pnpm");
        assert_eq!(args, vec!["tauri".to_string(), "info".to_string()]);
    }

    #[test]
    fn tauri_info_skips_when_no_local_binary_and_no_pnpm() {
        let tmp = tempfile::tempdir().unwrap();
        // No local tauri and no pnpm: skip rather than fall through to
        // npx --no-install, which can hang on a missing CLI (PR #12 review).
        assert!(tauri_info_cmd(tmp.path(), false).is_none());
    }

    // Covers the pnpm-exec-class JS-tool sites (tsc trace, eslint json,
    // esbuild meta) that previously fell through to `npx --no-install`.
    #[test]
    fn js_exec_prefers_local_binary_over_npx() {
        for tool in ["tsc", "eslint", "esbuild"] {
            let tmp = tempfile::tempdir().unwrap();
            let bin_dir = tmp.path().join("node_modules/.bin");
            fs::create_dir_all(&bin_dir).unwrap();
            fs::write(bin_dir.join(tool), "#!/bin/sh\n").unwrap();

            // has_pnpm=true must NOT win over a resolved local binary.
            let (cmd, args) =
                js_exec_cmd(tool, vec!["--flag".into()], tmp.path(), true).expect("local resolves");
            assert!(
                cmd.ends_with(&format!("node_modules/.bin/{tool}")),
                "expected a direct local exec for {tool}, got {cmd}"
            );
            // A resolved binary carries only the tool args, no `exec`/npx flags.
            assert_eq!(args, vec!["--flag".to_string()], "tool={tool}");
            assert!(
                !args.iter().any(|a| a == "--no-install" || a == "exec"),
                "a resolved binary must never carry launcher flags (tool={tool})"
            );
        }
    }

    #[test]
    fn js_exec_falls_back_to_pnpm_exec_without_local_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let (cmd, args) =
            js_exec_cmd("eslint", vec!["--flag".into()], tmp.path(), true).expect("pnpm fallback");
        assert_eq!(cmd, "pnpm");
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "eslint".to_string(),
                "--flag".to_string()
            ]
        );
        assert!(
            !args.iter().any(|a| a == "--no-install"),
            "pnpm exec fallback must never carry npx flags"
        );
    }

    #[test]
    fn js_exec_skips_when_no_local_binary_and_no_pnpm() {
        let tmp = tempfile::tempdir().unwrap();
        // No local binary and no pnpm: skip rather than fall through to
        // npx --no-install, which can hang on a missing CLI (PR #12 review).
        assert!(js_exec_cmd("tsc", vec!["--flag".into()], tmp.path(), false).is_none());
    }

    fn context_cmd(label: &str, gate: Option<&'static str>, cmd: &str, cwd: &Path) -> ContextCmd {
        ContextCmd {
            label: label.to_string(),
            gate,
            cmd: cmd.to_string(),
            args: Vec::new(),
            cwd: cwd.to_path_buf(),
            out_dir: cwd.to_path_buf(),
            out_file: String::new(),
        }
    }

    /// The half of the account the ledger used to be missing: the planner
    /// recorded only what it decided NOT to run, so a run could say why it
    /// skipped a tool but not that it had just spent time on one.
    #[test]
    fn an_executed_context_command_is_recorded_as_a_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmds = vec![
            // A gate-backed command answers under the GATE's id, not its label,
            // so the two halves of one task cannot land under two ids.
            context_cmd("eslint json", Some("ESLint"), "/bin/echo", tmp.path()),
            // No gate counterpart: recorded under its own slugged label.
            context_cmd("cargo tree", None, "/bin/echo", tmp.path()),
        ];

        let ledger = TaskLedger::new();
        let governor = ResourceGovernor::new();
        let timings = super::run_context_cmds_parallel(&cmds, 30, false, &governor)
            .expect("context commands");
        super::record_context_runs(&ledger, tmp.path(), &cmds, &timings);

        let entries = ledger.entries();
        assert_eq!(entries.len(), 2, "one entry per executed command");
        for entry in &entries {
            assert_eq!(entry.kind, TaskKind::ContextArtifact);
            match entry.state {
                TaskState::Run { duration } => assert!(
                    duration > Duration::ZERO,
                    "an executed command reports the time it actually took",
                ),
                ref other => panic!("expected a run, got {other:?}"),
            }
        }
        let tools: Vec<&str> = entries.iter().map(|e| e.key.tool.as_str()).collect();
        assert!(tools.contains(&"eslint"), "got {tools:?}");
        assert!(tools.contains(&"cargo_tree"), "got {tools:?}");
    }

    #[test]
    fn linked_snapshot_context_tools_record_borrowed_dependencies() {
        let (repo, target) = js_repo_with_off_head_target();
        let snapshot = crate::git::create_worktree_snapshot(repo.path(), &target)
            .expect("worktree snapshot of the reviewed commit");
        let scan_root = snapshot.worktree_path.clone();
        if !scan_root.join("node_modules").is_symlink() {
            // Snapshot dependency links are currently a Unix contract.
            return;
        }
        let cmds = ["tauri info", "esbuild meta", "npm sbom"]
            .into_iter()
            .map(|label| context_cmd(label, None, "/bin/echo", &scan_root))
            .collect::<Vec<_>>();

        let ledger = TaskLedger::new();
        let governor = ResourceGovernor::new();
        let timings = super::run_context_cmds_parallel(&cmds, 30, false, &governor)
            .expect("context commands");
        super::record_context_runs(&ledger, repo.path(), &cmds, &timings);

        let entries = ledger.entries();
        assert_eq!(entries.len(), 3);
        for entry in entries {
            assert!(matches!(entry.state, TaskState::Run { .. }));
            assert_eq!(
                entry.key.substrate.tree_state,
                Some(crate::checks::TreeState::SnapshotBorrowedDeps),
                "{} consumes the linked node_modules tree",
                entry.key.tool,
            );
        }
    }

    #[test]
    fn skipped_linked_snapshot_context_tool_keeps_borrowed_provenance() {
        let (repo, target) = js_repo_with_off_head_target();
        let snapshot = crate::git::create_worktree_snapshot(repo.path(), &target)
            .expect("worktree snapshot of the reviewed commit");
        let scan_root = snapshot.worktree_path.clone();
        if !scan_root.join("node_modules").is_symlink() {
            return;
        }
        let cmds = vec![context_cmd(
            "tauri info",
            None,
            &scan_root.join("no-such-tauri").display().to_string(),
            &scan_root,
        )];
        let governor = ResourceGovernor::new();
        let timings = super::run_context_cmds_parallel(&cmds, 30, false, &governor)
            .expect("context command accounting");
        assert_eq!(timings[0].status, "spawn_failed");

        let ledger = TaskLedger::new();
        super::record_context_runs(&ledger, repo.path(), &cmds, &timings);
        let entry = &ledger.entries()[0];
        assert!(matches!(entry.state, TaskState::Skipped { .. }));
        assert_eq!(
            entry.key.substrate.tree_state,
            Some(crate::checks::TreeState::SnapshotBorrowedDeps),
        );
    }

    #[test]
    fn context_tool_without_dependency_link_records_exact_snapshot() {
        let (repo, target) = js_repo_with_off_head_target();
        fs::remove_dir_all(repo.path().join("node_modules")).expect("remove local dependency tree");
        let snapshot = crate::git::create_worktree_snapshot(repo.path(), &target)
            .expect("worktree snapshot without borrowed dependencies");
        let scan_root = snapshot.worktree_path.clone();
        let cmds = vec![context_cmd("npm sbom", None, "/bin/echo", &scan_root)];
        let governor = ResourceGovernor::new();
        let timings =
            super::run_context_cmds_parallel(&cmds, 30, false, &governor).expect("context command");

        let ledger = TaskLedger::new();
        super::record_context_runs(&ledger, repo.path(), &cmds, &timings);
        assert_eq!(
            ledger.entries()[0].key.substrate.tree_state,
            Some(crate::checks::TreeState::Snapshot),
        );
    }

    /// A sleeping context command, for the tests that care about scheduling
    /// rather than about output.
    fn sleeping_cmd(label: &str, cwd: &Path, secs: &str) -> ContextCmd {
        ContextCmd {
            label: label.to_string(),
            gate: None,
            cmd: "sh".to_string(),
            args: vec!["-c".to_string(), format!("sleep {secs}")],
            cwd: cwd.to_path_buf(),
            out_dir: cwd.to_path_buf(),
            out_file: String::new(),
        }
    }

    /// The classification is one list and it is load-bearing, so it is asserted
    /// rather than left to the reader of the match arm.
    #[test]
    fn context_commands_declare_the_same_weights_the_gates_do() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for label in ["tsc trace", "eslint json", "stylelint json", "esbuild meta"] {
            assert_eq!(
                super::context_cmd_weight(&sleeping_cmd(label, tmp.path(), "0")),
                Weight::Exclusive,
                "{label} has no portable descendant-worker cap",
            );
        }
        for label in ["cargo tree", "cargo sbom", "npm sbom", "tauri info"] {
            assert_eq!(
                super::context_cmd_weight(&sleeping_cmd(label, tmp.path(), "0")),
                Weight::Light,
                "{label} reads metadata",
            );
        }
    }

    /// The budget bounds the context stage the same way it bounds the gates:
    /// with room for one heavy command, three of them run one after another
    /// instead of all at once. Asserted on wall time as a LOWER bound — the
    /// unbounded version finishes in about one sleep, the bounded one cannot.
    #[test]
    fn context_commands_wait_for_the_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmds: Vec<ContextCmd> = ["tsc trace", "eslint json", "stylelint json"]
            .into_iter()
            .map(|label| sleeping_cmd(label, tmp.path(), "0.3"))
            .collect();

        // Heavy costs the whole budget, so exactly one command runs at a time.
        let governor = ResourceGovernor::with_budget(2, 2);
        let started = Instant::now();
        let timings = super::run_context_cmds_parallel(&cmds, 30, false, &governor)
            .expect("context commands");
        let elapsed = started.elapsed();

        assert_eq!(timings.len(), 3, "every command still runs and reports");
        assert!(
            timings.iter().all(|t| t.status == "completed"),
            "got {:?}",
            timings.iter().map(|t| t.status).collect::<Vec<_>>(),
        );
        assert!(
            elapsed >= Duration::from_millis(750),
            "three serialised 0.3s commands cannot finish in {elapsed:?}",
        );
    }

    /// Exclusive admission used to mint a fresh deadline per spawn, so N hanging
    /// commands took N timeouts. The stage clock is shared.
    #[test]
    fn hanging_context_commands_share_one_stage_timeout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmds: Vec<ContextCmd> = ["tsc trace", "eslint json"]
            .into_iter()
            .map(|label| sleeping_cmd(label, tmp.path(), "30"))
            .collect();
        let governor = ResourceGovernor::with_budget(2, 1);
        let started = Instant::now();
        let timings =
            super::run_context_cmds_parallel(&cmds, 1, false, &governor).expect("context commands");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1800),
            "two exclusive hangs must share one 1s stage timeout, took {elapsed:?}"
        );
        assert_eq!(timings.len(), 2);
        assert!(
            timings.iter().all(|t| t.status == "timed_out"),
            "got {:?}",
            timings.iter().map(|t| t.status).collect::<Vec<_>>(),
        );
        assert_eq!(
            timings.iter().filter(|timing| timing.started).count(),
            1,
            "only the admitted command executed; the second expired in queue"
        );

        let ledger = TaskLedger::new();
        super::record_context_runs(&ledger, tmp.path(), &cmds, &timings);
        assert_eq!(
            ledger
                .entries()
                .iter()
                .filter(|entry| matches!(entry.state, TaskState::Run { .. }))
                .count(),
            1
        );
        assert_eq!(
            ledger
                .entries()
                .iter()
                .filter(|entry| matches!(entry.state, TaskState::Skipped { .. }))
                .count(),
            1,
            "a stage deadline reached before spawn is skipped, not executed"
        );
    }

    /// Cancellation can only reach a context child the governor knows about, and
    /// only for as long as that pid is really its child. Both halves are the
    /// test: the pid appears while the command runs and is gone afterwards.
    #[test]
    fn a_running_context_command_is_registered_with_the_governor() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmds = vec![sleeping_cmd("cargo tree", tmp.path(), "1")];
        let governor = ResourceGovernor::new();

        std::thread::scope(|scope| {
            let runner =
                scope.spawn(|| super::run_context_cmds_parallel(&cmds, 30, false, &governor));

            let mut seen = false;
            for _ in 0..200 {
                if governor.inflight_count() == 1 {
                    seen = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                seen,
                "a spawned context command must be reachable by cancel"
            );

            let timings = runner
                .join()
                .expect("runner must not panic")
                .expect("context commands");
            assert_eq!(timings.len(), 1);
        });

        assert_eq!(
            governor.inflight_count(),
            0,
            "a finished command leaves no pid the governor may signal",
        );
    }

    /// The synchronous spawn seam has the same late-registration window as the
    /// async check runner. Force cancellation after `spawn` but before
    /// `register_child`, then prove the worker-owned process group is reaped
    /// without waiting for the context timeout.
    #[cfg(unix)]
    #[test]
    fn registration_after_cancellation_kills_the_sync_process_group() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("late-context-grandchild.pid");
        let script = format!("sleep 30 & echo $! > {} ; wait", pidfile.display());
        let cmds = vec![ContextCmd {
            label: "late context tree".to_owned(),
            gate: None,
            cmd: "sh".to_owned(),
            args: vec!["-c".to_owned(), script],
            cwd: tmp.path().to_path_buf(),
            out_dir: tmp.path().to_path_buf(),
            out_file: String::new(),
        }];
        let governor = ResourceGovernor::new();
        let marker = pidfile.clone();

        let timings =
            super::run_context_cmds_parallel_after_spawn(&cmds, 2, false, &governor, |_| {
                for _ in 0..100 {
                    if crate::proc::read_published_unix_pid(&marker).is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(
                    crate::proc::read_published_unix_pid(&marker).is_some(),
                    "child must publish its complete grandchild pid"
                );
                governor.cancel();
            })
            .expect("cancelled context commands still return timings");

        let grandchild = crate::proc::read_published_unix_pid(&pidfile)
            .expect("complete numeric grandchild pid");
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 is a read-only existence/permission probe, and
            // `grandchild` came from the process tree created by this test.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) || errno == Some(libc::EPERM) {
                    gone = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(gone, "grandchild {grandchild} survived late registration");
        assert_eq!(governor.inflight_count(), 0);
        assert_eq!(timings[0].status, "cancelled");
    }

    /// A context wrapper that exits 0 may still have left a background process
    /// in its owned group. Success is not permission to detach that work from
    /// the review: the descendant must be gone before the timing is finalized.
    #[cfg(unix)]
    #[test]
    fn successful_context_root_exit_reaps_background_descendant() {
        use std::io::Read;

        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("successful-context-grandchild.pid");
        let script = format!("sleep 30 & echo $! > {}", pidfile.display());
        let cmds = vec![ContextCmd {
            label: "successful context tree".to_owned(),
            gate: None,
            cmd: "sh".to_owned(),
            args: vec!["-c".to_owned(), script],
            cwd: tmp.path().to_path_buf(),
            out_dir: tmp.path().to_path_buf(),
            out_file: String::new(),
        }];
        let governor = ResourceGovernor::new();

        let started = Instant::now();
        let timings =
            super::run_context_cmds_parallel(&cmds, 5, false, &governor).expect("context commands");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(timings.len(), 1);
        assert_eq!(timings[0].status, "completed");

        let mut contents = String::new();
        std::fs::File::open(&pidfile)
            .expect("context wrapper records its background pid")
            .read_to_string(&mut contents)
            .expect("read background pid");
        let grandchild: i32 = contents.trim().parse().expect("numeric grandchild pid");
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 is a read-only existence probe for the PID this
            // test's process group created and is responsible for reaping.
            if unsafe { libc::kill(grandchild, 0) } == -1 {
                let errno = std::io::Error::last_os_error().raw_os_error();
                if errno == Some(libc::ESRCH) || errno == Some(libc::EPERM) {
                    gone = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone, "background context descendant {grandchild} survived");
        assert_eq!(governor.inflight_count(), 0);
    }

    /// The synchronous context runner uses the same durable Windows ownership
    /// contract as async checks: a successful root PID may disappear, but its
    /// background child remains in the Job Object until prview terminates it.
    #[cfg(windows)]
    #[test]
    fn windows_successful_context_root_exit_reaps_job_descendant() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let child_pidfile = tmp.path().join("context-background-child.pid");
        let child_script = tmp.path().join("context-background-child.ps1");
        let parent_script = tmp.path().join("context-successful-parent.ps1");

        std::fs::write(&child_script, "Start-Sleep -Seconds 60\n")
            .expect("write Windows context child script");
        std::fs::write(
            &parent_script,
            format!(
                "$c = Start-Process -PassThru powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-File','{}'\nSet-Content -LiteralPath '{}' -Value $c.Id\nexit 0\n",
                child_script.display(),
                child_pidfile.display(),
            ),
        )
        .expect("write Windows context parent script");

        let cmds = vec![ContextCmd {
            label: "successful Windows context tree".to_owned(),
            gate: None,
            cmd: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-File".to_owned(),
                parent_script.display().to_string(),
            ],
            cwd: tmp.path().to_path_buf(),
            out_dir: tmp.path().to_path_buf(),
            out_file: String::new(),
        }];
        let governor = ResourceGovernor::new();
        let timings = super::run_context_cmds_parallel(&cmds, 10, false, &governor)
            .expect("context commands");
        assert_eq!(timings.len(), 1);
        assert_eq!(timings[0].status, "completed");

        let child_pid: u32 = std::fs::read_to_string(&child_pidfile)
            .expect("context parent records child pid")
            .trim()
            .parse()
            .expect("numeric Windows context child pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::proc::windows_pid_exists(child_pid) {
            assert!(
                Instant::now() < deadline,
                "Windows context Job Object descendant {child_pid} survived root exit",
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(governor.inflight_count(), 0);
    }

    /// A cancelled run starts nothing further, and says so about what it did
    /// not start. Silence would leave the pack claiming a context command was
    /// simply not planned.
    #[test]
    fn a_cancelled_run_starts_no_further_context_commands() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmds = vec![
            sleeping_cmd("cargo tree", tmp.path(), "30"),
            sleeping_cmd("npm sbom", tmp.path(), "30"),
        ];

        let governor = ResourceGovernor::new();
        governor.cancel();

        let started = Instant::now();
        let timings = super::run_context_cmds_parallel(&cmds, 30, false, &governor)
            .expect("context commands");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a cancelled stage must not wait out a budget that is never coming back",
        );
        assert_eq!(timings.len(), 2, "both commands are accounted for");
        assert!(
            timings.iter().all(|t| t.status == "cancelled"),
            "got {:?}",
            timings.iter().map(|t| t.status).collect::<Vec<_>>(),
        );

        // And a cancelled command is not a run: it read nothing.
        let ledger = TaskLedger::new();
        super::record_context_runs(&ledger, tmp.path(), &cmds, &timings);
        for entry in ledger.entries() {
            assert!(
                matches!(entry.state, TaskState::Skipped { .. }),
                "got {:?}",
                entry.state,
            );
        }
    }

    /// A command that never started did not read the tree. Recording it as a
    /// zero-second run would say it did.
    #[test]
    fn a_context_command_that_never_spawned_is_not_a_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cmds = vec![context_cmd(
            "tauri info",
            None,
            &tmp.path().join("no-such-binary").display().to_string(),
            tmp.path(),
        )];

        let ledger = TaskLedger::new();
        let governor = ResourceGovernor::new();
        let timings = super::run_context_cmds_parallel(&cmds, 30, false, &governor)
            .expect("context commands");
        assert_eq!(timings[0].status, "spawn_failed");
        assert!(
            timings[0]
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no-such-binary")),
            "the ledger timing must identify the command that failed"
        );
        super::record_context_runs(&ledger, tmp.path(), &cmds, &timings);

        let entry = &ledger.entries()[0];
        assert_eq!(entry.key.tool, "tauri_info");
        assert!(
            matches!(entry.state, TaskState::Skipped { .. }),
            "got {:?}",
            entry.state,
        );
    }

    /// Capture-file creation is part of spawning a command. If it fails, the
    /// command must still receive one deterministic ledger/timing row with its
    /// identity and the filesystem reason.
    #[test]
    fn a_context_capture_file_failure_is_ledgered_as_spawn_failed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let not_a_dir = tmp.path().join("capture-parent-is-a-file");
        fs::write(&not_a_dir, "not a directory").expect("fixture file");

        let mut cmd = context_cmd("cargo tree", None, "/bin/echo", tmp.path());
        cmd.args = vec!["hello".to_string()];
        cmd.out_dir = not_a_dir;
        let cmds = vec![cmd];
        let governor = ResourceGovernor::new();
        let timings = super::run_context_cmds_parallel(&cmds, 30, false, &governor)
            .expect("context commands");

        assert_eq!(timings.len(), 1);
        assert_eq!(timings[0].status, "spawn_failed");
        let reason = timings[0].reason.as_deref().expect("failure reason");
        assert!(reason.contains("/bin/echo hello"), "got {reason}");
        assert!(reason.contains("stdout capture"), "got {reason}");

        let ledger = TaskLedger::new();
        super::record_context_runs(&ledger, tmp.path(), &cmds, &timings);
        let entries = ledger.entries();
        let TaskState::Skipped {
            reason: ledger_reason,
        } = &entries[0].state
        else {
            panic!(
                "capture failure must be skipped, got {:?}",
                entries[0].state
            );
        };
        assert_eq!(ledger_reason, reason);
    }
}
