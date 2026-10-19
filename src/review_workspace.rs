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
        let home = crate::config::prview_home();
        let namespaces = [
            config.cache_dir(),
            home.join("runs").join(config.repo_name()),
            home.join("prune-trash"),
            home.join("diagnostics"),
            config.cargo_build_cache_dir(),
            config.uv_env_root(),
        ];
        for path in std::iter::once(home)
            .chain(config.output_dir.clone())
            .chain(namespaces.iter().cloned())
        {
            let physical = crate::checks::caller_integrity::planned_path(&path)?;
            ensure!(
                !physical.starts_with(&caller_root),
                "remote review write path {} is inside the caller; choose an external home/output/cache",
                path.display()
            );
        }
        // Inspect actual writer namespaces, not all of PRVIEW_HOME. A nested
        // redirect is as important as the configured root itself. Concrete
        // writer/maintenance admission rechecks the source roots for late swaps.
        for root in namespaces.into_iter().chain(config.output_dir.clone()) {
            if !root.exists() {
                continue;
            }
            for entry in walkdir::WalkDir::new(&root).follow_links(false) {
                let entry = entry?;
                if entry.file_type().is_symlink() {
                    let target = std::fs::canonicalize(entry.path())?;
                    ensure!(
                        !target.starts_with(&caller_root),
                        "remote review namespace {} redirects into the caller",
                        entry.path().display()
                    );
                }
            }
        }
        ensure!(
            !crate::checks::caller_integrity::planned_path(&std::env::temp_dir())?
                .starts_with(&caller_root),
            "remote review temporary namespace must be outside the caller"
        );
        let directory = tempfile::Builder::new()
            .prefix("prview-review-")
            .tempdir()?;
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
            ensure!(
                destination.find_commit(oid).is_ok(),
                "caller HEAD changed during object copy"
            );
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
        Ok(Self {
            caller_root,
            repo_root,
            _directory: directory,
        })
    }

    pub(crate) fn config(&self, original: &Config) -> Result<Config> {
        let mut config = original.clone();
        config.repo_root = self.repo_root.clone();
        config.repo_path = Some(self.repo_root.clone());
        config.scan_dir_override = None;
        // Programmatic profiles can contain absolute paths even when automatic
        // refresh is disabled. Never leave those pointing at the caller.
        let rebase = |path: &Path| -> Result<PathBuf> {
            Ok(self.repo_root.join(
                path.strip_prefix(&original.repo_root)
                    .or_else(|_| path.strip_prefix(&self.caller_root))
                    .context("profile path is outside remote review caller")?,
            ))
        };
        let rebase_profile = |profile: &mut crate::config::DetectedProfile| -> Result<()> {
            if let Some(path) = &profile.cargo_root {
                profile.cargo_root = Some(rebase(path)?);
            }
            profile.rust_dirs = profile
                .rust_dirs
                .iter()
                .map(|path| rebase(path))
                .collect::<Result<_>>()?;
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
    command
        .args([
            "-c",
            "core.hooksPath=",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "--update-head-ok",
        ])
        .arg(source.path())
        .arg("+refs/*:refs/*");
    if source.head().is_ok() {
        command.arg("+HEAD:refs/prview/copied-head");
    }
    command.current_dir(destination.path());
    let output = crate::proc::output_governed_with_timeout(
        command,
        "copy remote review objects",
        std::time::Duration::from_secs(120),
    )?;
    ensure!(
        output.status.success(),
        "cannot copy remote review objects: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn copy_modules(
    source: &git2::Repository,
    destination: &git2::Repository,
    depth: usize,
) -> Result<()> {
    ensure!(
        depth < 16,
        "remote review submodule object nesting limit exceeded"
    );
    // Normalize active modules by gitlink path (their configured name may be
    // different), including old-form checkouts with an embedded .git directory.
    if source.workdir().is_some() {
        for module in source.submodules()? {
            let relative = module.path();
            crate::paths::validate_repo_relative_str(
                relative
                    .to_str()
                    .context("non-UTF-8 remote review submodule path")?,
            )?;
            if let Ok(store) = module.open() {
                let copied = git2::Repository::init_bare(
                    destination.commondir().join("modules").join(relative),
                )?;
                copy_module_objects(&store, &copied)?;
                copy_modules(&store, &copied, depth + 1)?;
            }
        }
    }
    let modules = source.commondir().join("modules");
    if modules.exists() {
        copy_module_namespace(&modules, &destination.commondir().join("modules"), depth)?;
    }
    Ok(())
}

fn copy_module_namespace(source: &Path, destination: &Path, depth: usize) -> Result<()> {
    // A historical pin may outlive .gitmodules, its index entry and checkout.
    // The Git administrative stores, rather than the current module list, are
    // the object source. Multi-component module names form directory groups.
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let target = destination.join(entry.file_name());
        if path.join("HEAD").is_file() && path.join("objects").is_dir() {
            let module = git2::Repository::open_ext(
                &path,
                git2::RepositoryOpenFlags::NO_SEARCH | git2::RepositoryOpenFlags::BARE,
                &[] as &[&std::ffi::OsStr],
            )?;
            let copied = git2::Repository::init_bare(&target)?;
            copy_module_objects(&module, &copied)?;
            copy_modules(&module, &copied, depth + 1)?;
        } else {
            ensure!(
                !entry.file_type()?.is_symlink(),
                "unresolved submodule namespace link"
            );
            copy_module_namespace(&path, &target, depth)?;
        }
    }
    Ok(())
}

/// Copy module ODB bytes, including available pins no longer reachable from
/// module refs. Git upload-pack also follows stale core.worktree configuration
/// in retained stores; opening their ODB directly avoids that dependency.
/// Neither alternates nor repository config cross into the owned destination.
fn copy_module_objects(source: &git2::Repository, destination: &git2::Repository) -> Result<()> {
    let source = source.odb()?;
    let destination = destination.odb()?;
    let governor = crate::governor::current_run_governor();
    let started = std::time::Instant::now();
    let mut failure = None;
    let enumerated = source.foreach(|oid| {
        let result = (|| -> Result<()> {
            if let Some(governor) = &governor
                && governor.is_cancelled()
            {
                return Err(governor.cancellation_error());
            }
            ensure!(
                started.elapsed() < std::time::Duration::from_secs(120),
                "remote review module object copy timed out"
            );
            if !destination.exists(*oid) {
                let object = source.read(*oid)?;
                ensure!(
                    destination.write(object.kind(), object.data())? == *oid,
                    "copied module object identity mismatch"
                );
            }
            Ok(())
        })();
        if let Err(error) = result {
            failure = Some(error);
            false
        } else {
            true
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    enumerated?;
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
        assert_eq!(
            copied.head().unwrap().target(),
            original.head().unwrap().target()
        );
        assert_eq!(
            std::fs::read(workspace.repo_root.join("source.txt")).unwrap(),
            b"original\n"
        );
    }

    #[test]
    fn independent_guard_allows_foreign_edits_but_rejects_its_own_source_mutation() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let workspace = IndependentReview::create(&config).unwrap();
        let guard =
            CallerGuard::capture_sync(workspace.repo_root.clone(), home.path().join("diagnostics"));
        guard.protect_output_source(workspace.caller_root.clone());
        guard.verify_sync("before-checks", None).unwrap();
        std::fs::write(
            caller.path().join("source.txt"),
            "concurrent operator edit\n",
        )
        .unwrap();
        std::fs::write(caller.path().join("parallel.txt"), "new operator source\n").unwrap();
        guard.verify_sync("after-checks", None).unwrap();
        std::fs::write(workspace.repo_root.join("source.txt"), "check mutation\n").unwrap();
        assert!(guard.verify_sync("after-checks", None).is_err());
        assert_eq!(
            std::fs::read(caller.path().join("source.txt")).unwrap(),
            b"concurrent operator edit\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn original_tracked_output_alias_still_refused_before_open() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let workspace = IndependentReview::create(&config).unwrap();
        let guard =
            CallerGuard::capture_sync(workspace.repo_root.clone(), home.path().join("diagnostics"));
        guard.protect_output_source(workspace.caller_root.clone());
        let alias = home.path().join("external-alias.txt");
        std::fs::hard_link(caller.path().join("source.txt"), &alias).unwrap();
        assert!(
            guard
                .create_output(&alias, true, |_, _| panic!(
                    "must refuse before writer opens"
                ))
                .is_err()
        );
        assert_eq!(std::fs::read(&alias).unwrap(), b"original\n");
    }

    #[cfg(unix)]
    #[test]
    fn managed_cache_redirect_into_caller_is_refused() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let cache = config.cargo_build_cache_dir();
        std::fs::create_dir_all(&cache).unwrap();
        std::os::unix::fs::symlink(caller.path(), cache.join("redirect")).unwrap();
        let error = IndependentReview::create(&config)
            .err()
            .expect("redirect refused");
        assert!(error.to_string().contains("redirects into the caller"));
        assert_eq!(
            std::fs::read(caller.path().join("source.txt")).unwrap(),
            b"original\n"
        );
    }

    #[test]
    fn initialized_gitlink_objects_are_available_without_sharing_the_module_store() {
        assert_module_transport("standard");
    }

    #[test]
    fn named_gitlink_store_is_normalized_to_the_gitlink_path() {
        assert_module_transport("named");
    }

    #[test]
    fn legacy_gitlink_with_its_own_git_directory_is_copied() {
        assert_module_transport("legacy");
    }

    #[test]
    fn gitlink_pin_unreachable_from_module_refs_is_copied() {
        assert_module_transport("unreachable");
    }

    fn assert_module_transport(kind: &str) {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let (module, _module_home, _) = crate::checks::caller_guard_tests::fixture();
        let mut command = git_cmd();
        command.args([
            "-c",
            "protocol.file.allow=always",
            "-c",
            "core.hooksPath=",
            "submodule",
            "add",
            "--quiet",
        ]);
        if kind == "named" {
            command.args(["--name", "engine"]);
        }
        let status = command
            .arg(module.path())
            .arg("backend")
            .current_dir(caller.path())
            .status()
            .unwrap();
        assert!(status.success());
        let source = git2::Repository::open(caller.path()).unwrap();
        let mut index = source.index().unwrap();
        let tree = source.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = source.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let target = source
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "add module",
                &tree,
                &[&parent],
            )
            .unwrap();
        if kind == "legacy" {
            let checkout = caller.path().join("backend");
            std::fs::remove_file(checkout.join(".git")).unwrap();
            std::fs::rename(
                source.commondir().join("modules/backend"),
                checkout.join(".git"),
            )
            .unwrap();
            git2::Config::open(&checkout.join(".git/config"))
                .unwrap()
                .remove("core.worktree")
                .unwrap();
        }
        if kind == "unreachable" {
            let module_repo = git2::Repository::open(caller.path().join("backend")).unwrap();
            let pin = module_repo.head().unwrap().peel_to_commit().unwrap().id();
            let empty_tree = module_repo
                .find_tree(module_repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let orphan = module_repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    "unrelated module history",
                    &empty_tree,
                    &[],
                )
                .unwrap();
            module_repo.set_head_detached(orphan).unwrap();
            let names: Vec<_> = module_repo
                .references()
                .unwrap()
                .map(|r| r.unwrap().name().unwrap().to_owned())
                .collect();
            for name in names {
                module_repo.find_reference(&name).unwrap().delete().unwrap();
            }
            assert!(module_repo.find_commit(pin).is_ok());
        }
        let workspace = IndependentReview::create(&config).unwrap();
        let entries = crate::git::Repository::open(&workspace.repo_root)
            .unwrap()
            .tree_entries_with_gitlinks_at_oid(&target.to_string())
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == "backend/source.txt")
        );
        assert_ne!(
            source.commondir().join("modules/backend"),
            workspace.repo_root.join(".git/modules/backend")
        );
    }

    #[test]
    fn historical_gitlink_store_is_copied_without_an_active_module() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let (module, _module_home, _) = crate::checks::caller_guard_tests::fixture();
        let status = git_cmd()
            .args([
                "-c",
                "protocol.file.allow=always",
                "-c",
                "core.hooksPath=",
                "submodule",
                "add",
                "--quiet",
            ])
            .arg(module.path())
            .arg("backend")
            .current_dir(caller.path())
            .status()
            .unwrap();
        assert!(status.success());
        let source = git2::Repository::open(caller.path()).unwrap();
        let mut index = source.index().unwrap();
        let tree = source.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = source.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let target = source
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "add module",
                &tree,
                &[&parent],
            )
            .unwrap();
        index.remove_path(Path::new("backend")).unwrap();
        index.remove_path(Path::new(".gitmodules")).unwrap();
        index.write().unwrap();
        let removed_tree = source.find_tree(index.write_tree().unwrap()).unwrap();
        let previous = source.find_commit(target).unwrap();
        source
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "remove module",
                &removed_tree,
                &[&previous],
            )
            .unwrap();
        std::fs::remove_dir_all(caller.path().join("backend")).unwrap();
        std::fs::remove_file(caller.path().join(".gitmodules")).unwrap();
        let mut local_config = source.config().unwrap();
        let _ = local_config.remove("submodule.backend.url");
        let _ = local_config.remove("submodule.backend.active");
        assert!(source.submodules().unwrap().is_empty());
        assert!(source.commondir().join("modules/backend").is_dir());
        let workspace = IndependentReview::create(&config).unwrap();
        let entries = crate::git::Repository::open(&workspace.repo_root)
            .unwrap()
            .tree_entries_with_gitlinks_at_oid(&target.to_string())
            .unwrap();
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == "backend/source.txt")
        );
        assert_ne!(
            source.commondir().join("modules/backend"),
            workspace.repo_root.join(".git/modules/backend")
        );
    }

    #[cfg(unix)]
    #[test]
    fn result_cache_namespace_redirect_into_caller_is_refused() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let cache = config.cache_dir();
        std::fs::create_dir_all(&cache).unwrap();
        std::os::unix::fs::symlink(caller.path(), cache.join("Mock")).unwrap();
        assert!(IndependentReview::create(&config).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_cleanup_does_not_delete_caller_source_after_a_late_redirect() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let victims = caller.path().join("victims");
        std::fs::create_dir(&victims).unwrap();
        let source = git2::Repository::open(caller.path()).unwrap();
        let mut index = source.index().unwrap();
        for number in 0..6 {
            let name = format!("victims/key{number}");
            std::fs::write(caller.path().join(&name), "passed").unwrap();
            index.add_path(Path::new(&name)).unwrap();
        }
        index.write().unwrap();
        let tree = source.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = source.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        source
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "tracked cache-shaped data",
                &tree,
                &[&parent],
            )
            .unwrap();
        let workspace = IndependentReview::create(&config).unwrap();
        let guard =
            CallerGuard::capture_sync(workspace.repo_root.clone(), home.path().join("diagnostics"));
        guard.protect_output_source(workspace.caller_root.clone());
        let cache_root = home.path().join("late-cache");
        std::fs::create_dir(&cache_root).unwrap();
        std::os::unix::fs::symlink(&victims, cache_root.join("Mock")).unwrap();
        let cache = crate::cache::Cache::with_dir(cache_root, true);
        let _scope = crate::owned_output::enter(guard, None);
        let result = cache.set("Mock", "new-result", "passed", None, None);
        for number in 0..6 {
            assert_eq!(
                std::fs::read(victims.join(format!("key{number}"))).unwrap(),
                b"passed"
            );
        }
        assert!(result.is_err(), "redirected cache write must be refused");
        assert!(!victims.join("new-result").exists());
    }

    #[test]
    fn alternate_objects_are_transported_without_retaining_the_alternate() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let (alternate, _alternate_home, _) = crate::checks::caller_guard_tests::fixture();
        let alternate_repo = git2::Repository::open(alternate.path()).unwrap();
        let blob = alternate_repo.blob(b"alternate-only bytes\n").unwrap();
        let mut builder = alternate_repo.treebuilder(None).unwrap();
        builder.insert("alternate.txt", blob, 0o100644).unwrap();
        let tree = alternate_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let commit = alternate_repo
            .commit(None, &signature, &signature, "alternate-only", &tree, &[])
            .unwrap();
        let source = git2::Repository::open(caller.path()).unwrap();
        let info = source.path().join("objects/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", alternate_repo.path().join("objects").display()),
        )
        .unwrap();
        source
            .reference(
                "refs/remotes/origin/alternate",
                commit,
                true,
                "fixture alternate",
            )
            .unwrap();
        let workspace = IndependentReview::create(&config).unwrap();
        let private = git2::Repository::open(&workspace.repo_root).unwrap();
        assert!(!private.path().join("objects/info/alternates").exists());
        let copied_blob = private.find_blob(blob).unwrap();
        assert_eq!(copied_blob.content(), b"alternate-only bytes\n");
        assert!(private.find_commit(commit).is_ok());
        assert!(info.join("alternates").is_file());
    }

    #[test]
    fn module_alternate_objects_are_copied_without_retaining_the_alternate() {
        let (module, _home, _) = crate::checks::caller_guard_tests::fixture();
        let (alternate, _alternate_home, _) = crate::checks::caller_guard_tests::fixture();
        let alternate_repo = git2::Repository::open(alternate.path()).unwrap();
        let blob = alternate_repo
            .blob(b"module alternate-only bytes\n")
            .unwrap();
        let source = git2::Repository::open(module.path()).unwrap();
        let info = source.path().join("objects/info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", alternate_repo.path().join("objects").display()),
        )
        .unwrap();
        // Reopen after introducing the alternate, as the production bare opener does.
        let source = git2::Repository::open(module.path()).unwrap();
        assert!(source.find_blob(blob).is_ok());
        let private = tempfile::tempdir().unwrap();
        let destination = git2::Repository::init_bare(private.path()).unwrap();
        copy_module_objects(&source, &destination).unwrap();
        assert_eq!(
            destination.find_blob(blob).unwrap().content(),
            b"module alternate-only bytes\n"
        );
        assert!(!destination.path().join("objects/info/alternates").exists());
    }

    #[test]
    fn explicit_output_inside_caller_is_refused_without_allocating_it() {
        let (caller, _home, mut config) = crate::checks::caller_guard_tests::fixture();
        let output = caller.path().join("pack");
        config.output_dir = Some(output.clone());
        let error = IndependentReview::create(&config)
            .err()
            .expect("internal output refused");
        assert!(error.to_string().contains("inside the caller"));
        assert!(!output.exists());
    }
}
