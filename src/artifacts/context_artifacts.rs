//! Context generator planning and parallel execution (loctree/tsc-trace/tauri info).

use super::*;
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
#[derive(Debug, PartialEq, Eq)]
enum GateCoverage {
    /// A gate for this tool executed, or replayed a stored result, on this
    /// substrate. The signal exists; running the tool again buys nothing.
    Covered { origin: SubstrateKey },
    /// A gate for this tool was configured and deliberately ruled out — a preset
    /// that excludes it, a disabled flag, a tool the environment lacks. The
    /// missing signal is a decision, not a gap.
    RuledOut { reason: String },
    /// This run holds no gate for the tool at all, so nothing was decided about
    /// it and the context artifact is the only place its signal can come from.
    Uncovered,
}

fn gate_coverage(ledger: &TaskLedger, check_name: &str, substrate: &SubstrateKey) -> GateCoverage {
    let Some(entry) = ledger.lookup_tool(check_name, substrate) else {
        return GateCoverage::Uncovered;
    };
    match entry.state {
        // A gate that executed paid the cost already, whatever it concluded:
        // a failing or erroring run is still a run, and repeating it here would
        // buy the same answer twice.
        TaskState::Run { .. } => GateCoverage::Covered {
            origin: entry.key.substrate,
        },
        TaskState::Cached { origin, .. } => GateCoverage::Covered { origin },
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
    let state = match gate_coverage(ledger, check_name, substrate) {
        GateCoverage::Covered { origin } => TaskState::Cached {
            cache_age_secs: None,
            origin,
        },
        GateCoverage::RuledOut { reason } if runnable => TaskState::Skipped { reason },
        GateCoverage::RuledOut { reason } => TaskState::NotApplicable { reason },
        GateCoverage::Uncovered if runnable => return ContextToolPlan::Run,
        GateCoverage::Uncovered => TaskState::NotApplicable {
            reason: format!(
                "no {check_name} gate in this run and no runnable tool in the reviewed tree"
            ),
        },
    };

    let reason = match &state {
        TaskState::Cached { .. } => {
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
/// accounted for by `ContextCommandTiming`, which knows how long it took; the
/// planner does not, and a `Run` entry carrying a duration it invented would be
/// worse than no entry at all.
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
            && let GateCoverage::Covered { origin } =
                gate_coverage(ledger, "TypeScript", &substrate)
        {
            record_context_decision(
                ledger,
                "TypeScript",
                &substrate,
                TaskState::Cached {
                    cache_age_secs: None,
                    origin,
                },
            );
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
    pub(super) cmd: String,
    pub(super) args: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) out_dir: PathBuf,
    pub(super) out_file: String,
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
    Ok(run_context_cmds_parallel(
        &cmds,
        CONTEXT_GEN_TIMEOUT_SECS,
        emit_human_stdout,
    ))
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
            cmd: "cargo".into(),
            args: vec!["tree".into(), "--depth".into(), "2".into()],
            cwd: cwd.clone(),
            out_dir: ctx.clone(),
            out_file: "cargo-tree.txt".into(),
        });

        cmds.push(ContextCmd {
            label: "cargo sbom".into(),
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
        if !matches!(
            gate_coverage(
                ledger,
                "Vitest",
                &context_substrate("Vitest", &scan_root, &config.repo_root),
            ),
            GateCoverage::Covered { .. }
        ) {
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

/// Spawn all context commands in parallel and poll them with a shared timeout.
/// Each command gets `timeout_secs` from its own spawn time. Results are written
/// to the specified output files. Commands that exceed the timeout are killed.
pub(super) fn run_context_cmds_parallel(
    cmds: &[ContextCmd],
    timeout_secs: u64,
    emit: bool,
) -> Vec<ContextCommandTiming> {
    use std::time::Duration;

    struct RunningCmd {
        label: String,
        child: std::process::Child,
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

    for cmd in cmds {
        let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
        let idx = running.len();
        let stdout_path = cmd.out_dir.join(format!(".context-cmd-{idx}.stdout.tmp"));
        let stderr_path = cmd.out_dir.join(format!(".context-cmd-{idx}.stderr.tmp"));
        let stdout_file = match File::create(&stdout_path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let stderr_file = match File::create(&stderr_path) {
            Ok(file) => file,
            Err(_) => {
                let _ = fs::remove_file(&stdout_path);
                continue;
            }
        };

        match Command::new(&cmd.cmd)
            .args(&args)
            .current_dir(&cmd.cwd)
            // Context tools must never read the operator's terminal: an
            // interactive prompt (npx install, credential ask) with stdout
            // redirected to a file is invisible and steals keystrokes.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(stdout_file))
            .stderr(std::process::Stdio::from(stderr_file))
            .spawn()
        {
            Ok(child) => {
                let started_at = Instant::now();
                running.push(RunningCmd {
                    label: cmd.label.clone(),
                    child,
                    started_at,
                    deadline: started_at + Duration::from_secs(timeout_secs),
                    out_dir: cmd.out_dir.clone(),
                    out_file: cmd.out_file.clone(),
                    stdout_path,
                    stderr_path,
                    done: false,
                });
            }
            Err(_) => {
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&stderr_path);
                // Command not available: record it instead of skipping
                // silently, so the timings tell the truth about the pack.
                timings.push(ContextCommandTiming {
                    label: cmd.label.clone(),
                    artifact: None,
                    status: "spawn_failed",
                    duration_secs: 0.0,
                });
            }
        }
    }

    let poll_interval = Duration::from_millis(200);

    // Poll all until done or timed out
    while running.iter().any(|r| !r.done) {
        for r in running.iter_mut().filter(|r| !r.done) {
            match r.child.try_wait() {
                Ok(Some(exit)) => {
                    r.done = true;
                    // Collect output
                    collect_cmd_output(&r.stdout_path, &r.stderr_path, &r.out_dir, &r.out_file);
                    timings.push(ContextCommandTiming {
                        label: r.label.clone(),
                        artifact: (!r.out_file.is_empty()).then(|| {
                            Path::new("30_context")
                                .join(&r.out_file)
                                .display()
                                .to_string()
                        }),
                        status: if exit.success() {
                            "completed"
                        } else {
                            "failed"
                        },
                        duration_secs: r.started_at.elapsed().as_secs_f32(),
                    });
                }
                Ok(None) => {
                    if Instant::now() >= r.deadline {
                        let _ = r.child.kill();
                        let _ = r.child.wait();
                        r.done = true;
                        timings.push(ContextCommandTiming {
                            label: r.label.clone(),
                            artifact: (!r.out_file.is_empty()).then(|| {
                                Path::new("30_context")
                                    .join(&r.out_file)
                                    .display()
                                    .to_string()
                            }),
                            status: "timed_out",
                            duration_secs: r.started_at.elapsed().as_secs_f32(),
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
                Err(_) => {
                    r.done = true;
                    timings.push(ContextCommandTiming {
                        label: r.label.clone(),
                        artifact: (!r.out_file.is_empty()).then(|| {
                            Path::new("30_context")
                                .join(&r.out_file)
                                .display()
                                .to_string()
                        }),
                        status: "error",
                        duration_secs: r.started_at.elapsed().as_secs_f32(),
                    });
                }
            }
        }

        if running.iter().any(|r| !r.done) {
            std::thread::sleep(poll_interval);
        }
    }

    timings.sort_by(|a, b| b.duration_secs.total_cmp(&a.duration_secs));
    timings
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
        ContextCmd, context_substrate, js_exec_cmd, plan_context_cmds, plan_tsc_trace_artifact,
        tauri_info_cmd,
    };
    use crate::checks::{CheckProvenance, CheckResult, CheckStatus};
    use crate::config::{Config, test_config, test_js_profile};
    use crate::git::cmd::git_cmd;
    use crate::ledger::{SubstrateKey, TaskEntry, TaskKey, TaskKind, TaskLedger, TaskState};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

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
    /// eligibility skip lands in the first pass under an unknown substrate,
    /// an execution lands under the substrate its own provenance named.
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
            TaskState::Cached {
                cache_age_secs: None,
                origin: substrate,
            },
            "the artifact replays the gate's execution, and names whose",
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
            TaskState::Cached {
                cache_age_secs: None,
                origin: substrate,
            },
            "the deduped artifact must name the execution it stands on",
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
}
