//! An owned Git repository for remote reviews, not a host filesystem sandbox.
//!
//! Only Git objects/refs cross this boundary. Caller source, dependencies,
//! config and worktree registrations are never copied or borrowed for writes.
use crate::{Config, git::git_cmd};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

pub(crate) struct IndependentReview {
    pub(crate) caller_root: PathBuf,
    pub(crate) repo_root: PathBuf,
    _directory: tempfile::TempDir,
}

impl IndependentReview {
    pub(crate) fn create(config: &Config) -> Result<Self> {
        let caller_root = std::fs::canonicalize(&config.repo_root)?;
        // The old guard covered internal outputs too. Do not silently move those
        // writers outside its observation while decoupling unrelated source.
        for path in [
            Some(crate::config::prview_home()),
            config.output_dir.clone(),
            Some(config.cargo_build_cache_dir()),
            Some(config.uv_env_root()),
        ]
        .into_iter()
        .flatten()
        {
            let physical = crate::checks::caller_integrity::planned_path(&path)?;
            ensure!(
                !physical.starts_with(&caller_root),
                "remote review write path {} is inside the caller; choose an external home/output/cache",
                path.display()
            );
        }
        let directory = tempfile::Builder::new().prefix("prview-review-").tempdir()?;
        ensure!(
            !std::fs::canonicalize(directory.path())?.starts_with(&caller_root),
            "remote review temporary repository must be outside the caller"
        );
        let repo_root = directory.path().join(config.repo_name());
        let source = git2::Repository::open(&caller_root)?;
        let destination = git2::Repository::init(&repo_root)?;
        copy_objects(&source, &destination)?;
        copy_modules(&source, &destination, 0)?;
        if let Ok(origin) = source.find_remote("origin")
            && let Some(url) = origin.url()
        {
            destination.remote("origin", url)?;
        }
        if let Ok(head) = source.head() {
            let oid = head.peel_to_commit()?.id();
            ensure!(destination.find_commit(oid).is_ok(), "caller HEAD changed during object copy");
            if let Some(name) = head.name().filter(|_| head.is_branch()) {
                ensure!(
                    destination.find_reference(name)?.target() == Some(oid),
                    "caller branch changed during object copy"
                );
                destination.set_head(name)?;
            } else {
                destination.set_head_detached(oid)?;
            }
            destination.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))?;
        }
        Ok(Self { caller_root, repo_root, _directory: directory })
    }

    pub(crate) fn config(&self, original: &Config) -> Result<Config> {
        let mut config = original.clone();
        config.repo_root = self.repo_root.clone();
        config.repo_path = Some(self.repo_root.clone());
        config.scan_dir_override = None;
        // Programmatic profiles can contain absolute paths even when automatic
        // refresh is disabled. Never leave those pointing at the caller.
        let rebase = |path: &Path| -> Result<PathBuf> {
            Ok(self.repo_root.join(path.strip_prefix(&original.repo_root)
                .or_else(|_| path.strip_prefix(&self.caller_root))
                .context("profile path is outside remote review caller")?))
        };
        let rebase_profile = |profile: &mut crate::config::DetectedProfile| -> Result<()> {
            if let Some(path) = &profile.cargo_root {
                profile.cargo_root = Some(rebase(path)?);
            }
            profile.rust_dirs = profile.rust_dirs.iter()
                .map(|path| rebase(path)).collect::<Result<_>>()?;
            Ok(())
        };
        rebase_profile(&mut config.profile)?;
        if let Some(profile) = &mut config.profile_at_cli_detection {
            rebase_profile(profile)?;
        }
        if let Some(path) = &config.output_dir {
            config.output_dir = Some(crate::checks::caller_integrity::planned_path(path)?);
        }
        Ok(config)
    }
}

fn copy_objects(source: &git2::Repository, destination: &git2::Repository) -> Result<()> {
    // A normal fetch transports object bytes, unlike a local clone/shared ODB.
    // It reads the source but never creates a registration/ref/lock there.
    // No repository config, hooks, index or working-tree bytes are imported.
    let mut command = git_cmd();
    command.args(["-c", "core.hooksPath=", "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--update-head-ok"])
        .arg(source.path()).arg("+refs/*:refs/*");
    if source.head().is_ok() {
        command.arg("HEAD:refs/prview/copied-head");
    }
    command.current_dir(destination.path());
    let output = crate::proc::output_governed_with_timeout(
        command, "copy remote review objects", std::time::Duration::from_secs(120),
    )?;
    ensure!(output.status.success(), "cannot copy remote review objects: {}",
        String::from_utf8_lossy(&output.stderr).trim());
    Ok(())
}

fn copy_modules(source: &git2::Repository, destination: &git2::Repository, depth: usize) -> Result<()> {
    ensure!(depth < 16, "remote review submodule object nesting limit exceeded");
    for module in source.submodules()? {
        let Some(relative) = module.path().to_str() else {
            anyhow::bail!("non-UTF-8 remote review submodule path");
        };
        crate::paths::validate_repo_relative_str(relative)?;
        // An uninitialized/unavailable module remains unavailable. The exact
        // snapshot planner already fails closed if a required pin is missing.
        let Ok(source_module) = module.open() else { continue };
        let module_root = destination.commondir().join("modules").join(relative);
        let destination_module = git2::Repository::init_bare(module_root)?;
        copy_objects(&source_module, &destination_module)?;
        copy_modules(&source_module, &destination_module, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::caller_integrity::CallerGuard;

    #[test]
    fn independent_repo_copies_objects_without_ambient_source_or_dependencies() {
        let (caller, _home, config) = crate::checks::caller_guard_tests::fixture();
        let foreign = caller.path().join(".claude/worktrees/foreign");
        std::fs::create_dir_all(&foreign).unwrap();
        git2::Repository::init(&foreign).unwrap();
        std::fs::write(foreign.join("unrelated.txt"), "foreign\n").unwrap();
        std::fs::create_dir(caller.path().join("node_modules")).unwrap();
        std::fs::write(caller.path().join("untracked.txt"), "operator\n").unwrap();
        let workspace = IndependentReview::create(&config).unwrap();
        assert!(!workspace.repo_root.join(".claude").exists());
        assert!(!workspace.repo_root.join("node_modules").exists());
        assert!(!workspace.repo_root.join("untracked.txt").exists());
        let copied = git2::Repository::open(&workspace.repo_root).unwrap();
        let original = git2::Repository::open(caller.path()).unwrap();
        assert_ne!(copied.commondir(), original.commondir());
        assert!(!copied.path().join("objects/info/alternates").exists());
        assert_eq!(copied.head().unwrap().target(), original.head().unwrap().target());
        assert_eq!(std::fs::read(workspace.repo_root.join("source.txt")).unwrap(), b"original\n");
    }

    #[test]
    fn independent_guard_allows_foreign_edits_but_rejects_its_own_source_mutation() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let workspace = IndependentReview::create(&config).unwrap();
        let guard = CallerGuard::capture_sync(workspace.repo_root.clone(), home.path().join("diagnostics"));
        guard.protect_output_source(workspace.caller_root.clone());
        guard.verify_sync("before-checks", None).unwrap();
        std::fs::write(caller.path().join("source.txt"), "concurrent operator edit\n").unwrap();
        std::fs::write(caller.path().join("parallel.txt"), "new operator source\n").unwrap();
        guard.verify_sync("after-checks", None).unwrap();
        std::fs::write(workspace.repo_root.join("source.txt"), "check mutation\n").unwrap();
        assert!(guard.verify_sync("after-checks", None).is_err());
        assert_eq!(std::fs::read(caller.path().join("source.txt")).unwrap(), b"concurrent operator edit\n");
    }

    #[cfg(unix)]
    #[test]
    fn original_tracked_output_alias_still_refused_before_open() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let workspace = IndependentReview::create(&config).unwrap();
        let guard = CallerGuard::capture_sync(workspace.repo_root.clone(), home.path().join("diagnostics"));
        guard.protect_output_source(workspace.caller_root.clone());
        let alias = home.path().join("external-alias.txt");
        std::fs::hard_link(caller.path().join("source.txt"), &alias).unwrap();
        assert!(guard.create_output(&alias, true, |_, _| panic!("must refuse before writer opens")).is_err());
        assert_eq!(std::fs::read(&alias).unwrap(), b"original\n");
    }

    #[test]
    fn explicit_output_inside_caller_is_refused_without_allocating_it() {
        let (caller, _home, mut config) = crate::checks::caller_guard_tests::fixture();
        let output = caller.path().join("pack");
        config.output_dir = Some(output.clone());
        let error = IndependentReview::create(&config).err().expect("internal output refused");
        assert!(error.to_string().contains("inside the caller"));
        assert!(!output.exists());
    }
}
