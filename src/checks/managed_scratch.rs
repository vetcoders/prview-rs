//! Effective scratch for snapshot-only Cargo/uv commands. Config getters stay
//! configured paths; no old cache is migrated or pruned by relocation.
use crate::Config;
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

pub(super) fn snapshot_root(config: &Config, configured: PathBuf, kind: &str) -> Result<PathBuf> {
    let caller = std::fs::canonicalize(&config.repo_root)?;
    let resolved = super::caller_integrity::planned_path(&configured)?;
    let mut visible = source_visible(&caller, &configured)?;
    // A descendant link can redirect writes even when the namespace root is
    // external. Inspect links without traversing them or reading file contents.
    if resolved.exists() {
        for entry in walkdir::WalkDir::new(&resolved).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                let target = std::fs::canonicalize(entry.path()).with_context(|| {
                    format!("unresolved managed scratch link {}", entry.path().display())
                })?;
                if target.starts_with(&caller) || source_visible(&caller, &target)? {
                    visible = true;
                    break;
                }
            }
        }
    }
    if !visible {
        return Ok(configured);
    }
    let namespace = configured
        .file_name()
        .context("managed scratch has no repository namespace")?;
    let effective = std::env::temp_dir()
        .join("prview-managed-scratch")
        .join(kind)
        .join(namespace);
    let effective_resolved = super::caller_integrity::planned_path(&effective)?;
    ensure!(
        !effective_resolved.starts_with(&caller) && !source_visible(&caller, &effective_resolved)?,
        "cannot place {kind} snapshot scratch outside caller: temporary namespace {} is inside {}",
        effective.display(),
        caller.display()
    );
    // Do not reuse a redirected descendant in the chosen effective namespace.
    if effective_resolved.exists() {
        for entry in walkdir::WalkDir::new(&effective_resolved).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                let target = std::fs::canonicalize(entry.path())?;
                ensure!(
                    !target.starts_with(&caller) && !source_visible(&caller, &target)?,
                    "effective {kind} scratch redirects into caller at {}",
                    entry.path().display()
                );
            }
        }
    }
    eprintln!(
        "prview: {kind} snapshot scratch relocated from {} to {} because the configured namespace is caller-visible; old scratch retained",
        configured.display(),
        effective.display()
    );
    Ok(effective)
}

fn source_visible(caller: &std::path::Path, path: &std::path::Path) -> Result<bool> {
    Ok(
        super::caller_integrity::namespace_source_visible(caller, path)?
            || crate::owned_output::namespace_source_visible(path)?
            || crate::review_workspace::scratch_overlaps_caller(path)?,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::checks::{Check, CheckResult, CheckStatus};
    use std::sync::Arc;

    struct LateRedirect {
        caller: PathBuf,
        configured: PathBuf,
        kind: &'static str,
        redirect_root: bool,
    }

    #[async_trait::async_trait]
    impl Check for LateRedirect {
        fn name(&self) -> &str {
            "Late scratch control"
        }
        fn check_eligibility(&self, _: &Config) -> crate::checks::CheckEligibility {
            crate::checks::CheckEligibility::Run
        }
        async fn run(&self, config: &Config) -> Result<CheckResult> {
            if self.redirect_root {
                std::fs::create_dir_all(self.configured.parent().unwrap())?;
                std::os::unix::fs::symlink(&self.caller, &self.configured)?;
            } else {
                std::fs::create_dir_all(&self.configured)?;
                std::os::unix::fs::symlink(&self.caller, self.configured.join("redirect"))?;
            }
            let effective = snapshot_root(config, self.configured.clone(), self.kind)?;
            ensure!(
                effective != self.configured,
                "late redirected scratch was admitted"
            );
            Ok(CheckResult {
                name: self.name().to_owned(),
                status: CheckStatus::Passed,
                duration: std::time::Duration::ZERO,
                output: effective.display().to_string(),
                cached: false,
                provenance: None,
            })
        }
    }

    async fn assert_late_redirect(kind: &'static str, events: bool, redirect_root: bool) {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let workspace =
            Arc::new(crate::review_workspace::IndependentReview::create(&config).unwrap());
        let config = workspace.config(&config).unwrap();
        let configured = if kind == "cargo-target" {
            config.cargo_build_cache_dir()
        } else {
            config.uv_env_root()
        };
        let ledger = crate::ledger::TaskLedger::new();
        ledger.retain_execution_repo(Some(workspace));
        let checks: Vec<Box<dyn Check>> = vec![Box::new(LateRedirect {
            caller: caller.path().to_path_buf(),
            configured,
            kind,
            redirect_root,
        })];
        let cache = crate::cache::Cache::with_dir(home.path().join("results"), false);
        let governor = Arc::new(crate::governor::ResourceGovernor::new());
        let (results, _) = if events {
            crate::checks::run_all_checks_with_events(
                checks,
                cache,
                &config,
                &ledger,
                &governor,
                |_| {},
            )
            .await
            .unwrap()
        } else {
            crate::checks::run_all_checks(checks, cache, &config, &ledger, &governor)
                .await
                .unwrap()
        };
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].status,
            CheckStatus::Passed,
            "{}",
            results[0].output
        );
        assert_eq!(
            std::fs::read(caller.path().join("source.txt")).unwrap(),
            b"original\n"
        );
    }

    #[tokio::test]
    async fn late_cargo_scratch_redirect_is_rechecked_by_headless_planning() {
        assert_late_redirect("cargo-target", false, false).await;
    }

    #[tokio::test]
    async fn late_uv_scratch_redirect_is_rechecked_by_tui_planning() {
        assert_late_redirect("uv-env", true, false).await;
    }

    #[tokio::test]
    async fn late_cargo_scratch_root_redirect_is_rechecked() {
        assert_late_redirect("cargo-target", false, true).await;
    }

    #[tokio::test]
    async fn late_uv_scratch_root_redirect_is_rechecked() {
        assert_late_redirect("uv-env", true, true).await;
    }

    #[tokio::test]
    async fn unrelated_external_scratch_keeps_its_configured_path() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let workspace = crate::review_workspace::IndependentReview::create(&config).unwrap();
        let config = workspace.config(&config).unwrap();
        let configured = config.cargo_build_cache_dir();
        let planned = crate::review_workspace::with_caller_scratch_protection(
            Some(caller.path().canonicalize().unwrap()),
            async { snapshot_root(&config, configured.clone(), "cargo-target") },
        )
        .await
        .unwrap();
        assert_eq!(planned, configured);
    }
}
