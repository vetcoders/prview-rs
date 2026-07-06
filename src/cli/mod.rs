//! CLI argument parsing using clap
//!
//! Maintains compatibility with the bash version's flags.

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// PR Review & Artifact Generator by vetcoders - cross-language PR analysis tool
///
/// Generates Artifact Pack v1: structured, verifiable PR review artifacts
/// including per-gate quality reports, provenance tracking, hard fail
/// detection, heuristics (via Loctree), and integrity validation (MANIFEST + SANITY checks).
#[derive(Parser, Debug, Clone)]
#[command(name = "prview")]
#[command(author = "vetcoders <hello@vetcoders.io>")]
#[command(version)]
#[command(about = "PR Review & Artifact Generator by vetcoders")]
#[command(args_conflicts_with_subcommands = true)]
#[command(subcommand_precedence_over_arg = true)]
#[command(
    long_about = "PR Review & Artifact Generator by vetcoders - cross-language PR analysis tool\n\n\
Generates Artifact Pack v1 with structured, verifiable PR review artifacts:\n\n\
  Artifact layout:\n\
    00_summary/   RUN.json, MANIFEST.json, SANITY.json, MERGE_GATE,\n\
                  ARTIFACT_VERSION.txt, system/git meta, failures summary\n\
    10_diff/      full.patch, per-commit-diffs/, per-file-diffs/\n\
    20_quality/   <gate>.result.json + <gate>.log per check,\n\
                  full-checks.log, breaking changes, coverage delta\n\
    30_context/   changed-tests.txt, optional INLINE_FINDINGS.sarif, tooling\n\
    review.html   Standard portable human review export\n\
    dashboard.html  Interactive HTML report (optional via --no-dashboard)\n\n\
  Trustworthiness:\n\
    - CheckProvenance: command, exit_code, cwd, timestamps per gate\n\
    - Hard fail signatures: Rust panics, Node rejections, Python tracebacks,\n\
      segfaults, sanitizers (live + cached results validated)\n\
    - MANIFEST.json: SHA256 hashes of all generated files\n\
    - SANITY.json: post-generation integrity checks (4 validators)\n\n\
  Supported profiles: Rust, JavaScript/TypeScript, Python, Mixed, Generic (with Loctree heuristics)"
)]
#[command(after_help = "Examples:
  prview                              Standard review (tests + lint, current branch vs develop/main)
  prview --quick                      Quick review (skip tests/lint/heuristics)
  prview --deep feature/x dev         Full review with all checks
  prview --skip-tests feature/x dev   Review without tests
  prview --no-dashboard               Skip HTML dashboard generation
  prview --pr 42                      Analyze GitHub PR #42
  prview --pr 42 --gh-repo owner/repo PR from specific repository
  prview --update feat/x main         Incremental update after new commits
  prview gate                         Quality gate with contractual exit codes
  prview gate --strict --json         Machine-readable gate verdict
  prview --json --quiet               Compact JSON summary for CI and agents
  prview --ci                         CI mode: all checks, strict exit codes
  prview runs                         List previous runs for the current repository
  prview runs --all                   List runs across all repositories
  prview open                         Open dashboard for the latest run
  prview open <run-id>                Open dashboard for a specific run
  prview open <run-id> --dir          Print the artifact directory path for a run
  prview completions zsh              Generate shell completions for zsh")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<CliCommand>,

    /// Target branch to analyze (default: current branch)
    #[arg(index = 1)]
    pub target: Option<String>,

    /// Base branches to diff against (default: develop main)
    #[arg(index = 2, num_args = 0..)]
    pub bases: Vec<String>,

    // === Presets ===
    /// Quick mode: skip tests, lint, bundle, and heuristics
    #[arg(long, conflicts_with_all = ["deep", "ci", "ai_only"])]
    pub quick: bool,

    /// Deep mode: enable all checks including security and heuristics
    #[arg(long, conflicts_with_all = ["quick", "ci", "ai_only"])]
    pub deep: bool,

    /// CI mode: all checks, no colors, strict non-zero exit on failure
    #[arg(long, conflicts_with_all = ["quick", "deep", "ai_only"])]
    pub ci: bool,

    /// AI-only mode: minimal checks, generate only the AI context pack
    #[arg(long = "ai-only", conflicts_with_all = ["quick", "deep", "ci"])]
    pub ai_only: bool,

    // === Step control ===
    /// Force tests on even in modes that disable them (--quick, --update, --ai-only, --remote-only)
    #[arg(long = "with-tests")]
    pub with_tests: bool,

    /// Skip test execution (tests run by default in standard/deep/ci modes)
    #[arg(long = "skip-tests", conflicts_with = "with_tests")]
    pub skip_tests: bool,

    /// Force linters on even in modes that disable them (--quick, --update, --ai-only)
    #[arg(long = "with-lint")]
    pub with_lint: bool,

    /// Skip linter execution (linters run by default in standard/deep/ci modes)
    #[arg(long = "skip-lint", conflicts_with = "with_lint")]
    pub skip_lint: bool,

    /// Enable bundle build (off by default, enabled by --ci)
    #[arg(long = "with-bundle")]
    pub with_bundle: bool,

    /// Skip bundle build even when otherwise enabled
    #[arg(long = "skip-bundle", conflicts_with = "with_bundle")]
    pub skip_bundle: bool,

    /// Enable the heavy security posture (only runs with --deep or --ci by default)
    #[arg(
        long = "with-security",
        long_help = "Enable the heavy security posture. This raises the security tier used by \
                     --deep and --ci. cargo-geiger is NOT part of this tier — it is opt-in via \
                     --security-full. Lightweight checks like cargo-audit always run regardless."
    )]
    pub with_security: bool,

    /// Skip heavy security checks even when otherwise enabled
    #[arg(long = "skip-security", conflicts_with = "with_security")]
    pub skip_security: bool,

    /// Run the full security tier including full-tree semgrep and cargo-geiger
    #[arg(
        long = "security-full",
        long_help = "Opt in to the full security tier. Without this flag semgrep is scoped to \
                     findings introduced after the merge-base when a clean git baseline is \
                     available. With this flag semgrep scans the full tree and cargo-geiger's \
                     unsafe-usage scan is added. Geiger can take several minutes on large \
                     dependency trees, so it is off by default even under --deep."
    )]
    pub security_full: bool,

    // === Profile ===
    /// Language profile for check selection (default: auto-detected from repo contents)
    #[arg(long, value_enum, default_value = "auto")]
    pub profile: Profile,

    // === Special modes ===
    /// Fetch and analyze a GitHub PR by number (e.g. --pr 42)
    #[arg(long)]
    pub pr: Option<u64>,

    /// GitHub repository for --pr mode as owner/repo (default: inferred from git remote origin)
    #[arg(long = "gh-repo", requires = "pr")]
    pub gh_repo: Option<String>,

    /// Analyze remote tracking branch (origin/...) without requiring a local checkout
    #[arg(short = 'R', long)]
    pub remote: bool,

    /// Incremental update: find the last run for this branch and regenerate only for new commits
    #[arg(long)]
    pub update: bool,

    /// Watch mode: monitor file changes and regenerate artifacts automatically [experimental]
    #[arg(long)]
    pub watch: bool,

    // === Fetch control ===
    /// Skip git fetch before analysis (use already-fetched refs)
    #[arg(long = "no-fetch")]
    pub no_fetch: bool,

    /// Resolve base branches from local refs only (no origin/ prefix)
    #[arg(long = "local-only", conflicts_with = "remote_only")]
    pub local_only: bool,

    /// Resolve base branches from origin/* only (skips tests/heuristics in standard mode)
    #[arg(long = "remote-only", conflicts_with = "local_only")]
    pub remote_only: bool,

    // === Cache ===
    /// Disable result caching; re-run all checks from scratch
    #[arg(long = "no-cache")]
    pub no_cache: bool,

    // === Output ===
    /// Suppress progress output; only print errors and final summary
    #[arg(short, long)]
    pub quiet: bool,

    /// Emit a compact JSON summary to stdout (useful for CI pipelines and agents)
    #[arg(long)]
    pub json: bool,

    /// Disable ANSI color codes in output
    #[arg(long = "no-color")]
    pub no_color: bool,

    // === Misc ===
    /// Skip architecture heuristics (loctree cycle/dead-code/twins detection)
    #[arg(long = "no-heuristics")]
    pub no_heuristics: bool,

    /// Skip ZIP archive creation of the artifact directory
    #[arg(long = "no-zip")]
    pub no_zip: bool,

    /// Skip interactive HTML dashboard generation
    #[arg(long = "no-dashboard")]
    pub no_dashboard: bool,

    /// Analyze only the current branch state without diffing against a base branch
    #[arg(long = "current-only")]
    pub current_only: bool,

    /// Always exit 0 regardless of check failures (useful for advisory-only CI jobs)
    #[arg(long = "soft-exit")]
    pub soft_exit: bool,

    /// Regex pattern to filter which tests to run (passed to the test runner, e.g. vitest --grep)
    #[arg(long = "tests-pattern", value_name = "REGEX")]
    pub tests_pattern: Option<String>,

    /// PR URL to embed in artifact metadata (e.g. for traceability in RUN.json)
    #[arg(long = "pr-url", value_name = "URL")]
    pub pr_url: Option<String>,

    /// Path to repo-local policy file (default: .prview-policy.yml at repo root)
    #[arg(long = "policy-file", value_name = "PATH")]
    pub policy_file: Option<PathBuf>,

    /// Override policy rollout mode: shadow (log only), warn (non-blocking), block (fail on violation)
    #[arg(long = "policy-mode", value_enum)]
    pub policy_mode: Option<PolicyModeArg>,

    /// Internal: modular bridge stage 0-4 for bash-to-rust migration rollout
    #[arg(long = "bridge-stage", default_value_t = 0, hide = true)]
    pub bridge_stage: u8,

    /// Run in interactive TUI mode instead of streaming text output
    #[arg(long)]
    pub tui: bool,

    /// Override the artifacts output directory (default: ~/.prview/runs/<repo>/<branch>/<run_id>/)
    #[arg(long = "output-dir", value_name = "PATH")]
    pub output_dir: Option<PathBuf>,

    /// Print shell alias setup instructions and exit
    #[arg(long = "shell-setup")]
    pub shell_setup: bool,

    /// Explain why the merge gate is blocking (human-readable)
    #[arg(long = "why-blocked")]
    pub why_blocked: bool,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    /// Run the quality gate and exit with contractual status codes
    Gate(GateArgs),
    /// Inspect repository state
    State(StateArgs),
    /// Check system dependencies and missing toolchains
    Doctor,
    /// List previous runs
    Runs(RunsArgs),
    /// Open artifacts from a run
    Open(OpenArgs),
    /// Generate shell completions and print to stdout
    Completions(CompletionsArgs),
    /// Automatically apply auto-fixes for findings (cargo fix, lint fix)
    Fix,
    /// Initialize prview in the current repository (create policy, ignore)
    Init,
    /// Generate a scoped review pack (only matching files/commits)
    Scope(ScopeArgs),
    /// Run the MCP server over stdio (for agent integrations)
    Mcp {
        #[command(flatten)]
        args: McpArgs,
    },
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
#[command(after_help = "Exit codes:
  0 = PASS, or CONDITIONAL without --strict
  1 = BLOCK
  2 = CONDITIONAL with --strict
  3 = gate could not execute (internal/tooling error)")]
pub struct GateArgs {
    /// Treat CONDITIONAL as exit code 2 instead of advisory exit 0
    #[arg(long)]
    pub strict: bool,

    /// Emit machine-readable gate JSON to stdout
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct McpArgs {
    /// Run a bounded self-smoke of the MCP stdio server and exit
    #[arg(long)]
    pub probe: bool,

    /// Emit the probe result as JSON
    #[arg(long, requires = "probe")]
    pub json: bool,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct ScopeArgs {
    /// Glob patterns for files to include (repeatable)
    #[arg(long, num_args = 1..)]
    pub include: Vec<String>,

    /// Glob patterns for files to exclude (repeatable)
    #[arg(long, num_args = 1..)]
    pub exclude: Vec<String>,

    /// Include staged and unstaged changes
    #[arg(long)]
    pub wip: bool,

    /// Also generate per-file diffs
    #[arg(long = "per-file")]
    pub per_file: bool,

    /// Override merge-base ref (default: auto-detect vs develop/main)
    #[arg(long)]
    pub base: Option<String>,

    /// Output directory (default: ./prview-scope/)
    #[arg(short, long, default_value = "prview-scope")]
    pub output: PathBuf,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct CompletionsArgs {
    /// Target shell
    #[arg(value_enum)]
    pub shell: Shell,
}

impl CompletionsArgs {
    /// Generate completions for the given shell and write to stdout.
    pub fn run(&self) {
        let mut cmd = Cli::command();
        generate(self.shell, &mut cmd, "prview", &mut std::io::stdout());
    }
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct StateArgs {
    /// Fast mode: skip slower repo-state checks
    #[arg(long)]
    pub fast: bool,

    /// Machine-readable JSON output
    #[arg(long)]
    pub json: bool,

    /// Include hot files / churn summary
    #[arg(long)]
    pub hot: bool,

    /// Open the state viewer in TUI mode
    #[arg(long)]
    pub tui: bool,

    /// Inspect a specific repository path
    #[arg(long = "repo", value_name = "PATH")]
    pub repo_path: Option<PathBuf>,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct RunsArgs {
    /// List runs for all repositories
    #[arg(long, short = 'a')]
    pub all: bool,

    /// Filter by branch name
    #[arg(long, short = 'b')]
    pub branch: Option<String>,

    /// Filter by status (pass, fail)
    #[arg(long, short = 's')]
    pub status: Option<String>,

    /// Machine-readable JSON output
    #[arg(long)]
    pub json: bool,

    /// Rebuild the run index from disk
    #[arg(long)]
    pub rebuild: bool,
}

#[derive(Args, Debug, Clone, PartialEq, Eq)]
pub struct OpenArgs {
    /// Open a specific run ID (default: latest for current branch)
    pub run_id: Option<String>,

    /// Print directory path instead of opening the dashboard
    #[arg(long, short = 'd')]
    pub dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum Profile {
    #[default]
    Auto,
    Js,
    Rust,
    Python,
    Mixed,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PolicyModeArg {
    Shadow,
    Warn,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionMode {
    Standard,
    Quick,
    Deep,
    Ci,
    AiOnly,
    Update,
}

impl ExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Quick => "quick",
            Self::Deep => "deep",
            Self::Ci => "ci",
            Self::AiOnly => "ai-only",
            Self::Update => "update",
        }
    }
}

impl Cli {
    pub fn execution_mode(&self) -> ExecutionMode {
        if self.quick {
            ExecutionMode::Quick
        } else if self.ai_only {
            ExecutionMode::AiOnly
        } else if self.update {
            ExecutionMode::Update
        } else if self.ci {
            ExecutionMode::Ci
        } else if self.deep {
            ExecutionMode::Deep
        } else {
            ExecutionMode::Standard
        }
    }

    /// Whether colored output should be force-disabled.
    ///
    /// True for `--no-color` or `--ci`. The `NO_COLOR` environment
    /// convention (no-color.org) is honored natively by the `colored`
    /// crate's auto-detection, so it is intentionally not repeated here.
    pub fn color_disabled(&self) -> bool {
        self.no_color || self.ci
    }

    pub(crate) fn uses_fast_remote_only_standard_preset(&self) -> bool {
        matches!(self.execution_mode(), ExecutionMode::Standard)
            && (self.remote_only || (self.pr.is_some() && !self.local_only))
    }

    /// Determine effective test setting
    pub fn should_run_tests(&self) -> bool {
        if self.skip_tests {
            return false;
        }
        if self.with_tests {
            return true;
        }
        if self.uses_fast_remote_only_standard_preset() {
            return false;
        }
        !matches!(
            self.execution_mode(),
            ExecutionMode::Quick | ExecutionMode::AiOnly | ExecutionMode::Update
        )
    }

    /// Determine effective lint setting
    pub fn should_run_lint(&self) -> bool {
        if self.skip_lint {
            return false;
        }
        if self.with_lint {
            return true;
        }
        !matches!(
            self.execution_mode(),
            ExecutionMode::Quick | ExecutionMode::AiOnly | ExecutionMode::Update
        )
    }

    /// Determine effective heavy security setting (cargo geiger)
    ///
    /// Heavy security checks are slow (3+ min on large projects) so they
    /// only run with --deep, --ci, --with-security, or the explicit
    /// --security-full opt-in. Lightweight security (cargo audit) always runs
    /// regardless of this flag.
    ///
    /// `--security-full` is a deliberate opt-in to the full security tier, so
    /// it implies security intent on its own (a bare `prview --security-full`
    /// runs geiger). It does not override an explicit disable: `--skip-security`
    /// still wins, and the fast presets (`--quick`/`--ai-only`/`--update`) keep
    /// requiring an explicit `--with-security` before any heavy security runs.
    pub fn should_run_security(&self) -> bool {
        if self.quick || self.ai_only || self.update {
            return self.with_security; // only if explicitly requested
        }
        if self.skip_security {
            return false;
        }
        self.with_security || self.deep || self.ci || self.security_full
    }

    /// Determine effective bundle setting
    pub fn should_run_bundle(&self) -> bool {
        if self.quick || self.ai_only || self.update {
            return false;
        }
        if self.skip_bundle {
            return false;
        }
        self.with_bundle || self.ci
    }

    /// Determine effective heuristics setting
    pub fn should_run_heuristics(&self) -> bool {
        if self.quick || self.ai_only || self.update || self.no_heuristics {
            return false;
        }
        if self.uses_fast_remote_only_standard_preset() {
            return false;
        }
        true
    }

    /// Get effective bases (with defaults)
    pub fn effective_bases(&self) -> Vec<String> {
        if self.bases.is_empty() {
            // Try common branch names - will be filtered to existing ones later
            vec![
                "develop".to_string(),
                "main".to_string(),
                "master".to_string(),
            ]
        } else {
            self.bases.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_cli() -> Cli {
        Cli {
            command: None,
            target: None,
            bases: vec![],
            quick: false,
            deep: false,
            ci: false,
            ai_only: false,
            with_tests: false,
            skip_tests: false,
            with_lint: false,
            skip_lint: false,
            with_bundle: false,
            skip_bundle: false,
            with_security: false,
            skip_security: false,
            security_full: false,
            profile: Profile::Auto,
            pr: None,
            gh_repo: None,
            remote: false,
            update: false,
            watch: false,
            no_fetch: false,
            local_only: false,
            remote_only: false,
            no_cache: false,
            quiet: false,
            json: false,
            no_color: false,
            no_heuristics: false,
            no_zip: false,
            no_dashboard: false,
            current_only: false,
            soft_exit: false,
            tests_pattern: None,
            pr_url: None,
            policy_file: None,
            policy_mode: None,
            bridge_stage: 0,
            tui: false,
            output_dir: None,
            shell_setup: false,
            why_blocked: false,
        }
    }

    #[test]
    fn test_should_run_tests_default() {
        let cli = default_cli();
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_security_full_implies_security_in_default_mode() {
        // `--security-full` is a deliberate opt-in to the full security tier, so
        // a bare `prview --security-full` must enable security (and thus geiger),
        // even without `--with-security`/`--deep`/`--ci`.
        let mut cli = default_cli();
        cli.security_full = true;
        assert!(cli.should_run_security());
    }

    #[test]
    fn test_skip_security_overrides_security_full() {
        // An explicit `--skip-security` is a direct security-off and must win
        // over `--security-full`: no minutes-slow geiger when security is
        // deliberately disabled.
        let mut cli = default_cli();
        cli.security_full = true;
        cli.skip_security = true;
        assert!(!cli.should_run_security());
    }

    #[test]
    fn test_quick_does_not_promote_security_full() {
        // Fast presets stay fast: `--security-full` does not force heavy
        // security under `--quick` without an explicit `--with-security`.
        let mut cli = default_cli();
        cli.security_full = true;
        cli.quick = true;
        assert!(!cli.should_run_security());
    }

    #[test]
    fn test_color_disabled_default_keeps_color() {
        assert!(!default_cli().color_disabled());
    }

    #[test]
    fn test_color_disabled_by_no_color_flag() {
        let mut cli = default_cli();
        cli.no_color = true;
        assert!(cli.color_disabled());
    }

    #[test]
    fn test_color_disabled_by_ci() {
        let mut cli = default_cli();
        cli.ci = true;
        assert!(cli.color_disabled());
    }

    #[test]
    fn test_should_run_tests_with_tests() {
        let mut cli = default_cli();
        cli.with_tests = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_deep_mode() {
        let mut cli = default_cli();
        cli.deep = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_ci_mode() {
        let mut cli = default_cli();
        cli.ci = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_with_tests_overrides_quick() {
        let mut cli = default_cli();
        cli.with_tests = true;
        cli.quick = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_skip_tests() {
        let mut cli = default_cli();
        cli.deep = true;
        cli.skip_tests = true;
        assert!(!cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_remote_only_standard_disables_by_default() {
        let mut cli = default_cli();
        cli.remote_only = true;
        assert!(!cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_remote_only_with_tests_overrides_default() {
        let mut cli = default_cli();
        cli.remote_only = true;
        cli.with_tests = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_pr_standard_disables_by_default() {
        let mut cli = default_cli();
        cli.pr = Some(42);
        assert!(!cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_pr_with_tests_overrides_default() {
        let mut cli = default_cli();
        cli.pr = Some(42);
        cli.with_tests = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_pr_local_only_keeps_standard_behavior() {
        let mut cli = default_cli();
        cli.pr = Some(42);
        cli.local_only = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_lint_default() {
        let cli = default_cli();
        assert!(cli.should_run_lint());
    }

    #[test]
    fn test_should_run_lint_with_lint() {
        let mut cli = default_cli();
        cli.with_lint = true;
        assert!(cli.should_run_lint());
    }

    #[test]
    fn test_should_run_lint_deep_mode() {
        let mut cli = default_cli();
        cli.deep = true;
        assert!(cli.should_run_lint());
    }

    #[test]
    fn test_should_run_lint_skip_lint() {
        let mut cli = default_cli();
        cli.with_lint = true;
        cli.skip_lint = true;
        assert!(!cli.should_run_lint());
    }

    #[test]
    fn test_should_run_bundle_default() {
        let cli = default_cli();
        assert!(!cli.should_run_bundle());
    }

    #[test]
    fn test_should_run_bundle_with_bundle() {
        let mut cli = default_cli();
        cli.with_bundle = true;
        assert!(cli.should_run_bundle());
    }

    #[test]
    fn test_should_run_bundle_ci_mode() {
        let mut cli = default_cli();
        cli.ci = true;
        assert!(cli.should_run_bundle());
    }

    #[test]
    fn test_should_run_heuristics_default() {
        let cli = default_cli();
        assert!(cli.should_run_heuristics());
    }

    #[test]
    fn test_should_run_heuristics_quick_disables() {
        let mut cli = default_cli();
        cli.quick = true;
        assert!(!cli.should_run_heuristics());
    }

    #[test]
    fn test_should_run_heuristics_no_heuristics() {
        let mut cli = default_cli();
        cli.no_heuristics = true;
        assert!(!cli.should_run_heuristics());
    }

    #[test]
    fn test_should_run_heuristics_remote_only_standard_disables_by_default() {
        let mut cli = default_cli();
        cli.remote_only = true;
        assert!(!cli.should_run_heuristics());
    }

    #[test]
    fn test_should_run_heuristics_remote_only_deep_stays_enabled() {
        let mut cli = default_cli();
        cli.remote_only = true;
        cli.deep = true;
        assert!(cli.should_run_heuristics());
    }

    #[test]
    fn test_should_run_heuristics_pr_standard_disables_by_default() {
        let mut cli = default_cli();
        cli.pr = Some(42);
        assert!(!cli.should_run_heuristics());
    }

    #[test]
    fn test_should_run_heuristics_pr_deep_stays_enabled() {
        let mut cli = default_cli();
        cli.pr = Some(42);
        cli.deep = true;
        assert!(cli.should_run_heuristics());
    }

    #[test]
    fn test_parse_no_dashboard_flag() {
        let cli = Cli::parse_from(["prview", "--no-dashboard"]);
        assert!(cli.no_dashboard);
    }

    #[test]
    fn test_effective_bases_default() {
        let cli = default_cli();
        let bases = cli.effective_bases();
        assert_eq!(
            bases,
            vec![
                "develop".to_string(),
                "main".to_string(),
                "master".to_string()
            ]
        );
    }

    #[test]
    fn test_effective_bases_custom() {
        let mut cli = default_cli();
        cli.bases = vec!["master".to_string(), "staging".to_string()];
        let bases = cli.effective_bases();
        assert_eq!(bases, vec!["master".to_string(), "staging".to_string()]);
    }

    #[test]
    fn test_should_run_tests_with_tests_overrides_ai_only() {
        let mut cli = default_cli();
        cli.with_tests = true;
        cli.ai_only = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_tests_with_tests_overrides_update() {
        let mut cli = default_cli();
        cli.with_tests = true;
        cli.update = true;
        assert!(cli.should_run_tests());
    }

    #[test]
    fn test_should_run_lint_with_lint_overrides_ai_only() {
        let mut cli = default_cli();
        cli.with_lint = true;
        cli.ai_only = true;
        assert!(cli.should_run_lint());
    }

    #[test]
    fn test_should_run_lint_with_lint_overrides_update() {
        let mut cli = default_cli();
        cli.with_lint = true;
        cli.update = true;
        assert!(cli.should_run_lint());
    }

    #[test]
    fn test_should_run_lint_ci_mode() {
        let mut cli = default_cli();
        cli.ci = true;
        assert!(cli.should_run_lint());
    }

    #[test]
    fn test_execution_mode_default() {
        let cli = default_cli();
        assert_eq!(cli.execution_mode(), ExecutionMode::Standard);
    }

    #[test]
    fn test_execution_mode_update_overrides_deep() {
        let mut cli = default_cli();
        cli.deep = true;
        cli.update = true;
        assert_eq!(cli.execution_mode(), ExecutionMode::Update);
    }

    #[test]
    fn test_should_run_bundle_skip_bundle() {
        let mut cli = default_cli();
        cli.with_bundle = true;
        cli.skip_bundle = true;
        assert!(!cli.should_run_bundle());
    }

    #[test]
    fn test_should_run_bundle_ai_only_disables() {
        let mut cli = default_cli();
        cli.with_bundle = true;
        cli.ai_only = true;
        assert!(!cli.should_run_bundle());
    }

    #[test]
    fn test_should_run_bundle_update_disables() {
        let mut cli = default_cli();
        cli.with_bundle = true;
        cli.update = true;
        assert!(!cli.should_run_bundle());
    }

    #[test]
    fn test_should_run_bundle_deep_mode() {
        let mut cli = default_cli();
        cli.deep = true;
        assert!(!cli.should_run_bundle()); // deep doesn't enable bundle
    }

    #[test]
    fn test_should_run_bundle_quick_disables() {
        let mut cli = default_cli();
        cli.ci = true;
        cli.quick = true; // Note: conflicts_with in clap
        assert!(!cli.should_run_bundle());
    }

    #[test]
    fn test_should_run_heuristics_ai_only_disables() {
        let mut cli = default_cli();
        cli.ai_only = true;
        assert!(!cli.should_run_heuristics());
    }

    #[test]
    fn test_should_run_heuristics_update_disables() {
        let mut cli = default_cli();
        cli.update = true;
        assert!(!cli.should_run_heuristics());
    }

    #[test]
    fn test_profile_default() {
        let cli = default_cli();
        assert_eq!(cli.profile, Profile::Auto);
    }

    #[test]
    fn test_effective_bases_single() {
        let mut cli = default_cli();
        cli.bases = vec!["feature".to_string()];
        let bases = cli.effective_bases();
        assert_eq!(bases.len(), 1);
    }

    #[test]
    fn test_cli_clone() {
        let cli = default_cli();
        let cloned = cli.clone();
        assert_eq!(cli.profile, cloned.profile);
        assert_eq!(cli.quick, cloned.quick);
    }

    #[test]
    fn test_parse_state_subcommand() {
        let cli = Cli::try_parse_from(["prview", "state", "--fast", "--repo", "."]).unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::State(StateArgs {
                fast: true,
                json: false,
                hot: false,
                tui: false,
                repo_path: Some(PathBuf::from(".")),
            }))
        );
        assert_eq!(cli.target, None);
    }

    #[test]
    fn test_parse_gate_subcommand() {
        let cli = Cli::try_parse_from(["prview", "gate", "--strict", "--json"]).unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::Gate(GateArgs {
                strict: true,
                json: true,
            }))
        );
        assert_eq!(cli.target, None);
    }

    #[test]
    fn test_parse_runs_subcommand() {
        let cli =
            Cli::try_parse_from(["prview", "runs", "--all", "--branch", "feature/test"]).unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::Runs(RunsArgs {
                all: true,
                branch: Some("feature/test".to_string()),
                status: None,
                json: false,
                rebuild: false,
            }))
        );
    }

    #[test]
    fn test_parse_open_subcommand() {
        let cli = Cli::try_parse_from(["prview", "open", "20260306-010203", "--dir"]).unwrap();

        assert_eq!(
            cli.command,
            Some(CliCommand::Open(OpenArgs {
                run_id: Some("20260306-010203".to_string()),
                dir: true,
            }))
        );
    }

    #[test]
    fn test_parse_reserved_branch_name_with_double_dash() {
        let cli = Cli::try_parse_from(["prview", "--", "state"]).unwrap();

        assert_eq!(cli.command, None);
        assert_eq!(cli.target, Some("state".to_string()));
    }
}
