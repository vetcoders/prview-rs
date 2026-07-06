use anyhow::{Context, Result, bail};
use clap::Parser;
use colored::Colorize;
use prview::cli::{GateArgs, McpArgs};
use prview::git::git_cmd;
use prview::{App, Cli, CliCommand, Config, OpenArgs, RunsArgs, ScopeArgs, StateArgs};
use std::path::{Path, PathBuf};
use std::process::Command;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        display_error(&err);
        std::process::exit(1);
    }
}

fn display_error(err: &anyhow::Error) {
    eprintln!("{} {}", "error:".red().bold(), err);

    // Show cause chain (skip root which we already printed)
    for cause in err.chain().skip(1) {
        eprintln!("  {} {cause}", "caused by:".yellow());
    }

    // Contextual hints based on error message content
    let msg = format!("{err:?}").to_lowercase();
    let hint = if msg.contains("repository") || msg.contains("git") {
        Some("make sure you're running prview from inside a git repository")
    } else if msg.contains("permission") || msg.contains("denied") {
        Some("check file permissions on ~/.prview/")
    } else if msg.contains("not found") {
        ["cargo", "npm", "python", "node"]
            .iter()
            .find(|tool| msg.contains(**tool))
            .map(|tool| {
                // Leak is fine: we exit right after this
                Box::leak(
                    format!("make sure {tool} is installed and in your PATH").into_boxed_str(),
                ) as &str
            })
    } else if msg.contains("remote") || msg.contains("fetch") {
        Some("check your network connection and remote repository access")
    } else {
        None
    };

    if let Some(hint) = hint {
        eprintln!("  {} {hint}", "hint:".cyan().bold());
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Force-disable ANSI color for --no-color / --ci before anything prints.
    // set_override wins over colored's auto-detection; the NO_COLOR env
    // convention is already honored natively by the colored crate.
    if cli.color_disabled() {
        colored::control::set_override(false);
    }

    if cli.shell_setup {
        print_shell_setup();
        return Ok(());
    }

    if let Some(command) = &cli.command {
        return match command {
            CliCommand::Gate(args) => match run_gate_command(&cli, args).await {
                Ok(exit_code) => std::process::exit(exit_code),
                Err(err) => {
                    display_error(&err);
                    std::process::exit(prview::gate::GATE_EXECUTION_ERROR_EXIT_CODE);
                }
            },
            CliCommand::State(args) => {
                run_state_command(Config::from_cli(&cli).ok().as_ref(), args).await
            }
            CliCommand::Doctor => run_doctor_command(Config::from_cli(&cli)).await,
            CliCommand::Runs(args) => run_runs_command(args),
            CliCommand::Open(args) => run_open_command(args),
            CliCommand::Fix => run_fix_command().await,
            CliCommand::Init => run_init_command(&cli).await,
            CliCommand::Completions(args) => {
                args.run();
                Ok(())
            }
            CliCommand::Scope(args) => run_scope_command(args),
            CliCommand::Mcp { args } => run_mcp_command(args).await,
        };
    }

    let config = Config::from_cli(&cli)?;

    // TUI mode
    if cli.tui {
        prview::tui::run_tui(config).await?;
        return Ok(());
    }

    let app = App::from_config(config)?;

    // Watch mode
    if cli.watch {
        app.run_watch().await?;
        return Ok(());
    }

    // Normal run
    let report = app.run().await?;

    let cli_summary = prview::output::build_cli_json_summary(&app.config, &report);

    // JSON output mode. Human summaries are emitted by App::run(); do not
    // print them a second time here.
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&cli_summary)?);
    }

    // Exit with appropriate code. An unchanged --update run re-checked nothing,
    // so it exits 0 regardless of --json — previously the human path exited 0
    // while the JSON path derived its code from an empty gate.
    let exit_code = if report.unchanged || cli.soft_exit {
        0
    } else {
        prview::output::compute_exit_code(&cli_summary)
    };

    std::process::exit(exit_code);
}

async fn run_gate_command(cli: &Cli, args: &GateArgs) -> Result<i32> {
    let mut run_cli = cli.clone();
    run_cli.command = None;
    run_cli.quick = true;
    run_cli.deep = false;
    run_cli.ci = false;
    run_cli.ai_only = false;
    run_cli.update = false;
    run_cli.watch = false;
    run_cli.tui = false;
    run_cli.shell_setup = false;
    run_cli.quiet = true;
    run_cli.json = false;
    run_cli.soft_exit = false;

    let mut config = Config::from_cli(&run_cli)?;
    config.apply_gate_profile();
    let app = App::from_config(config)?;
    let report = app.run().await.context("gate review run failed")?;
    let cli_summary = prview::output::build_cli_json_summary(&app.config, &report);
    let merge_gate_path = report
        .artifacts_dir
        .join("00_summary")
        .join("MERGE_GATE.json");
    let summary =
        prview::gate::build_gate_json_output(&cli_summary, &merge_gate_path, args.strict)?;

    if args.json || cli.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_gate_summary(&summary);
    }

    Ok(summary.exit_code)
}

fn print_gate_summary(summary: &prview::gate::GateJsonOutput) {
    println!(
        "prview gate: {} (exit {})",
        summary.verdict.as_str(),
        summary.exit_code
    );
    println!("output: {}", summary.output_dir);

    if !summary.blocking_issues.is_empty() {
        println!("blocking issues:");
        for issue in &summary.blocking_issues {
            println!("  - {issue}");
        }
    }

    if !summary.caveats.is_empty() {
        println!("caveats:");
        for caveat in &summary.caveats {
            println!("  - {caveat}");
        }
    }

    if let Some(reason) = &summary.decision_reason {
        println!("reason: {reason}");
    }
}

async fn run_fix_command() -> Result<()> {
    println!(
        "{}",
        "Applying automatic fixes for common findings...".cyan()
    );

    // Track per-tool outcomes so the final message tells the truth.
    let mut ran: Vec<(&str, bool)> = Vec::new();
    let fix_status = |cmd: &mut Command| -> bool {
        cmd.stdin(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    if Path::new("Cargo.toml").exists() {
        println!("  {} Running cargo fmt...", "▶".blue());
        ran.push(("cargo fmt", fix_status(Command::new("cargo").arg("fmt"))));

        println!("  {} Running cargo clippy --fix...", "▶".blue());
        ran.push((
            "cargo clippy --fix",
            fix_status(Command::new("cargo").args([
                "clippy",
                "--fix",
                "--allow-dirty",
                "--allow-staged",
                "--allow-no-vcs-ignore",
            ])),
        ));
    }

    if Path::new("package.json").exists() {
        println!("  {} Running eslint --fix...", "▶".blue());
        // --no-install + null stdin: a missing eslint must fail fast, not
        // auto-install from the registry or sit on an interactive prompt.
        let ok = if which::which("pnpm").is_ok() {
            fix_status(Command::new("pnpm").args(["exec", "eslint", "--fix", "."]))
        } else {
            fix_status(Command::new("npx").args(["--no-install", "eslint", "--fix", "."]))
        };
        ran.push(("eslint --fix", ok));
    }

    let failed: Vec<&str> = ran
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(name, _)| *name)
        .collect();
    if failed.is_empty() {
        println!(
            "\n{} Fixes applied. Run `git diff` to review changes.",
            "✓".green()
        );
    } else {
        println!(
            "\n{} Some fixers did not succeed: {}. Run `git diff` to review what was applied.",
            "⚠".yellow(),
            failed.join(", ")
        );
    }
    Ok(())
}

async fn run_init_command(cli: &prview::Cli) -> Result<()> {
    use prview::config::ProfileKind;
    use std::fs;
    println!(
        "{}",
        "=== Initializing prview in the current repository ==="
            .cyan()
            .bold()
    );

    let repo_root = std::env::current_dir()?;
    let config = Config::from_cli(cli)?;
    let profile_kind = config.profile.kind;
    println!(
        "  {} Detected project profile: {:?}",
        "✓".green(),
        profile_kind
    );

    // 1. Check if it's a git repo
    if !repo_root.join(".git").exists() {
        println!(
            "  {} Warning: .git directory not found. Not a git repository?",
            "⚠".yellow()
        );
    } else {
        println!("  {} Git repository detected", "✓".green());
    }

    // 2. Create .prview-policy.yml if missing
    let policy_path = repo_root.join(".prview-policy.yml");
    if !policy_path.exists() {
        println!(
            "  {} Creating profile-aware .prview-policy.yml...",
            "▶".blue()
        );

        let checks_preset = match profile_kind {
            ProfileKind::Rust => {
                r#"
  cargo_audit: block
  cargo_geiger: block
  clippy: warn
  dead_exports: warn
  cycles: block"#
            }
            ProfileKind::Js => {
                r#"
  eslint: warn
  stylelint: warn
  dep_audit: block"#
            }
            ProfileKind::Python => {
                r#"
  ruff: warn
  mypy: warn
  pip_audit: block"#
            }
            _ => {
                r#"
  breaking_changes: block
  coverage_regression: warn"#
            }
        };

        let default_policy = format!(
            r#"# prview merge gate policy (v1)
version: 1

# Mode: shadow (log only), warn (non-blocking), block (fail on violation)
mode: warn

# Default severity for checks not explicitly listed
default_severity: warn

# Explicit check severity overrides for {:?} profile
checks:{}
  breaking_changes: block
  coverage_regression: warn
"#,
            profile_kind, checks_preset
        );

        fs::write(&policy_path, default_policy)?;
        println!("  {} Created .prview-policy.yml", "✓".green());
    } else {
        println!(
            "  {} .prview-policy.yml already exists, skipping.",
            "ℹ".blue()
        );
    }

    // 3. Update .gitignore if missing prview-artifacts
    let gitignore_path = repo_root.join(".gitignore");
    if gitignore_path.exists() {
        let content = fs::read_to_string(&gitignore_path)?;
        if !content.contains("prview-artifacts") {
            println!("  {} Adding prview-artifacts to .gitignore...", "▶".blue());
            let mut file = fs::OpenOptions::new().append(true).open(&gitignore_path)?;
            use std::io::Write;
            writeln!(file, "\n# prview artifacts\nprview-artifacts/")?;
            println!("  {} Updated .gitignore", "✓".green());
        } else {
            println!(
                "  {} prview-artifacts already in .gitignore, skipping.",
                "ℹ".blue()
            );
        }
    } else {
        println!("  {} .gitignore not found, skipping.", "ℹ".blue());
    }

    println!(
        "\n{} Initialization complete! Run `prview` to start your first analysis.",
        "✓".green()
    );
    Ok(())
}

fn print_shell_setup() {
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "prview".into());

    let shell = std::env::var("SHELL").unwrap_or_default();
    let is_zsh = shell.contains("zsh");
    let rc_file = if is_zsh { "~/.zshrc" } else { "~/.bashrc" };

    println!("# prview shell setup");
    println!("#");
    println!("# Add this to {} :", rc_file);
    println!();
    println!("# --- prview aliases ---");
    println!("alias prview='{bin}'");
    println!("alias prv='prview --quick'");
    println!();
    println!("prvpr() {{");
    println!("  if [ -z \"${{1:-}}\" ]; then");
    println!("    echo \"Usage: prvpr <PR_NUMBER> [extra flags]\"");
    println!("    return 2");
    println!("  fi");
    println!("  local pr_number=\"$1\"");
    println!("  shift");
    println!("  prview --pr \"$pr_number\" --quick \"$@\"");
    println!("}}");
    println!();
    println!("prvjson() {{");
    println!("  prview --json --quiet \"$@\"");
    println!("}}");
    println!("# --- end prview ---");
    println!();
    println!("# Aliases:");
    println!("#   prview             Full binary, all flags available");
    println!("#   prv                Quick run (skip lint+tests)");
    println!("#   prvpr <N>          Quick run for GitHub PR #N");
    println!("#   prvjson            Machine-readable JSON output");
    println!();
    println!("# Or source the bundled file:");
    println!("#   source <prview-repo>/tools/shell/prview-aliases.zsh");
}

async fn run_state_command(_config: Option<&Config>, args: &StateArgs) -> Result<()> {
    let root = if let Some(path) = &args.repo_path {
        path.clone()
    } else {
        find_repo_root()?
    };

    let opts = prview::state::StateOpts {
        fast: args.fast,
        json: args.json,
        hot: args.hot,
    };

    if args.tui {
        if args.json {
            eprintln!("prview state: --json is ignored in --tui mode");
        }
        let repo_state = prview::state::collect_state(&root, &opts)?;
        let config = if let Some(c) = _config {
            c.clone()
        } else {
            Config::for_state_viewer(&root)?
        };
        prview::tui::run_tui_state(config, repo_state).await?;
        return Ok(());
    }

    prview::state::run(&root, &opts)
}

async fn run_doctor_command(config: Result<Config>) -> Result<()> {
    use colored::Colorize;
    println!(
        "{}",
        "╔════════════════════════════════════════════════════════════════╗".cyan()
    );
    println!(
        "{}",
        "║                      PRVIEW DOCTOR                             ║"
            .cyan()
            .bold()
    );
    println!(
        "{}",
        "╠════════════════════════════════════════════════════════════════╣".cyan()
    );
    println!("{} {}", "║".cyan(), "Vetcoders".bold());
    println!(
        "{}",
        "╚════════════════════════════════════════════════════════════════╝".cyan()
    );
    println!();

    let cwd = std::env::current_dir()?;
    let _repo_root = match find_repo_root() {
        Ok(root) => {
            println!("{} Repository found at: {}", "✓".green(), root.display());
            root
        }
        Err(_) => {
            println!("{} Not inside a git repository.", "⚠".yellow());
            cwd
        }
    };

    // Check active profile
    match &config {
        Ok(config) => {
            println!(
                "{} Active profile: {}",
                "✓".green(),
                format!("{:?}", config.profile.kind).bold()
            );
            if config.profile.is_workspace {
                println!("   (detected as monorepo/workspace)");
            }
        }
        Err(e) => {
            // Surface the real reason (policy parse error, not-a-repo, ...)
            // instead of the previous blanket "maybe not in a project?".
            println!("{} Could not determine active profile: {}", "⚠".yellow(), e);
            for cause in e.chain().skip(1) {
                println!("   {} {cause}", "caused by:".yellow());
            }
        }
    }

    println!();
    println!("{}", "--- Toolchains & Dependencies ---".bold());

    let mut tools = vec![
        ("git", "git --version"),
        ("make", "make --version"),
        ("semgrep", "semgrep --version"),
    ];

    // Profile-specific tools
    if let Ok(config) = &config {
        match config.profile.kind {
            prview::config::ProfileKind::Rust | prview::config::ProfileKind::Mixed => {
                tools.push(("cargo", "cargo --version"));
                tools.push(("rustc", "rustc --version"));
                tools.push(("rustfmt", "cargo fmt --version"));
                tools.push(("clippy", "cargo clippy --version"));
            }
            _ => {}
        }

        match config.profile.kind {
            prview::config::ProfileKind::Js | prview::config::ProfileKind::Mixed => {
                tools.push(("node", "node --version"));
                tools.push(("npm", "npm --version"));
                tools.push(("pnpm", "pnpm --version"));
                tools.push(("yarn", "yarn --version"));
            }
            _ => {}
        }

        match config.profile.kind {
            prview::config::ProfileKind::Python | prview::config::ProfileKind::Mixed => {
                tools.push(("python3", "python3 --version"));
                tools.push(("pip", "pip --version"));
                tools.push(("ruff", "ruff --version"));
            }
            _ => {}
        }
    } else {
        // Fallback for no config
        tools.extend([
            ("cargo", "cargo --version"),
            ("npm", "npm --version"),
            ("python3", "python3 --version"),
        ]);
    }

    for (name, cmd) in tools {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let mut command = std::process::Command::new(parts[0]);
        for arg in &parts[1..] {
            command.arg(arg);
        }

        match command.output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                println!("  {} {:<10} ({})", "✓".green(), name, version.trim());
            }
            _ => {
                println!("  {} {:<10} not found", "✗".red(), name);
            }
        }
    }

    println!();
    Ok(())
}

fn find_repo_root() -> Result<PathBuf> {
    let output = git_cmd()
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Failed to run git — is git installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Not a git repository: {}", stderr.trim());
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("Not a git repository (empty git output)");
    }

    Ok(PathBuf::from(root))
}

fn run_runs_command(args: &RunsArgs) -> Result<()> {
    let opts = prview::storage::RunsOpts {
        all: args.all,
        branch: args.branch.clone(),
        status: args.status.clone(),
        json: args.json,
        rebuild: args.rebuild,
    };
    prview::storage::run_runs_command(&opts)
}

fn run_open_command(args: &OpenArgs) -> Result<()> {
    let opts = prview::storage::OpenOpts {
        run_id: args.run_id.clone(),
        dir_only: args.dir,
    };
    prview::storage::run_open_command(&opts)
}

fn run_scope_command(args: &ScopeArgs) -> Result<()> {
    prview::scope::run(args)
}

async fn run_mcp_command(args: &McpArgs) -> Result<()> {
    if args.probe {
        prview::mcp::probe(args.json).await
    } else {
        prview::mcp::serve().await
    }
}
