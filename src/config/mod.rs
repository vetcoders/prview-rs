//! Configuration management
//!
//! Handles CLI args → Config conversion and project detection.

pub mod manifest;
pub use manifest::*;

use crate::cli::{Cli, ExecutionMode, PolicyModeArg, Profile};
use crate::git::{git_cmd, short_sha};
use crate::policy::{PolicyConfig, PolicyMode, load_policy, resolve_policy_path};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runtime configuration derived from CLI and environment
#[derive(Debug, Clone)]
pub struct Config {
    pub repo_root: PathBuf,
    pub target: Option<String>,
    pub bases: Vec<String>,
    pub profile: DetectedProfile,

    // Modes
    pub execution_mode: ExecutionMode,
    pub update_mode: bool,
    pub remote_mode: bool,
    pub current_only: bool,
    pub tui_mode: bool,

    // Step flags
    pub run_tests: bool,
    pub run_lint: bool,
    pub lint_forced: bool,
    pub run_bundle: bool,
    pub run_security: bool,
    pub run_heuristics: bool,
    /// Opt-in to the full security tier (`cargo geiger`). When false, geiger is
    /// simply not part of the profile — cleanly absent, not a skipped caveat.
    pub security_full: bool,

    // Fetch
    pub do_fetch: bool,
    pub local_only: bool,
    pub remote_only: bool,

    // Cache
    pub use_cache: bool,

    // Output
    pub quiet: bool,
    pub json: bool,
    pub create_zip: bool,
    pub create_dashboard: bool,
    pub soft_exit: bool,

    // Paths (optional overrides)
    pub repo_path: Option<PathBuf>,
    pub output_dir: Option<PathBuf>,

    // Misc
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub pr_head_oid: Option<String>,
    pub pr_base_oid: Option<String>,
    pub gh_repo: Option<String>,
    pub tests_pattern: Option<String>,
    pub why_blocked: bool,

    // Policy
    pub policy_file: PathBuf,
    pub policy: PolicyConfig,

    // Bridge rollout metadata (bash->rust migration)
    pub bridge_stage: u8,
    pub lint_ignore_patterns: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct StepFlags {
    run_tests: bool,
    run_lint: bool,
    lint_forced: bool,
    run_bundle: bool,
    run_security: bool,
    run_heuristics: bool,
}

impl StepFlags {
    fn from_cli(
        cli: &Cli,
        run_tests: bool,
        run_lint: bool,
        run_bundle: bool,
        run_security: bool,
    ) -> Self {
        Self {
            run_tests,
            run_lint,
            lint_forced: cli.with_lint,
            run_bundle,
            run_security,
            run_heuristics: cli.should_run_heuristics(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FetchConfig {
    do_fetch: bool,
    local_only: bool,
    remote_only: bool,
}

impl FetchConfig {
    #[cfg(test)]
    fn default_enabled() -> Self {
        Self {
            do_fetch: true,
            local_only: false,
            remote_only: false,
        }
    }

    fn disabled() -> Self {
        Self {
            do_fetch: false,
            local_only: false,
            remote_only: false,
        }
    }

    fn from_cli(cli: &Cli) -> Self {
        Self {
            do_fetch: !cli.no_fetch,
            local_only: cli.local_only,
            remote_only: cli.remote_only || cli.uses_fast_remote_only_standard_preset(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OutputConfig {
    quiet: bool,
    json: bool,
    create_zip: bool,
    create_dashboard: bool,
    soft_exit: bool,
}

impl OutputConfig {
    #[cfg(test)]
    fn standard() -> Self {
        Self {
            quiet: false,
            json: false,
            create_zip: true,
            create_dashboard: true,
            soft_exit: false,
        }
    }

    fn disabled() -> Self {
        Self {
            quiet: false,
            json: false,
            create_zip: false,
            create_dashboard: false,
            soft_exit: false,
        }
    }

    fn from_cli(cli: &Cli) -> Self {
        Self {
            quiet: cli.quiet,
            json: cli.json,
            create_zip: !cli.no_zip,
            create_dashboard: !cli.no_dashboard,
            soft_exit: cli.soft_exit,
        }
    }
}

/// Detected project profile with paths
#[derive(Debug, Clone)]
pub struct DetectedProfile {
    pub kind: ProfileKind,
    pub has_package_json: bool,
    pub has_tsconfig: bool,
    pub has_cargo: bool,
    pub has_pyproject: bool,
    /// True if Python appears to be runtime/source code, not only tooling
    /// config or fixtures.
    pub has_python_source: bool,
    /// True if there are actual JS/TS source files (not just tooling)
    pub has_js_source: bool,
    pub cargo_root: Option<PathBuf>,
    pub rust_dirs: Vec<PathBuf>,
    pub is_workspace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    Js,
    Rust,
    Python,
    Mixed,
    Generic,
}

impl ProfileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Js => "Js",
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::Mixed => "Mixed",
            Self::Generic => "Generic",
        }
    }
}

impl DetectedProfile {
    /// Whether Python checks (ruff/mypy/pytest) should run.
    ///
    /// Alongside Rust or JS project markers, Python counts as a real component
    /// only when it is *declared* via `pyproject.toml` — not merely because some
    /// `.py` scripts/fixtures exist. Otherwise a Rust/JS workspace with a few
    /// helper scripts wrongly runs mypy/ruff/pytest, which adds noise and crowds
    /// out the repo's native checks (PV-05).
    pub fn runs_python_checks(&self) -> bool {
        match self.kind {
            ProfileKind::Python => self.has_python_source || self.has_pyproject,
            ProfileKind::Js | ProfileKind::Rust | ProfileKind::Generic => false,
            ProfileKind::Mixed => {
                // A pyproject.toml is an explicit Python project declaration, so
                // it is both REQUIRED and SUFFICIENT to run Python checks in a
                // mixed repo. A repo declaring only pyproject (no .py source yet)
                // must not lose Ruff/Mypy/Pytest (PR #12 review #11). Conversely,
                // stray .py scripts WITHOUT a pyproject are incidental — not a
                // declared project — so they still do not pull in Python checks
                // (PV-05); source alone is deliberately insufficient here.
                self.has_pyproject
            }
        }
    }

    /// Get list of check names applicable to this profile
    pub fn get_check_names(&self) -> Vec<&'static str> {
        match self.kind {
            ProfileKind::Js => vec!["TypeScript", "ESLint", "Stylelint", "Vitest"],
            ProfileKind::Rust => vec!["Cargo check", "Clippy", "Cargo test"],
            ProfileKind::Python => vec!["Ruff", "Mypy", "Pytest"],
            ProfileKind::Mixed => {
                let mut checks = Vec::new();
                if self.has_package_json || self.has_tsconfig {
                    checks.extend(["TypeScript", "ESLint", "Stylelint", "Vitest"]);
                }
                if self.has_cargo {
                    checks.extend(["Cargo check", "Clippy", "Cargo test"]);
                }
                if self.runs_python_checks() {
                    checks.extend(["Ruff", "Mypy", "Pytest"]);
                }
                checks
            }
            ProfileKind::Generic => vec!["Loctree"],
        }
    }
}

#[cfg(test)]
pub(crate) fn test_generic_profile() -> DetectedProfile {
    DetectedProfile {
        kind: ProfileKind::Generic,
        has_package_json: false,
        has_tsconfig: false,
        has_cargo: false,
        has_pyproject: false,
        has_python_source: false,
        has_js_source: false,
        cargo_root: None,
        rust_dirs: vec![],
        is_workspace: false,
    }
}

#[cfg(test)]
pub(crate) fn test_rust_profile(has_cargo: bool) -> DetectedProfile {
    DetectedProfile {
        kind: ProfileKind::Rust,
        has_package_json: false,
        has_tsconfig: false,
        has_cargo,
        has_pyproject: false,
        has_python_source: false,
        has_js_source: false,
        cargo_root: has_cargo.then(|| PathBuf::from(".")),
        rust_dirs: if has_cargo {
            vec![PathBuf::from(".")]
        } else {
            vec![]
        },
        is_workspace: false,
    }
}

#[cfg(test)]
pub(crate) fn test_python_profile(has_pyproject: bool) -> DetectedProfile {
    DetectedProfile {
        kind: ProfileKind::Python,
        has_package_json: false,
        has_tsconfig: false,
        has_cargo: false,
        has_pyproject,
        has_python_source: has_pyproject,
        has_js_source: false,
        cargo_root: None,
        rust_dirs: vec![],
        is_workspace: false,
    }
}

#[cfg(test)]
pub(crate) fn test_js_profile(has_tsconfig: bool) -> DetectedProfile {
    DetectedProfile {
        kind: ProfileKind::Js,
        has_package_json: true,
        has_tsconfig,
        has_cargo: false,
        has_pyproject: false,
        has_python_source: false,
        has_js_source: true,
        cargo_root: None,
        rust_dirs: vec![],
        is_workspace: false,
    }
}

#[cfg(test)]
pub fn test_config() -> Config {
    Config::base(
        PathBuf::from("."),
        test_generic_profile(),
        PathBuf::from(".prview-policy.yml"),
        PolicyConfig::default(),
        None,
    )
    .with_fetch_config(FetchConfig::default_enabled())
    .with_output_config(OutputConfig::standard())
}

#[cfg(test)]
pub(crate) fn test_config_builder() -> TestConfigBuilder {
    TestConfigBuilder::new()
}

#[cfg(test)]
pub(crate) struct TestConfigBuilder {
    config: Config,
}

#[cfg(test)]
impl TestConfigBuilder {
    fn new() -> Self {
        Self {
            config: test_config(),
        }
    }

    pub(crate) fn repo_root(mut self, repo_root: impl Into<PathBuf>) -> Self {
        self.config.repo_root = repo_root.into();
        self
    }

    pub(crate) fn target(mut self, target: Option<&str>) -> Self {
        self.config.target = target.map(str::to_string);
        self
    }

    pub(crate) fn bases(mut self, bases: &[&str]) -> Self {
        self.config.bases = bases.iter().map(|base| (*base).to_string()).collect();
        self
    }

    pub(crate) fn profile(mut self, profile: DetectedProfile) -> Self {
        self.config.profile = profile;
        self
    }

    pub(crate) fn execution_mode(mut self, execution_mode: ExecutionMode) -> Self {
        self.config.execution_mode = execution_mode;
        self
    }

    pub(crate) fn run_lint(mut self, run_lint: bool) -> Self {
        self.config.run_lint = run_lint;
        self
    }

    pub(crate) fn run_tests(mut self, run_tests: bool) -> Self {
        self.config.run_tests = run_tests;
        self
    }

    pub(crate) fn security_full(mut self, security_full: bool) -> Self {
        self.config.security_full = security_full;
        self
    }

    pub(crate) fn do_fetch(mut self, do_fetch: bool) -> Self {
        self.config.do_fetch = do_fetch;
        self
    }

    pub(crate) fn use_cache(mut self, use_cache: bool) -> Self {
        self.config.use_cache = use_cache;
        self
    }

    pub(crate) fn create_zip(mut self, create_zip: bool) -> Self {
        self.config.create_zip = create_zip;
        self
    }

    pub(crate) fn policy(mut self, policy: PolicyConfig) -> Self {
        self.config.policy = policy;
        self
    }

    pub(crate) fn build(self) -> Config {
        self.config
    }
}

/// Determine prview home directory.
///
/// Priority: `PRVIEW_HOME` env > `~/.prview`
pub fn prview_home() -> PathBuf {
    std::env::var("PRVIEW_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".prview")
        })
}

fn current_run_stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

pub(crate) fn short_head(repo: &Path) -> String {
    git_cmd()
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn allocate_run_dir(
    repo_name: &str,
    branch_key: &str,
    commit: &str,
) -> Result<(PathBuf, String)> {
    let repo_runs_root = prview_home().join("runs").join(repo_name);
    let stamp = current_run_stamp();
    allocate_run_dir_in(&repo_runs_root, branch_key, &stamp, Some(commit))
}

fn run_id_base(stamp: &str, commit: Option<&str>) -> String {
    match commit.map(str::trim).filter(|value| !value.is_empty()) {
        Some(commit) => format!("{stamp}-{}", short_sha(commit)),
        None => stamp.to_string(),
    }
}

fn run_dir_path_in(
    repo_runs_root: &Path,
    branch_key: &str,
    stamp: &str,
    commit: Option<&str>,
) -> PathBuf {
    let id_base = run_id_base(stamp, commit);
    let mut run_id = id_base.clone();
    let mut suffix = 2u32;
    while run_id_taken_in_repo(repo_runs_root, &run_id) {
        run_id = format!("{id_base}-{suffix}");
        suffix += 1;
    }
    repo_runs_root.join(branch_key).join(run_id)
}

/// Exclusive, race-free allocation of a run directory under `runs/<repo>`.
pub(crate) fn allocate_run_dir_in(
    repo_runs_root: &Path,
    branch_key: &str,
    stamp: &str,
    commit: Option<&str>,
) -> Result<(PathBuf, String)> {
    allocate_run_dir_in_with_post_create(repo_runs_root, branch_key, stamp, commit, |_| Ok(()))
}

fn allocate_run_dir_in_with_post_create(
    repo_runs_root: &Path,
    branch_key: &str,
    stamp: &str,
    commit: Option<&str>,
    mut post_create: impl FnMut(&str) -> Result<()>,
) -> Result<(PathBuf, String)> {
    let base = repo_runs_root.join(branch_key);
    std::fs::create_dir_all(&base)
        .with_context(|| format!("failed to create runs dir {}", base.display()))?;

    let id_base = run_id_base(stamp, commit);
    let mut run_id = id_base.clone();
    let mut suffix = 2u32;
    loop {
        if !run_id_taken_in_repo(repo_runs_root, &run_id) {
            let run_dir = base.join(&run_id);
            match std::fs::create_dir(&run_dir) {
                Ok(()) => {
                    post_create(&run_id)?;
                    if run_id_taken_in_other_branch(repo_runs_root, branch_key, &run_id) {
                        std::fs::remove_dir(&run_dir).with_context(|| {
                            format!(
                                "failed to remove duplicate fresh run dir {}",
                                run_dir.display()
                            )
                        })?;
                    } else {
                        return Ok((run_dir, run_id));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => bail!("failed to create run dir {}: {e}", run_dir.display()),
            }
        }
        run_id = format!("{id_base}-{suffix}");
        suffix += 1;
        if suffix > 10_000 {
            bail!("exhausted run-id suffixes allocating a run directory");
        }
    }
}

fn run_id_taken_in_other_branch(repo_runs_root: &Path, branch_key: &str, run_id: &str) -> bool {
    let Ok(read) = std::fs::read_dir(repo_runs_root) else {
        return false;
    };
    for branch in read.flatten() {
        let bp = branch.path();
        if branch.file_name().to_string_lossy() == branch_key {
            continue;
        }
        if bp.is_dir() && bp.join(run_id).is_dir() {
            return true;
        }
    }
    false
}

/// True when `run_id` already names a directory under any `runs/<repo>/<branch>`
/// subtree.
fn run_id_taken_in_repo(repo_runs_root: &Path, run_id: &str) -> bool {
    let Ok(read) = std::fs::read_dir(repo_runs_root) else {
        return false;
    };
    for branch in read.flatten() {
        let bp = branch.path();
        if bp.is_dir() && bp.join(run_id).exists() {
            return true;
        }
    }
    false
}

impl Config {
    pub(crate) fn base(
        repo_root: PathBuf,
        profile: DetectedProfile,
        policy_file: PathBuf,
        policy: PolicyConfig,
        manifest: Option<PrviewManifest>,
    ) -> Self {
        let lint_ignore_patterns = manifest
            .and_then(|m| m.lint.ignore_patterns)
            .unwrap_or_default();

        Self {
            repo_root,
            target: None,
            bases: vec![],
            profile,
            execution_mode: ExecutionMode::Standard,
            update_mode: false,
            remote_mode: false,
            current_only: false,
            tui_mode: false,
            run_tests: false,
            run_lint: false,
            lint_forced: false,
            run_bundle: false,
            run_security: false,
            run_heuristics: false,
            security_full: false,
            do_fetch: false,
            local_only: false,
            remote_only: false,
            use_cache: true,
            quiet: false,
            json: false,
            create_zip: false,
            create_dashboard: false,
            soft_exit: false,
            repo_path: None,
            output_dir: None,
            pr_number: None,
            pr_url: None,
            pr_head_oid: None,
            pr_base_oid: None,
            gh_repo: None,
            tests_pattern: None,
            why_blocked: false,
            policy_file,
            policy,
            bridge_stage: 0,
            lint_ignore_patterns,
        }
    }

    fn with_step_flags(mut self, flags: StepFlags) -> Self {
        self.run_tests = flags.run_tests;
        self.run_lint = flags.run_lint;
        self.lint_forced = flags.lint_forced;
        self.run_bundle = flags.run_bundle;
        self.run_security = flags.run_security;
        self.run_heuristics = flags.run_heuristics;
        self
    }

    fn with_fetch_config(mut self, fetch: FetchConfig) -> Self {
        self.do_fetch = fetch.do_fetch;
        self.local_only = fetch.local_only;
        self.remote_only = fetch.remote_only;
        self
    }

    fn with_output_config(mut self, output: OutputConfig) -> Self {
        self.quiet = output.quiet;
        self.json = output.json;
        self.create_zip = output.create_zip;
        self.create_dashboard = output.create_dashboard;
        self.soft_exit = output.soft_exit;
        self
    }

    /// Create Config from CLI arguments
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let repo_root = find_repo_root()?;
        let manifest = PrviewManifest::load_from(&repo_root);

        let profile = detect_profile(&repo_root, cli.profile, manifest.as_ref())?;
        let policy_file = resolve_policy_path(&repo_root, cli.policy_file.as_deref())?;
        let policy_mode_override = cli.policy_mode.map(|m| match m {
            PolicyModeArg::Shadow => PolicyMode::Shadow,
            PolicyModeArg::Warn => PolicyMode::Warn,
            PolicyModeArg::Block => PolicyMode::Block,
        });
        let policy = load_policy(&policy_file, policy_mode_override)?;

        // Handle --pr mode: fetch branch from GitHub and apply presets
        let gh_repo = cli
            .gh_repo
            .clone()
            .or_else(|| infer_gh_repo_from_origin(&repo_root));

        let (
            target,
            bases,
            remote_mode,
            run_tests,
            run_lint,
            run_bundle,
            run_security,
            pr_url,
            pr_head_oid,
            pr_base_oid,
        ) = if let Some(pr_num) = cli.pr {
            use colored::Colorize;

            let pr_info = fetch_pr_info(pr_num, gh_repo.as_deref())?;
            if !cli.json && !cli.quiet {
                println!(
                    "{} PR #{} → {} → {}",
                    "ℹ".blue(),
                    pr_num,
                    pr_info.head.cyan(),
                    pr_info.base.green()
                );
            }

            let url = cli.pr_url.clone().or(pr_info.url);

            // PR mode presets:
            // - target = head branch from PR
            // - bases = [base branch from PR]
            // - remote_mode = true (use origin/)
            // - standard --pr uses the fast remote-only preset unless overridden
            // - run_tests/run_lint respect CLI flags (--quick disables them)
            // - run_bundle = false
            (
                Some(pr_info.head),
                vec![pr_info.base],
                true, // remote_mode
                cli.should_run_tests(),
                cli.should_run_lint(),
                false, // run_bundle
                cli.should_run_security(),
                url,
                pr_info.head_oid,
                pr_info.base_oid,
            )
        } else {
            (
                cli.target.clone(),
                cli.effective_bases(),
                cli.remote,
                cli.should_run_tests(),
                cli.should_run_lint(),
                cli.should_run_bundle(),
                cli.should_run_security(),
                cli.pr_url.clone(),
                None,
                None,
            )
        };

        let mut config = Self::base(repo_root, profile, policy_file, policy, manifest)
            .with_step_flags(StepFlags::from_cli(
                cli,
                run_tests,
                run_lint,
                run_bundle,
                run_security,
            ))
            .with_fetch_config(FetchConfig::from_cli(cli))
            .with_output_config(OutputConfig::from_cli(cli));

        config.target = target;
        config.bases = bases;
        config.security_full = cli.security_full;
        config.execution_mode = cli.execution_mode();
        config.update_mode = cli.update;
        config.remote_mode = remote_mode;
        config.current_only = cli.current_only;
        config.tui_mode = cli.tui;
        config.use_cache = !cli.no_cache;
        config.output_dir = cli.output_dir.clone();
        config.pr_number = cli.pr;
        config.pr_url = pr_url;
        config.pr_head_oid = pr_head_oid;
        config.pr_base_oid = pr_base_oid;
        config.gh_repo = gh_repo;
        config.tests_pattern = cli.tests_pattern.clone();
        config.why_blocked = cli.why_blocked;
        config.bridge_stage = cli.bridge_stage.min(4);

        Ok(config)
    }

    /// Create a minimal Config for state-only TUI viewer.
    /// Does not parse CLI args — avoids re-parsing issues.
    pub fn for_state_viewer(repo_root: &std::path::Path) -> Result<Self> {
        let manifest = PrviewManifest::load_from(repo_root);
        let profile = detect_profile(
            &repo_root.to_path_buf(),
            crate::cli::Profile::Auto,
            manifest.as_ref(),
        )?;
        let policy_file = resolve_policy_path(repo_root, None)?;
        let policy = load_policy(&policy_file, None)?;
        let mut config = Self::base(
            repo_root.to_path_buf(),
            profile,
            policy_file,
            policy,
            manifest,
        )
        .with_fetch_config(FetchConfig::disabled())
        .with_output_config(OutputConfig::disabled());
        config.tui_mode = true;
        config.use_cache = false;
        Ok(config)
    }

    /// Get safe target name for filesystem (/ → -)
    pub fn safe_target_name(&self) -> String {
        branch_storage_key(&self.target_ref_name())
    }

    pub fn is_fast_remote_only_standard(&self) -> bool {
        self.remote_only && matches!(self.execution_mode, ExecutionMode::Standard)
    }

    pub fn should_run_heavy_rust_lint(&self) -> bool {
        self.profile.has_cargo
            && self.run_lint
            && (!self.is_fast_remote_only_standard() || self.lint_forced)
    }

    pub fn requires_cargo_test_signal(&self) -> bool {
        self.profile.has_cargo
            && (self.run_tests
                || (matches!(
                    self.execution_mode,
                    ExecutionMode::Standard | ExecutionMode::Deep | ExecutionMode::Ci
                ) && !self.is_fast_remote_only_standard()))
    }

    pub fn requires_clippy_signal(&self) -> bool {
        self.profile.has_cargo
            && (self.should_run_heavy_rust_lint()
                || (matches!(
                    self.execution_mode,
                    ExecutionMode::Standard | ExecutionMode::Deep | ExecutionMode::Ci
                ) && !self.is_fast_remote_only_standard()))
    }

    /// Get the effective target ref name for the current run.
    ///
    /// When `--target` is omitted, normal branch workflows should still use the
    /// actual checked out branch instead of persisting a synthetic `HEAD` key.
    pub fn target_ref_name(&self) -> String {
        self.target
            .clone()
            .filter(|target| !target.trim().is_empty())
            .or_else(|| current_branch_name(&self.repo_root))
            .unwrap_or_else(|| "HEAD".to_string())
    }

    /// Get repository name from repo_root directory name
    pub fn repo_name(&self) -> String {
        repo_name_from_root(&self.repo_root)
    }

    /// Get artifacts output directory.
    ///
    /// Priority: `--output-dir` > `PRVIEW_HOME` > `~/.prview`
    pub fn artifacts_dir(&self) -> PathBuf {
        if let Some(ref dir) = self.output_dir {
            return dir.clone();
        }
        let repo_runs_root = prview_home().join("runs").join(self.repo_name());
        let stamp = current_run_stamp();
        let commit = self.run_id_commit_suffix();
        run_dir_path_in(
            &repo_runs_root,
            &self.safe_target_name(),
            &stamp,
            commit.as_deref(),
        )
    }

    /// Allocate the artifacts output directory for a real write path.
    ///
    /// `artifacts_dir()` remains a cheap predictor for callers that only need a
    /// path shape. Generation uses this allocator so same-second runs claim an
    /// exclusive repo-global id before writing.
    pub fn allocate_artifacts_dir(&self) -> Result<PathBuf> {
        if let Some(ref dir) = self.output_dir {
            return Ok(dir.clone());
        }
        self.allocate_artifacts_dir_with_stamp(&current_run_stamp())
    }

    pub(crate) fn allocate_artifacts_dir_for_commit(&self, commit: &str) -> Result<PathBuf> {
        if let Some(ref dir) = self.output_dir {
            return Ok(dir.clone());
        }
        self.allocate_artifacts_dir_in_home_with_stamp_and_commit(
            &prview_home(),
            &current_run_stamp(),
            Some(commit),
        )
    }

    fn allocate_artifacts_dir_with_stamp(&self, stamp: &str) -> Result<PathBuf> {
        self.allocate_artifacts_dir_in_home_with_stamp(&prview_home(), stamp)
    }

    fn allocate_artifacts_dir_in_home_with_stamp(
        &self,
        prview_home: &Path,
        stamp: &str,
    ) -> Result<PathBuf> {
        let commit = self.run_id_commit_suffix();
        self.allocate_artifacts_dir_in_home_with_stamp_and_commit(
            prview_home,
            stamp,
            commit.as_deref(),
        )
    }

    fn allocate_artifacts_dir_in_home_with_stamp_and_commit(
        &self,
        prview_home: &Path,
        stamp: &str,
        commit: Option<&str>,
    ) -> Result<PathBuf> {
        let repo_runs_root = prview_home.join("runs").join(self.repo_name());
        let (dir, _) =
            allocate_run_dir_in(&repo_runs_root, &self.safe_target_name(), stamp, commit)?;
        Ok(dir)
    }

    fn run_id_commit_suffix(&self) -> Option<String> {
        self.pr_head_oid
            .as_deref()
            .map(str::trim)
            .filter(|commit| !commit.is_empty())
            .map(|commit| short_sha(commit).to_string())
            .or_else(|| {
                let head = short_head(&self.repo_root);
                (!head.is_empty()).then_some(head)
            })
    }

    /// Get artifacts base directory (without timestamp) for history lookups.
    pub fn artifacts_base(&self) -> PathBuf {
        prview_home()
            .join("runs")
            .join(self.repo_name())
            .join(self.safe_target_name())
    }

    /// Get cache directory
    pub fn cache_dir(&self) -> PathBuf {
        prview_home()
            .join("cache")
            .join(cache_namespace_from_root(&self.repo_root))
    }
}

/// Find git repository root from current directory
fn find_repo_root() -> Result<PathBuf> {
    let current = std::env::current_dir()?;
    find_repo_root_from(&current)
}

/// Find git repository root from any path inside the repo.
pub fn find_repo_root_from(start: &Path) -> Result<PathBuf> {
    let mut path = start;

    loop {
        if path.join(".git").exists() {
            return Ok(path.to_path_buf());
        }
        path = path.parent().context("Not inside a git repository")?;
    }
}

/// Derive the repository directory name from a resolved repo root.
pub fn repo_name_from_root(repo_root: &Path) -> String {
    repo_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn cache_namespace_from_root(repo_root: &Path) -> String {
    infer_gh_repo_from_origin(repo_root)
        .and_then(|slug| slug.rsplit('/').next().map(str::to_string))
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| repo_name_from_root(repo_root))
        .to_ascii_lowercase()
}

/// Resolve the current git branch name when HEAD points at a branch.
pub fn current_branch_name(repo_root: &Path) -> Option<String> {
    let output = git_cmd()
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Encode a branch/ref into a filesystem-safe, collision-resistant storage key.
pub fn branch_storage_key(branch: &str) -> String {
    let mut encoded = String::with_capacity(branch.len());

    for byte in branch.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                encoded.push(char::from(*byte));
            }
            _ => encoded.push_str(&format!("%{:02X}", byte)),
        }
    }

    if encoded.is_empty() {
        "HEAD".to_string()
    } else {
        encoded
    }
}

/// Storage key for the current checkout, identical on the write and read paths.
///
/// A detached HEAD has no branch name, so `current_branch_name` returns `None`
/// and the key normalizes to the literal `HEAD`. Callers MUST derive the
/// storage key through this helper rather than off a display string like
/// `state.branch` (`HEAD (detached)`), otherwise a run recorded on a detached
/// commit becomes unfindable by a later lookup (PR #12 review).
pub fn storage_branch_key(repo_root: &Path) -> String {
    branch_storage_key(&current_branch_name(repo_root).unwrap_or_else(|| "HEAD".to_string()))
}

/// Detect project profile based on files present
fn detect_profile(
    repo_root: &PathBuf,
    requested: Profile,
    manifest: Option<&PrviewManifest>,
) -> Result<DetectedProfile> {
    let has_package_json = repo_root.join("package.json").exists();
    let has_tsconfig = repo_root.join("tsconfig.json").exists()
        || glob::glob(&format!("{}/**/tsconfig*.json", repo_root.display()))
            .map(|mut g| g.next().is_some())
            .unwrap_or(false);
    let has_pyproject = repo_root.join("pyproject.toml").exists();
    let has_python_source = detect_python_source(repo_root);

    // Detect Rust projects
    let (has_cargo, cargo_root, rust_dirs, is_workspace) = detect_rust_project(repo_root, manifest);

    // Detect actual JS/TS source files (not just tooling like package.json)
    let has_js_source = detect_js_source(repo_root);

    // Keep package.json as a lightweight signal so monorepos / app layouts
    // without root-level src discovery still get JS checks in auto mode.
    let has_js_project = has_js_source || has_tsconfig || has_package_json;

    let has_python_project = has_python_source || (has_pyproject && !has_cargo);

    let kind = match requested {
        Profile::Auto => {
            // Auto-detect based on what exists
            match (has_js_project, has_cargo, has_python_project) {
                (true, true, _) => ProfileKind::Mixed,
                (true, false, true) => ProfileKind::Mixed,
                (false, true, true) => ProfileKind::Mixed,
                (true, false, false) => ProfileKind::Js,
                (false, true, false) => ProfileKind::Rust,
                (false, false, true) => ProfileKind::Python,
                _ => ProfileKind::Generic,
            }
        }
        Profile::Js => ProfileKind::Js,
        Profile::Rust => ProfileKind::Rust,
        Profile::Python => ProfileKind::Python,
        Profile::Mixed => ProfileKind::Mixed,
        Profile::Generic => ProfileKind::Generic,
    };

    Ok(DetectedProfile {
        kind,
        has_package_json,
        has_tsconfig,
        has_cargo,
        has_pyproject,
        has_python_source,
        has_js_source,
        cargo_root,
        rust_dirs,
        is_workspace,
    })
}

/// Detect if there are actual JS/TS source files (not just tooling)
fn detect_js_source(repo_root: &Path) -> bool {
    // Check for JS/TS files in src/ directory
    let src_dir = repo_root.join("src");
    if src_dir.exists() {
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            let pattern = format!("{}/**/*.{}", src_dir.display(), ext);
            if glob::glob(&pattern)
                .map(|mut g| g.next().is_some())
                .unwrap_or(false)
            {
                return true;
            }
        }
    }

    // Also check root level for simple projects
    for ext in ["ts", "tsx", "js", "jsx"] {
        let pattern = format!("{}/*.{}", repo_root.display(), ext);
        if glob::glob(&pattern)
            .map(|mut g| {
                g.any(|p| {
                    // Exclude config files
                    p.as_ref()
                        .map(|path| {
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            !name.contains(".config.")
                                && !name.contains("eslint")
                                && !name.contains("prettier")
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
        {
            return true;
        }
    }

    false
}

fn detect_python_source(repo_root: &Path) -> bool {
    walkdir::WalkDir::new(repo_root)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git"
                    | ".venv"
                    | ".tox"
                    | "__pycache__"
                    | "node_modules"
                    | "target"
                    | "dist"
                    | "build"
                    | "fixtures"
                    | "fixture"
            )
        })
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .any(|entry| is_runtime_python_file(repo_root, entry.path()))
}

fn is_runtime_python_file(repo_root: &Path, path: &Path) -> bool {
    if path.extension().is_none_or(|ext| ext != "py") {
        return false;
    }

    let rel = path.strip_prefix(repo_root).unwrap_or(path);
    let parts: Vec<String> = rel
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();

    if parts.iter().any(|part| {
        matches!(
            part.as_str(),
            "fixtures"
                | "fixture"
                | "examples"
                | "docs"
                | "target"
                | "node_modules"
                | "__pycache__"
        )
    }) {
        return false;
    }

    match parts.as_slice() {
        [file] => !matches!(
            file.as_str(),
            "setup.py" | "conftest.py" | "noxfile.py" | "fabfile.py"
        ),
        [first, ..] => matches!(first.as_str(), "src" | "tests" | "test" | "python" | "py"),
        [] => false,
    }
}

/// Detect Rust project structure
fn detect_rust_project(
    repo_root: &PathBuf,
    manifest: Option<&PrviewManifest>,
) -> (bool, Option<PathBuf>, Vec<PathBuf>, bool) {
    let mut rust_dirs = Vec::new();
    let mut cargo_root = None;
    let mut is_workspace = false;

    if let Some(m) = manifest
        && let Some(ref croot) = m.project.cargo_root
    {
        let path = repo_root.join(croot);
        if path.join("Cargo.toml").exists() {
            cargo_root = Some(path.clone());
            rust_dirs.push(path.clone());
        }
    }

    // Check root Cargo.toml
    let root_cargo = repo_root.join("Cargo.toml");
    if root_cargo.exists() {
        if cargo_root.is_none() {
            cargo_root = Some(repo_root.clone());
        }

        // Check if it's a workspace
        if let Ok(content) = std::fs::read_to_string(&root_cargo) {
            is_workspace = content.contains("[workspace]");
        }

        if !rust_dirs.iter().any(|d| d == repo_root) {
            rust_dirs.push(repo_root.clone());
        }
    }

    let mut tried_heuristics = false;
    // Check common Rust subdirectories only if cargo root is not configured explicitly
    if cargo_root.is_none() {
        tried_heuristics = true;
        let rust_subdirs = ["src-tauri", "rust", "crates"];
        for subdir in rust_subdirs {
            let path = repo_root.join(subdir);
            if path.join("Cargo.toml").exists() {
                rust_dirs.push(path.clone());
                if cargo_root.is_none() {
                    cargo_root = Some(path);
                }
            }
        }
    }

    // Check for *_rs pattern
    if (tried_heuristics || cargo_root.is_none())
        && let Ok(entries) = std::fs::read_dir(repo_root)
    {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with("_rs") || name.ends_with("-rs") {
                let path = entry.path();
                if path.join("Cargo.toml").exists() {
                    if !rust_dirs.contains(&path) {
                        rust_dirs.push(path.clone());
                    }
                    if cargo_root.is_none() {
                        cargo_root = Some(path);
                    }
                }
            }
        }
    }

    let has_cargo = !rust_dirs.is_empty();

    (has_cargo, cargo_root, rust_dirs, is_workspace)
}

/// PR info returned from GitHub
struct PrInfo {
    head: String,
    base: String,
    head_oid: Option<String>,
    base_oid: Option<String>,
    url: Option<String>,
}

/// Fetch PR branch names (head and base) from GitHub using gh CLI
fn fetch_pr_info(pr_number: u64, gh_repo: Option<&str>) -> Result<PrInfo> {
    // Check if gh is available
    if Command::new("gh")
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .output()
        .is_err()
    {
        bail!("gh CLI is not installed (required for --pr mode)");
    }

    // Use `gh api` from a non-repo directory to avoid git-fetch noise on /dev/tty.
    // When gh runs inside a git repo, it triggers a background git fetch that writes
    // progress directly to /dev/tty (bypassing stdout/stderr pipes). Running from
    // /tmp prevents gh from detecting a git context.
    if let Some(repo) = gh_repo {
        let endpoint = format!("repos/{}/pulls/{}", repo, pr_number);
        let output = Command::new("gh")
            .args([
                "api",
                &endpoint,
                "--jq",
                "[.head.ref, .base.ref, .head.sha, .base.sha, .html_url] | @tsv",
            ])
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .output()
            .context("Failed to execute gh api")?;

        if output.status.success() {
            let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let parts: Vec<&str> = result.split('\t').collect();
            if parts.len() == 5 && !parts[0].is_empty() && !parts[1].is_empty() {
                return Ok(PrInfo {
                    head: parts[0].to_string(),
                    base: parts[1].to_string(),
                    head_oid: non_empty_string(parts[2]),
                    base_oid: non_empty_string(parts[3]),
                    url: Some(parts[4].to_string()),
                });
            }
        }
    }

    // Fallback: `gh pr view` (for repos where gh_repo is unknown)
    let mut args = vec![
        "pr".to_string(),
        "view".to_string(),
        pr_number.to_string(),
        "--json".to_string(),
        "headRefName,baseRefName,headRefOid,baseRefOid,url".to_string(),
    ];
    if let Some(repo) = gh_repo {
        args.push("--repo".to_string());
        args.push(repo.to_string());
    }

    let output = Command::new("gh")
        .args(&args)
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .output()
        .context("Failed to execute gh pr view")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Failed to get info for PR #{}: {}",
            pr_number,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_line = stdout
        .lines()
        .rfind(|line| {
            let trimmed = line.trim();
            trimmed.starts_with('{') && trimmed.ends_with('}')
        })
        .unwrap_or_else(|| stdout.trim());

    #[derive(Deserialize)]
    struct GhPrViewResponse {
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        #[serde(rename = "baseRefName")]
        base_ref_name: String,
        #[serde(rename = "headRefOid")]
        head_ref_oid: Option<String>,
        #[serde(rename = "baseRefOid")]
        base_ref_oid: Option<String>,
        url: Option<String>,
    }

    let parsed: GhPrViewResponse = serde_json::from_str(json_line).with_context(|| {
        format!(
            "Could not parse JSON PR info for PR #{} from gh output '{}'",
            pr_number, json_line
        )
    })?;

    if parsed.head_ref_name.is_empty() || parsed.base_ref_name.is_empty() {
        bail!(
            "Could not parse PR info for PR #{}: missing head/base refs",
            pr_number
        );
    }

    Ok(PrInfo {
        head: parsed.head_ref_name,
        base: parsed.base_ref_name,
        head_oid: parsed.head_ref_oid.filter(|oid| !oid.trim().is_empty()),
        base_oid: parsed.base_ref_oid.filter(|oid| !oid.trim().is_empty()),
        url: parsed.url,
    })
}

fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Infer GitHub owner/repo from origin remote URL
fn infer_gh_repo_from_origin(repo_root: &Path) -> Option<String> {
    let output = git_cmd()
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_github_owner_repo(&url)
}

/// Parse owner/repo from GitHub URL (SSH or HTTPS)
fn parse_github_owner_repo(url: &str) -> Option<String> {
    // git@github.com:Owner/Repo.git or https://github.com/Owner/Repo.git
    let re = regex::Regex::new(r"github\.com[:/]+([^/]+)/([^/]+?)(?:\.git)?$").ok()?;
    let caps = re.captures(url)?;
    let owner = caps.get(1)?.as_str();
    let repo = caps.get(2)?.as_str();
    if !owner.is_empty() && !repo.is_empty() {
        Some(format!("{}/{}", owner, repo))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn init_git_repo_with_branch(branch: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();

        let init_status = git_cmd()
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(init_status.success());

        let checkout_status = git_cmd()
            .args(["checkout", "-q", "-b", branch])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(checkout_status.success());

        tmp
    }

    fn init_git_repo_with_commit(branch: &str) -> tempfile::TempDir {
        let tmp = init_git_repo_with_branch(branch);
        for args in [
            &["config", "user.email", "t@t.co"][..],
            &["config", "user.name", "T"][..],
        ] {
            assert!(
                git_cmd()
                    .args(args)
                    .current_dir(tmp.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(tmp.path().join("f.txt"), "x\n").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(
            git_cmd()
                .args(["commit", "-q", "-m", "init"])
                .current_dir(tmp.path())
                .status()
                .unwrap()
                .success()
        );
        tmp
    }

    /// PR #12 review: a detached HEAD must produce the same storage key as the
    /// write path (`HEAD`), not one derived from a display string. Otherwise a
    /// run recorded on a detached commit is unfindable on read.
    #[test]
    fn storage_branch_key_normalizes_detached_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t.co"][..],
            &["config", "user.name", "T"][..],
        ] {
            assert!(
                git_cmd()
                    .args(args)
                    .current_dir(repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(repo.join("f.txt"), "x\n").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(repo)
            .status()
            .unwrap();
        git_cmd()
            .args(["commit", "-q", "-m", "init"])
            .current_dir(repo)
            .status()
            .unwrap();

        // On a branch: keyed by the branch name.
        assert_eq!(storage_branch_key(repo), branch_storage_key("main"));

        // Detach HEAD onto the commit, then the key must fall back to `HEAD`.
        let sha = String::from_utf8(
            git_cmd()
                .args(["rev-parse", "HEAD"])
                .current_dir(repo)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        git_cmd()
            .args(["checkout", "-q", sha.trim()])
            .current_dir(repo)
            .status()
            .unwrap();
        assert_eq!(storage_branch_key(repo), branch_storage_key("HEAD"));
        assert_eq!(storage_branch_key(repo), "HEAD");
    }

    fn make_test_config(repo_root: &str, target: Option<&str>) -> Config {
        test_config_builder()
            .repo_root(repo_root)
            .target(target)
            .build()
    }

    #[test]
    fn test_profile_kind_as_str() {
        assert_eq!(ProfileKind::Js.as_str(), "Js");
        assert_eq!(ProfileKind::Rust.as_str(), "Rust");
        assert_eq!(ProfileKind::Python.as_str(), "Python");
        assert_eq!(ProfileKind::Mixed.as_str(), "Mixed");
        assert_eq!(ProfileKind::Generic.as_str(), "Generic");
    }

    #[test]
    fn test_profile_kind_equality() {
        assert_eq!(ProfileKind::Js, ProfileKind::Js);
        assert_ne!(ProfileKind::Js, ProfileKind::Rust);
    }

    #[test]
    fn test_detected_profile_get_check_names_js() {
        let profile = DetectedProfile {
            kind: ProfileKind::Js,
            has_package_json: true,
            has_tsconfig: true,
            has_cargo: false,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: None,
            rust_dirs: vec![],
            is_workspace: false,
        };
        let checks = profile.get_check_names();
        assert!(checks.contains(&"TypeScript"));
        assert!(checks.contains(&"ESLint"));
        assert!(checks.contains(&"Stylelint"));
        assert!(checks.contains(&"Vitest"));
        assert!(!checks.contains(&"Bundle"));
    }

    #[test]
    fn test_detected_profile_get_check_names_rust() {
        let profile = DetectedProfile {
            kind: ProfileKind::Rust,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: true,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: Some(PathBuf::from(".")),
            rust_dirs: vec![PathBuf::from(".")],
            is_workspace: false,
        };
        let checks = profile.get_check_names();
        assert!(checks.contains(&"Cargo check"));
        assert!(checks.contains(&"Clippy"));
        assert!(checks.contains(&"Cargo test"));
    }

    #[test]
    fn test_detected_profile_get_check_names_python() {
        let profile = DetectedProfile {
            kind: ProfileKind::Python,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: false,
            has_pyproject: true,
            has_python_source: true,
            has_js_source: false,
            cargo_root: None,
            rust_dirs: vec![],
            is_workspace: false,
        };
        let checks = profile.get_check_names();
        assert!(checks.contains(&"Ruff"));
        assert!(checks.contains(&"Mypy"));
        assert!(checks.contains(&"Pytest"));
    }

    #[test]
    fn test_detected_profile_get_check_names_mixed() {
        let profile = DetectedProfile {
            kind: ProfileKind::Mixed,
            has_package_json: true,
            has_tsconfig: true,
            has_cargo: true,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: Some(PathBuf::from(".")),
            rust_dirs: vec![PathBuf::from(".")],
            is_workspace: false,
        };
        let checks = profile.get_check_names();
        assert!(checks.contains(&"TypeScript"));
        assert!(checks.contains(&"Cargo check"));
    }

    #[test]
    fn test_detected_profile_get_check_names_generic() {
        let profile = DetectedProfile {
            kind: ProfileKind::Generic,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: false,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: None,
            rust_dirs: vec![],
            is_workspace: false,
        };
        let checks = profile.get_check_names();
        assert!(checks.contains(&"Loctree"));
    }

    #[test]
    fn test_config_safe_target_name() {
        let config = test_config_builder()
            .target(Some("feature/my-branch"))
            .bases(&["main"])
            .build();

        assert_eq!(config.safe_target_name(), "feature%2Fmy-branch");
    }

    #[test]
    fn test_config_safe_target_name_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = make_test_config(tmp.path().to_str().unwrap(), None);
        assert_eq!(config.safe_target_name(), "HEAD");
    }

    #[test]
    fn test_config_cache_dir() {
        // cache_dir uses prview_home() which depends on env — just verify structure
        let config = make_test_config("/tmp/myrepo", None);
        let cache = config.cache_dir();
        // Should end with cache/<repo_name>
        assert!(cache.ends_with("cache/myrepo"));
    }

    #[test]
    fn test_config_cache_dir_prefers_origin_repo_name_for_worktree_like_clone() {
        let parent = tempfile::tempdir().unwrap();
        // Local dir name intentionally differs from the origin repo name, so the
        // assertion proves the cache key is derived from origin, not the worktree dir.
        let repo_root = parent.path().join("myapp-worktree");
        std::fs::create_dir(&repo_root).unwrap();

        let init_status = git_cmd()
            .args(["init", "-q"])
            .current_dir(&repo_root)
            .status()
            .unwrap();
        assert!(init_status.success());

        let remote_status = git_cmd()
            .args(["remote", "add", "origin", "git@github.com:owner/myapp.git"])
            .current_dir(&repo_root)
            .status()
            .unwrap();
        assert!(remote_status.success());

        let config = make_test_config(repo_root.to_str().unwrap(), None);
        assert!(config.cache_dir().ends_with("cache/myapp"));
    }

    #[test]
    fn test_config_repo_name() {
        let config = make_test_config("/tmp/myrepo", None);
        assert_eq!(config.repo_name(), "myrepo");

        let config2 = make_test_config("/home/user/projects/cool-app", None);
        assert_eq!(config2.repo_name(), "cool-app");
    }

    #[test]
    fn test_fast_remote_only_standard_helpers() {
        let mut config = test_config();
        config.profile = test_rust_profile(true);
        config.execution_mode = ExecutionMode::Standard;
        config.remote_only = true;
        config.run_lint = true;

        assert!(config.is_fast_remote_only_standard());
        assert!(!config.should_run_heavy_rust_lint());
        assert!(!config.requires_cargo_test_signal());
        assert!(!config.requires_clippy_signal());

        config.lint_forced = true;
        assert!(config.should_run_heavy_rust_lint());
        assert!(config.requires_clippy_signal());

        config.run_tests = true;
        assert!(config.requires_cargo_test_signal());
    }

    #[test]
    fn test_fetch_config_enables_fast_remote_only_for_standard_prs() {
        let cli = Cli::parse_from(["prview", "--pr", "42"]);
        let fetch = FetchConfig::from_cli(&cli);

        assert!(fetch.remote_only);
        assert!(!fetch.local_only);
    }

    #[test]
    fn test_fetch_config_keeps_deep_pr_out_of_fast_remote_only_preset() {
        let cli = Cli::parse_from(["prview", "--pr", "42", "--deep"]);
        let fetch = FetchConfig::from_cli(&cli);

        assert!(!fetch.remote_only);
    }

    #[test]
    fn test_config_artifacts_base() {
        let config = make_test_config("/tmp/myrepo", Some("feature/test"));
        let base = config.artifacts_base();
        assert!(base.ends_with("runs/myrepo/feature%2Ftest"));
    }

    #[test]
    fn test_profile_kind_serialization() {
        let js = ProfileKind::Js;
        let serialized = serde_json::to_string(&js).unwrap();
        assert_eq!(serialized, "\"js\"");

        let rust = ProfileKind::Rust;
        let serialized = serde_json::to_string(&rust).unwrap();
        assert_eq!(serialized, "\"rust\"");
    }

    #[test]
    fn test_profile_kind_deserialization() {
        let js: ProfileKind = serde_json::from_str("\"js\"").unwrap();
        assert_eq!(js, ProfileKind::Js);

        let rust: ProfileKind = serde_json::from_str("\"rust\"").unwrap();
        assert_eq!(rust, ProfileKind::Rust);
    }

    #[test]
    fn test_detected_profile_get_check_names_mixed_with_python() {
        let profile = DetectedProfile {
            kind: ProfileKind::Mixed,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: true,
            has_pyproject: true,
            has_python_source: true,
            has_js_source: false,
            cargo_root: Some(PathBuf::from(".")),
            rust_dirs: vec![PathBuf::from(".")],
            is_workspace: false,
        };
        let checks = profile.get_check_names();
        assert!(checks.contains(&"Cargo check"));
        assert!(checks.contains(&"Ruff"));
    }

    #[test]
    fn test_detect_profile_auto_keeps_rust_when_pyproject_is_tooling_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("cargo");
        std::fs::write(tmp.path().join("pyproject.toml"), "[tool.ruff]\n").expect("pyproject");
        let fixture_dir = tmp.path().join("tools/fixtures/python-threading");
        std::fs::create_dir_all(&fixture_dir).expect("fixtures");
        std::fs::write(fixture_dir.join("case.py"), "print('fixture')\n").expect("fixture");

        let profile =
            detect_profile(&tmp.path().to_path_buf(), Profile::Auto, None).expect("profile");

        assert_eq!(profile.kind, ProfileKind::Rust);
        assert!(profile.has_cargo);
        assert!(profile.has_pyproject);
        assert!(!profile.has_python_source);
        assert!(!profile.runs_python_checks());
    }

    #[test]
    fn test_rust_repo_with_root_py_script_but_no_pyproject_skips_python_checks() {
        // PV-05: a Rust repo with incidental .py scripts (and NO pyproject) must
        // not pull in mypy/ruff/pytest — they crowd out the native Rust signal.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("cargo");
        // Root-level helper script — detected as python source, but NOT a
        // declared Python project (no pyproject.toml).
        std::fs::write(tmp.path().join("release_helper.py"), "print('hi')\n").expect("py");

        let profile =
            detect_profile(&tmp.path().to_path_buf(), Profile::Auto, None).expect("profile");

        assert!(profile.has_cargo);
        assert!(
            profile.has_python_source,
            "a root-level .py must be detected as python source"
        );
        assert!(!profile.has_pyproject);
        assert!(
            !profile.runs_python_checks(),
            "Rust repo with stray .py scripts (no pyproject) must NOT run Python checks (PV-05)"
        );
    }

    #[test]
    fn test_detect_profile_auto_keeps_package_json_signal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("package.json"), "{\"name\":\"demo\"}").expect("package");

        let profile =
            detect_profile(&tmp.path().to_path_buf(), Profile::Auto, None).expect("profile");
        assert_eq!(profile.kind, ProfileKind::Js);
        assert!(profile.has_package_json);
    }

    #[test]
    fn test_profile_kind_copy() {
        let original = ProfileKind::Rust;
        let copied = original;
        assert_eq!(original, copied);
    }

    #[test]
    fn test_config_artifacts_dir() {
        let config = make_test_config("/tmp/myrepo", Some("feature/test"));
        let artifacts_dir = config.artifacts_dir();
        // Should contain runs/<repo>/<branch>/<run_id>
        let path_str = artifacts_dir.to_string_lossy();
        assert!(
            path_str.contains("runs/myrepo/feature%2Ftest/"),
            "path was: {}",
            path_str
        );
    }

    #[test]
    fn test_config_artifacts_dir_includes_short_head_suffix() {
        let repo = init_git_repo_with_commit("main");
        let config = make_test_config(repo.path().to_str().unwrap(), Some("main"));
        let artifacts_dir = config.artifacts_dir();
        let run_id = artifacts_dir.file_name().unwrap().to_string_lossy();
        let head = short_head(repo.path());

        assert!(
            run_id.contains(&format!("-{head}")),
            "run_id {run_id} should include short HEAD {head}"
        );
    }

    #[test]
    fn test_config_allocate_artifacts_dir_uses_resolved_target_suffix() {
        let repo = init_git_repo_with_commit("main");
        let target_commit = String::from_utf8(
            git_cmd()
                .args(["rev-parse", "HEAD"])
                .current_dir(repo.path())
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();

        std::fs::write(repo.path().join("f.txt"), "x\ny\n").unwrap();
        git_cmd()
            .args(["add", "."])
            .current_dir(repo.path())
            .status()
            .unwrap();
        assert!(
            git_cmd()
                .args(["commit", "-q", "-m", "local head"])
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let local_head = short_head(repo.path());
        assert_ne!(short_sha(&target_commit), local_head);

        let home = tempfile::tempdir().unwrap();
        let stamp = "20260704-121500";
        let config = make_test_config(repo.path().to_str().unwrap(), Some("main"));
        let artifacts_dir = config
            .allocate_artifacts_dir_in_home_with_stamp_and_commit(
                home.path(),
                stamp,
                Some(&target_commit),
            )
            .unwrap();
        let run_id = artifacts_dir.file_name().unwrap().to_string_lossy();

        assert_eq!(run_id, format!("{stamp}-{}", short_sha(&target_commit)));
        assert!(
            !run_id.ends_with(&local_head),
            "run_id {run_id} should not use local HEAD {local_head}"
        );
    }

    #[test]
    fn test_config_allocate_artifacts_dir_is_unique_across_branches_same_second() {
        let repo = init_git_repo_with_commit("main");
        let home = tempfile::tempdir().unwrap();
        let stamp = "20260704-120000";

        let main = make_test_config(repo.path().to_str().unwrap(), Some("main"));
        let feature = make_test_config(repo.path().to_str().unwrap(), Some("feature/test"));

        let main_dir = main
            .allocate_artifacts_dir_in_home_with_stamp(home.path(), stamp)
            .unwrap();
        let feature_dir = feature
            .allocate_artifacts_dir_in_home_with_stamp(home.path(), stamp)
            .unwrap();
        let main_id = main_dir.file_name().unwrap().to_string_lossy();
        let feature_id = feature_dir.file_name().unwrap().to_string_lossy();

        assert_ne!(main_id, feature_id);
        assert!(main_id.starts_with(stamp));
        assert!(feature_id.starts_with(stamp));
        assert!(main_dir.is_dir());
        assert!(feature_dir.is_dir());
    }

    #[test]
    fn test_allocate_run_dir_retries_when_duplicate_appears_after_create() {
        let home = tempfile::tempdir().unwrap();
        let repo_runs_root = home.path().join("runs").join("demo");
        let branch_key = "feature%2Ftest";
        let stamp = "20260704-123000";
        let commit = "abcdef1234567890abcdef1234567890abcdef12";
        let colliding_id = format!("{stamp}-{}", short_sha(commit));
        let injected_dir = repo_runs_root.join("main").join(&colliding_id);
        let mut injected = false;

        let (run_dir, run_id) = allocate_run_dir_in_with_post_create(
            &repo_runs_root,
            branch_key,
            stamp,
            Some(commit),
            |created_id| {
                if !injected {
                    assert_eq!(created_id, colliding_id);
                    std::fs::create_dir_all(injected_dir.parent().unwrap()).unwrap();
                    std::fs::create_dir(&injected_dir).unwrap();
                    injected = true;
                }
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(run_id, format!("{colliding_id}-2"));
        assert!(run_dir.is_dir());
        assert!(injected_dir.is_dir());
        assert!(!repo_runs_root.join(branch_key).join(colliding_id).exists());
    }

    #[test]
    fn test_config_artifacts_dir_output_dir_override() {
        let mut config = make_test_config("/tmp/myrepo", Some("feature/test"));
        config.output_dir = Some(PathBuf::from("/tmp/custom-output"));

        // --output-dir takes priority
        assert_eq!(config.artifacts_dir(), PathBuf::from("/tmp/custom-output"));
    }

    #[test]
    fn test_config_builder_applies_multiple_overrides() {
        let config = test_config_builder()
            .repo_root("/tmp/builder-repo")
            .target(Some("feature/builder"))
            .bases(&["main", "release"])
            .profile(test_rust_profile(true))
            .execution_mode(ExecutionMode::Ci)
            .run_lint(true)
            .run_tests(true)
            .do_fetch(false)
            .use_cache(false)
            .create_zip(false)
            .build();

        assert_eq!(config.repo_root, PathBuf::from("/tmp/builder-repo"));
        assert_eq!(config.target.as_deref(), Some("feature/builder"));
        assert_eq!(config.bases, vec!["main", "release"]);
        assert_eq!(config.profile.kind, ProfileKind::Rust);
        assert_eq!(config.execution_mode, ExecutionMode::Ci);
        assert!(config.run_lint);
        assert!(config.run_tests);
        assert!(!config.do_fetch);
        assert!(!config.use_cache);
        assert!(!config.create_zip);
    }

    #[test]
    fn test_detected_profile_mixed_no_checks() {
        // Mixed profile with no actual project files detected
        let profile = DetectedProfile {
            kind: ProfileKind::Mixed,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: false,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: None,
            rust_dirs: vec![],
            is_workspace: false,
        };
        let checks = profile.get_check_names();
        assert!(checks.is_empty());
    }

    #[test]
    fn test_detected_profile_clone() {
        let profile = DetectedProfile {
            kind: ProfileKind::Rust,
            has_package_json: false,
            has_tsconfig: false,
            has_cargo: true,
            has_pyproject: false,
            has_python_source: false,
            has_js_source: false,
            cargo_root: Some(PathBuf::from(".")),
            rust_dirs: vec![PathBuf::from(".")],
            is_workspace: true,
        };
        let cloned = profile.clone();
        assert_eq!(profile.kind, cloned.kind);
        assert_eq!(profile.is_workspace, cloned.is_workspace);
    }

    #[test]
    fn test_profile_kind_debug() {
        let kind = ProfileKind::Python;
        let debug_str = format!("{:?}", kind);
        assert_eq!(debug_str, "Python");
    }

    #[test]
    fn test_config_safe_target_multiple_slashes() {
        let config = test_config_builder()
            .target(Some("feature/user/auth/login"))
            .build();

        assert_eq!(config.safe_target_name(), "feature%2Fuser%2Fauth%2Flogin");
    }

    #[test]
    fn test_branch_storage_key_avoids_dash_slash_collisions() {
        assert_ne!(
            branch_storage_key("release/foo-bar"),
            branch_storage_key("release-foo/bar")
        );
        assert_eq!(branch_storage_key("release/foo-bar"), "release%2Ffoo-bar");
    }

    #[test]
    fn test_target_ref_name_uses_current_branch_when_target_is_omitted() {
        let repo = init_git_repo_with_branch("feature/user-auth");
        let config = make_test_config(repo.path().to_str().unwrap(), None);

        assert_eq!(config.target_ref_name(), "feature/user-auth");
        assert_eq!(config.safe_target_name(), "feature%2Fuser-auth");
    }

    #[test]
    fn test_find_repo_root_from_subdirectory() {
        let repo = init_git_repo_with_branch("main");
        let nested = repo.path().join("src/deep/module");
        std::fs::create_dir_all(&nested).unwrap();

        let root = find_repo_root_from(&nested).unwrap();
        assert_eq!(root, repo.path());
    }

    #[test]
    fn test_detect_rust_project_fallback_no_manifest() {
        let repo = tempfile::tempdir().expect("tempdir");
        let tauri_dir = repo.path().join("src-tauri");
        std::fs::create_dir_all(&tauri_dir).unwrap();
        std::fs::write(tauri_dir.join("Cargo.toml"), "").unwrap();

        let (has_cargo, cargo_root, rust_dirs, _) =
            detect_rust_project(&repo.path().to_path_buf(), None);
        assert!(has_cargo);
        assert_eq!(cargo_root, Some(tauri_dir.clone()));
        assert_eq!(*rust_dirs.first().unwrap(), tauri_dir);
    }

    #[test]
    fn test_detect_rust_project_manifest_overrides_cargo_root() {
        let repo = tempfile::tempdir().expect("tempdir");

        let custom_dir = repo.path().join("custom_backend");
        std::fs::create_dir_all(&custom_dir).unwrap();
        std::fs::write(custom_dir.join("Cargo.toml"), "").unwrap();

        let tauri_dir = repo.path().join("src-tauri");
        std::fs::create_dir_all(&tauri_dir).unwrap();
        std::fs::write(tauri_dir.join("Cargo.toml"), "").unwrap();

        let manifest = crate::config::manifest::PrviewManifest {
            project: crate::config::manifest::ProjectConfig {
                cargo_root: Some("custom_backend".to_string()),
            },
            lint: Default::default(),
        };

        let (has_cargo, cargo_root, rust_dirs, _) =
            detect_rust_project(&repo.path().to_path_buf(), Some(&manifest));
        assert!(has_cargo);
        assert_eq!(cargo_root, Some(custom_dir.clone()));
        assert_eq!(*rust_dirs.first().unwrap(), custom_dir);
    }

    #[test]
    fn test_load_manifest_malformed_toml_is_ignored_with_silent_fallback() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("prview.toml"), "invalid = [ toml").unwrap();

        let manifest = crate::config::manifest::PrviewManifest::load_from(repo.path());
        assert!(
            manifest.is_none(),
            "Malformed TOML should silently return None and trigger fallback"
        );
    }
}
