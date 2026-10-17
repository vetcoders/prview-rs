//! Existing exhaustive Config/Cli clients must keep compiling.

pub fn roundtrip_config(value: prview::Config) -> prview::Config {
    let prview::Config {
        repo_root,
        target,
        pinned_target,
        pinned_diff_bases,
        required_base,
        required_base_exact,
        bases,
        profile,
        requested_profile,
        profile_at_cli_detection,
        execution_mode,
        enforcement_mode,
        update_mode,
        remote_mode,
        current_only,
        tui_mode,
        run_tests,
        run_lint,
        lint_forced,
        run_bundle,
        run_security,
        skip_security,
        run_heuristics,
        security_full,
        do_fetch,
        local_only,
        remote_only,
        use_cache,
        resource_budget,
        resource_plan,
        quiet,
        json,
        create_zip,
        create_dashboard,
        soft_exit,
        repo_path,
        output_dir,
        pr_number,
        pr_url,
        pr_head_oid,
        pr_base_oid,
        gh_repo,
        tests_pattern,
        why_blocked,
        policy_file,
        policy,
        bridge_stage,
        lint_ignore_patterns,
        breaking_escalation,
        scope_non_participating,
        scope_non_participating_builtins,
        changed_paths,
        operator_worktree_clean,
        test_scope,
        full_tests,
        scan_dir_override,
        deadline,
    } = value;
    prview::Config {
        repo_root,
        target,
        pinned_target,
        pinned_diff_bases,
        required_base,
        required_base_exact,
        bases,
        profile,
        requested_profile,
        profile_at_cli_detection,
        execution_mode,
        enforcement_mode,
        update_mode,
        remote_mode,
        current_only,
        tui_mode,
        run_tests,
        run_lint,
        lint_forced,
        run_bundle,
        run_security,
        skip_security,
        run_heuristics,
        security_full,
        do_fetch,
        local_only,
        remote_only,
        use_cache,
        resource_budget,
        resource_plan,
        quiet,
        json,
        create_zip,
        create_dashboard,
        soft_exit,
        repo_path,
        output_dir,
        pr_number,
        pr_url,
        pr_head_oid,
        pr_base_oid,
        gh_repo,
        tests_pattern,
        why_blocked,
        policy_file,
        policy,
        bridge_stage,
        lint_ignore_patterns,
        breaking_escalation,
        scope_non_participating,
        scope_non_participating_builtins,
        changed_paths,
        operator_worktree_clean,
        test_scope,
        full_tests,
        scan_dir_override,
        deadline,
    }
}
pub fn roundtrip_cli(value: prview::cli::Cli) -> prview::cli::Cli {
    let prview::cli::Cli {
        command,
        target,
        bases,
        quick,
        deep,
        ci,
        ai_only,
        with_tests,
        skip_tests,
        with_lint,
        skip_lint,
        with_bundle,
        skip_bundle,
        with_security,
        skip_security,
        security_full,
        profile,
        pr,
        gh_repo,
        remote,
        update,
        watch,
        no_fetch,
        local_only,
        remote_only,
        no_cache,
        resource_budget,
        quiet,
        json,
        no_color,
        no_heuristics,
        no_zip,
        no_dashboard,
        current_only,
        soft_exit,
        fail_on_warnings,
        tests_pattern,
        full_tests,
        deadline,
        no_deadline,
        pr_url,
        policy_file,
        policy_mode,
        bridge_stage,
        tui,
        output_dir,
        shell_setup,
        build_source_sha,
        why_blocked,
    } = value;
    prview::cli::Cli {
        command,
        target,
        bases,
        quick,
        deep,
        ci,
        ai_only,
        with_tests,
        skip_tests,
        with_lint,
        skip_lint,
        with_bundle,
        skip_bundle,
        with_security,
        skip_security,
        security_full,
        profile,
        pr,
        gh_repo,
        remote,
        update,
        watch,
        no_fetch,
        local_only,
        remote_only,
        no_cache,
        resource_budget,
        quiet,
        json,
        no_color,
        no_heuristics,
        no_zip,
        no_dashboard,
        current_only,
        soft_exit,
        fail_on_warnings,
        tests_pattern,
        full_tests,
        deadline,
        no_deadline,
        pr_url,
        policy_file,
        policy_mode,
        bridge_stage,
        tui,
        output_dir,
        shell_setup,
        build_source_sha,
        why_blocked,
    }
}

#[test]
fn exhaustive_public_patterns_and_literals_keep_their_field_sets() {
    let _: fn(prview::Config) -> prview::Config = roundtrip_config;
    let _: fn(prview::Cli) -> prview::Cli = roundtrip_cli;
}

#[test]
fn library_selection_child() {
    use clap::Parser;
    let Ok(case) = std::env::var("PRVIEW_API_SELECTION_CASE") else {
        return;
    };
    let args = if case == "cli-explicit" || case == "builder-auto" {
        vec![
            "prview",
            "HEAD",
            "main",
            "--quick",
            "--skip-security",
            "--no-heuristics",
            "--no-fetch",
            "--no-zip",
            "--no-dashboard",
            "--quiet",
        ]
    } else {
        vec![
            "prview",
            "--quick",
            "--skip-security",
            "--no-heuristics",
            "--no-fetch",
            "--no-zip",
            "--no-dashboard",
            "--quiet",
        ]
    };
    let cli = prview::Cli::parse_from(args);
    let mut config = prview::Config::from_cli(&cli).unwrap();
    if case == "pr-auto-override" {
        config.pr_number = Some(17);
    }
    let mut app = if case.starts_with("cli-") {
        prview::App::new(&cli).unwrap()
    } else {
        prview::App::from_config(config).unwrap()
    };
    if case == "builder-explicit" {
        app = app.with_explicit_bases(true);
    }
    if case == "builder-auto" || case == "pr-auto-override" {
        app = app.with_explicit_bases(false);
    }
    let out = std::env::var_os("PRVIEW_API_SELECTION_OUTPUT").unwrap();
    app.config.output_dir = Some(out.into());
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(app.run())
        .unwrap();
}

#[test]
fn config_cli_chain_keeps_automatic_fallback_and_explicit_entry_points() {
    use std::{fs, process::Command};
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = prview::git::git_cmd()
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "api@example.test"]);
    git(&["config", "user.name", "API compatibility"]);
    fs::write(repo.path().join("README.md"), "fixture\n").unwrap();
    fs::write(
        repo.path().join(".prview-policy.yml"),
        "version: 1\nmode: warn\ndefault_severity: ignore\n",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "fixture"]);
    for (case, explicit) in [
        ("legacy-chain", false),
        ("builder-explicit", true),
        ("builder-auto", false),
        ("cli-explicit", true),
        ("cli-auto", false),
        ("pr-auto-override", true),
    ] {
        let owned = tempfile::tempdir().unwrap();
        let pack = owned.path().join("pack");
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "library_selection_child", "--nocapture"])
            .current_dir(repo.path())
            .env("PRVIEW_HOME", owned.path().join("home"))
            .env("PRVIEW_API_SELECTION_CASE", case)
            .env("PRVIEW_API_SELECTION_OUTPUT", &pack)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{case}: {} {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_slice(&fs::read(pack.join(name)).unwrap()).unwrap()
        };
        let gate = read("00_summary/MERGE_GATE.json");
        assert_eq!(gate["review_range"]["explicit_bases"], explicit, "{case}");
        assert_eq!(
            gate["review_range"]["comparisons"][0]["status"], "self_diff",
            "{case}"
        );
        assert_eq!(
            gate["decision"]["verdict"],
            if explicit { "CONDITIONAL" } else { "PASS" },
            "{case}"
        );
        assert_eq!(
            gate["review_range"],
            read("report.json")["review_range"],
            "{case}"
        );
        assert_eq!(
            gate["review_range"],
            read("00_summary/PROVENANCE.json")["review_range"],
            "{case}"
        );
    }
}
