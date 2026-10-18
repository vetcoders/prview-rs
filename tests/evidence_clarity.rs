mod support;

use prview::git::git_cmd;
use serde_json::Value;
use std::{fs, path::Path};

fn git(repo: &Path, args: &[&str]) {
    assert!(
        git_cmd()
            .current_dir(repo)
            .args(args)
            .status()
            .unwrap()
            .success()
    );
}

fn read(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn dogfood_real_packs_keep_count_only_history_and_security_scope_truthful() {
    let environment = support::ContractEnvironment::new();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Evidence contract"]);
    git(&repo, &["config", "user.email", "contract@example.invalid"]);
    fs::write(repo.join("README.md"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["branch", "-M", "main"]);
    fs::write(repo.join("README.md"), "documentation change\n").unwrap();
    git(&repo, &["commit", "-qam", "docs"]);
    let previous = temp.path().join("runs/20261017-empty");
    let current = temp.path().join("runs/20261018-source");
    for (out, security_disabled) in [(&previous, false), (&current, true)] {
        if security_disabled {
            fs::write(repo.join("module.py"), "def answer():\n    return 42\n").unwrap();
            git(&repo, &["add", "."]);
            git(&repo, &["commit", "-qm", "source"]);
        }
        let mut command = environment.command();
        command
            .current_dir(&repo)
            .args([
                "HEAD",
                "main",
                "--json",
                "--quiet",
                "--quick",
                "--no-heuristics",
                "--skip-lint",
                "--skip-tests",
                "--no-fetch",
                "--local-only",
                "--no-cache",
                "--output-dir",
            ])
            .arg(out);
        if security_disabled {
            command.arg("--skip-security");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cli: Value = serde_json::from_slice(&output.stdout).unwrap();
        let gate = read(out.join("00_summary/MERGE_GATE.json"));
        let report = read(out.join("report.json"));
        let run = read(out.join("00_summary/RUN.json"));
        assert_eq!(cli["verdict"], gate["decision"]["verdict"]);
        assert_eq!(cli["allow_merge"], gate["decision"]["allow_merge"]);
        assert_eq!(gate["security_scope"], report["security_scope"]);
        assert_eq!(gate["security_scope"], run["security_scope"]);
        assert_eq!(
            report["quality"]["coverage"]["execution_coverage_measured"],
            false
        );
        let scanner = gate["security_scope"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "semgrep_scan")
            .unwrap();
        assert_eq!(
            scanner["execution"],
            if security_disabled {
                "not_executed"
            } else {
                "executed"
            }
        );
        if security_disabled {
            assert_eq!(gate["security_scope"]["requested"], "disabled");
            let comparison = &report["regression"]["tests"]["coverage_comparison"];
            assert_eq!(
                comparison,
                &run["regression"]["tests"]["coverage_comparison"]
            );
            assert_eq!(comparison["source_status"], "recorded");
            assert_eq!(comparison["source"]["run_id"], "20261017-empty");
            assert!(comparison["source"]["metric_path"].is_null());
            assert_eq!(
                comparison["source"]["untested_count_path"],
                "/regression/tests/untested_code_count"
            );
            let narrative = fs::read_to_string(out.join("PR_REVIEW.md")).unwrap();
            assert!(narrative.contains("20261017-empty"));
            assert!(narrative.contains("not with the PR base commit"));
        } else {
            assert!(report["quality"]["coverage"]["heuristic_ratio"].is_null());
            assert_eq!(report["regression"]["tests"]["untested_code_count"], 0);
        }
    }
    let human = environment
        .command()
        .current_dir(&repo)
        .args([
            "HEAD",
            "main",
            "--quick",
            "--no-heuristics",
            "--skip-lint",
            "--skip-tests",
            "--skip-security",
            "--no-fetch",
            "--local-only",
            "--no-cache",
            "--output-dir",
        ])
        .arg(temp.path().join("human-disabled"))
        .output()
        .unwrap();
    assert!(human.status.success());
    let console = String::from_utf8_lossy(&human.stdout);
    assert!(console.contains("requested=disabled"));
    assert!(!console.contains("semgrep(capped)"));
    assert!(!console.contains("security+"));
    assert!(
        git_cmd()
            .current_dir(&repo)
            .args(["status", "--porcelain"])
            .output()
            .unwrap()
            .stdout
            .is_empty()
    );
}
