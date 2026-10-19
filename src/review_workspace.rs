//! An owned Git repository for remote reviews, not a host filesystem sandbox.
//!
//! Only Git objects/refs cross this boundary. Caller source, dependencies,
//! worktree registrations are never copied or borrowed for writes. Only the
//! origin URL/fetch mapping is recreated, not arbitrary caller configuration.
use crate::{Config, git::git_cmd};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

tokio::task_local! {
    static SCRATCH_CALLER: PathBuf;
}

/// Carry the owned workspace's original source identity through async check
/// planning, without granting checks an owned-output write capability.
pub(crate) async fn with_caller_scratch_protection<F: std::future::Future>(
    caller: Option<PathBuf>,
    work: F,
) -> F::Output {
    match caller {
        Some(caller) => SCRATCH_CALLER.scope(caller, work).await,
        None => work.await,
    }
}

pub(crate) fn scratch_overlaps_caller(path: &Path) -> Result<bool> {
    let Ok(caller) = SCRATCH_CALLER.try_with(Clone::clone) else {
        return Ok(false);
    };
    let path = crate::checks::caller_integrity::planned_path(path)?;
    Ok(path.starts_with(&caller) || caller.starts_with(path))
}

pub(crate) struct IndependentReview {
    pub(crate) caller_root: PathBuf,
    pub(crate) repo_root: PathBuf,
    _directory: tempfile::TempDir,
}

impl IndependentReview {
    pub(crate) fn create(config: &Config) -> Result<Self> {
        let caller_root = std::fs::canonicalize(&config.repo_root)?;
        validate_write_namespaces(config, &caller_root)?;
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
        match source.find_remote("origin") {
            Ok(origin) => {
                let url = origin.url().context(
                    "remote review origin URL is not valid UTF-8; cannot preserve origin refresh",
                )?;
                destination.remote("origin", &anchored_origin_url(url, &caller_root)?)?;
                // URL alone does not define fetch semantics: a Gerrit/custom
                // mapping can select a different commit from the same-named head.
                // Replace the generated default with the exact ordered data list,
                // including exclusions and an intentionally empty list. These are
                // Git-parsed refspec values, never hooks, commands or config keys.
                let mut remote_config = destination.config()?;
                remote_config.remove_multivar("remote.origin.fetch", ".*")?;
                for refspec in origin.fetch_refspecs()?.iter() {
                    remote_config.set_multivar(
                        "remote.origin.fetch",
                        "^$",
                        refspec.context("origin fetch refspec is not UTF-8")?,
                    )?;
                }
            }
            Err(error) if error.code() == git2::ErrorCode::NotFound => {}
            Err(error) => return Err(error).context("cannot inspect remote review origin"),
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
        validate_write_namespaces(original, &self.caller_root)?;
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

/// Admit the current public write namespaces before rebasing execution config.
/// Retained sessions must observe late redirects just like a fresh remote run.
fn validate_write_namespaces(config: &Config, caller_root: &Path) -> Result<()> {
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
            !physical.starts_with(caller_root),
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
                    !target.starts_with(caller_root),
                    "remote review namespace {} redirects into the caller",
                    entry.path().display()
                );
            }
        }
    }
    ensure!(
        !crate::checks::caller_integrity::planned_path(&std::env::temp_dir())?
            .starts_with(caller_root),
        "remote review temporary namespace must be outside the caller"
    );
    Ok(())
}

fn anchored_origin_url(url: &str, caller: &Path) -> Result<String> {
    // A colon before the first slash denotes a URL/helper or scp-style
    // transport. Absolute native paths retain their meaning too. Ordinary
    // relative filesystem remotes must keep the invocation checkout as base.
    let transport = url
        .find(':')
        .is_some_and(|colon| url.find('/').is_none_or(|slash| colon < slash));
    if Path::new(url).is_absolute() || transport {
        return Ok(url.to_owned());
    }
    caller
        .join(url)
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("relative origin resolves to a non-UTF-8 path"))
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
            "--update-shallow",
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
    // Use committed metadata, never a dirty/half-written caller .gitmodules.
    // Names and paths differ legitimately; old-form modules keep their ODB in
    // the checkout while absorbed/historical modules keep it administratively.
    for (name, relative) in committed_module_paths(source, destination)? {
        let active = source
            .workdir()
            .and_then(|root| git2::Repository::open(root.join(&relative)).ok());
        let store = active.or_else(|| {
            git2::Repository::open_ext(
                source.commondir().join("modules").join(&name),
                git2::RepositoryOpenFlags::NO_SEARCH | git2::RepositoryOpenFlags::BARE,
                &[] as &[&std::ffi::OsStr],
            )
            .ok()
        });
        if let Some(store) = store {
            let copied = git2::Repository::init_bare(
                destination.commondir().join("modules").join(relative),
            )?;
            copy_module_objects(&store, &copied)?;
            copy_modules(&store, &copied, depth + 1)?;
        }
    }
    let modules = source.commondir().join("modules");
    if modules.exists() {
        copy_module_namespace(&modules, &destination.commondir().join("modules"), depth)?;
    }
    Ok(())
}

fn committed_module_paths(
    source: &git2::Repository,
    destination: &git2::Repository,
) -> Result<Vec<(String, String)>> {
    use std::io::Write;
    let Ok(tree) = source.head().and_then(|head| head.peel_to_tree()) else {
        return Ok(Vec::new());
    };
    let entry = match tree.get_path(Path::new(".gitmodules")) {
        Ok(entry) => entry,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        entry.filemode() == 0o100644 || entry.filemode() == 0o100755,
        "committed .gitmodules is not a regular file"
    );
    let blob = source.find_blob(entry.id())?;
    let mut file = tempfile::NamedTempFile::new_in(destination.path())?;
    file.write_all(blob.content())?;
    let mut command = git_cmd();
    command
        .args(["-c", "core.hooksPath=", "config", "--no-includes", "--file"])
        .arg(file.path())
        .args(["--null", "--get-regexp", r"^submodule\..*\.path$"])
        .current_dir(destination.path());
    let output = crate::proc::output_governed_with_timeout(
        command,
        "read committed module mappings",
        std::time::Duration::from_secs(120),
    )?;
    ensure!(
        output.status.success() || output.status.code() == Some(1),
        "cannot read committed module mappings: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut paths = Vec::new();
    for record in std::str::from_utf8(&output.stdout)?.split_terminator('\0') {
        let (key, path) = record
            .split_once('\n')
            .context("invalid committed module mapping")?;
        let name = key
            .strip_prefix("submodule.")
            .and_then(|key| key.strip_suffix(".path"))
            .context("invalid committed module name")?;
        crate::paths::validate_repo_relative_str(name)?;
        crate::paths::validate_repo_relative_str(path)?;
        paths.push((name.to_owned(), path.to_owned()));
    }
    Ok(paths)
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
    fn malformed_ambient_gitmodules_does_not_override_committed_module_mapping() {
        assert_module_transport("malformed");
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
        if kind == "named" || kind == "malformed" {
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
        if kind == "malformed" {
            std::fs::write(caller.path().join(".gitmodules"), "[submodule \"unfinished").unwrap();
        }
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
    fn origin_transport_addresses_and_absolute_paths_keep_their_meaning() {
        let caller = std::env::temp_dir().join("caller");
        for url in [
            "https://example.invalid/repo.git",
            "ssh://example.invalid/repo.git",
            "git@example.invalid:repo.git",
            "file:///tmp/repo.git",
            "ext::git custom-helper",
        ] {
            assert_eq!(anchored_origin_url(url, &caller).unwrap(), url);
        }
        let absolute = caller.join("upstream.git");
        assert_eq!(
            anchored_origin_url(absolute.to_str().unwrap(), &caller).unwrap(),
            absolute.to_str().unwrap()
        );
        assert_eq!(
            anchored_origin_url("./path:with-colon", &caller).unwrap(),
            caller.join("./path:with-colon").to_str().unwrap()
        );
    }

    #[test]
    fn shallow_caller_retains_head_and_its_shallow_boundary() {
        let (source, home, mut config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let repo = git2::Repository::open(source.path()).unwrap();
        let tree = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let tip = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "shallow tip",
                &tree,
                &[&parent],
            )
            .unwrap();
        let shallow = home.path().join("shallow");
        let url = format!("file://{}", source.path().display());
        assert!(
            git_cmd()
                .args([
                    "-c",
                    "core.hooksPath=",
                    "clone",
                    "--quiet",
                    "--depth=1",
                    &url
                ])
                .arg(&shallow)
                .status()
                .unwrap()
                .success()
        );
        config.repo_root = shallow.clone();
        let original_boundary = std::fs::read(shallow.join(".git/shallow")).unwrap();
        let workspace = IndependentReview::create(&config).unwrap();
        let copied = git2::Repository::open(&workspace.repo_root).unwrap();
        assert_eq!(copied.head().unwrap().target(), Some(tip));
        assert!(copied.is_shallow());
        assert_eq!(
            std::fs::read(shallow.join(".git/shallow")).unwrap(),
            original_boundary
        );
    }

    #[test]
    fn custom_origin_fetch_mapping_keeps_the_requested_remote_commit() {
        let (caller, home, mut config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let source = git2::Repository::open(caller.path()).unwrap();
        let expected = source.head().unwrap().target().unwrap();
        let upstream_path = home.path().join("upstream.git");
        let upstream = git2::Repository::init_bare(&upstream_path).unwrap();
        assert!(
            git_cmd()
                .args(["-c", "core.hooksPath=", "fetch", "--quiet"])
                .arg(caller.path())
                .arg("+HEAD:refs/changes/review")
                .current_dir(&upstream_path)
                .status()
                .unwrap()
                .success()
        );
        let parent = upstream.find_commit(expected).unwrap();
        let signature = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
        let wrong = upstream
            .commit(
                Some("refs/heads/review"),
                &signature,
                &signature,
                "different heads mapping",
                &parent.tree().unwrap(),
                &[&parent],
            )
            .unwrap();
        assert_ne!(expected, wrong);
        upstream
            .reference("refs/changes/excluded/private", wrong, true, "fixture")
            .unwrap();
        source
            .remote("origin", upstream_path.to_str().unwrap())
            .unwrap();
        let mappings = [
            "+refs/changes/*:refs/remotes/origin/*",
            "+refs/heads/*:refs/remotes/heads/*",
            "^refs/changes/excluded/*",
        ];
        let mut source_config = source.config().unwrap();
        source_config
            .remove_multivar("remote.origin.fetch", ".*")
            .unwrap();
        for mapping in mappings {
            source_config
                .set_multivar("remote.origin.fetch", "^$", mapping)
                .unwrap();
        }
        source
            .reference("refs/remotes/origin/review", expected, true, "fixture")
            .unwrap();
        let before_config = std::fs::read(source.path().join("config")).unwrap();
        let workspace = IndependentReview::create(&config).unwrap();
        config.do_fetch = true;
        crate::git::Repository::open(&workspace.repo_root)
            .unwrap()
            .prepare_refs(&workspace.config(&config).unwrap())
            .unwrap();
        let copied = git2::Repository::open(&workspace.repo_root).unwrap();
        assert_eq!(
            copied
                .find_reference("refs/remotes/origin/review")
                .unwrap()
                .target(),
            Some(expected),
            "custom mapping must not silently select the same-named heads ref"
        );
        assert_eq!(
            copied
                .find_reference("refs/remotes/heads/review")
                .unwrap()
                .target(),
            Some(wrong)
        );
        assert!(
            copied
                .find_reference("refs/remotes/origin/excluded/private")
                .is_err()
        );
        assert_eq!(
            copied
                .find_remote("origin")
                .unwrap()
                .fetch_refspecs()
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            mappings.map(Some)
        );
        assert_eq!(
            std::fs::read(source.path().join("config")).unwrap(),
            before_config
        );
        assert_eq!(
            source
                .find_reference("refs/remotes/origin/review")
                .unwrap()
                .target(),
            Some(expected)
        );
    }

    #[test]
    fn absent_origin_fetch_mapping_does_not_gain_a_default_mapping() {
        let (caller, home, config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let source = git2::Repository::open(caller.path()).unwrap();
        source
            .remote("origin", "https://example.invalid/repo")
            .unwrap();
        source
            .config()
            .unwrap()
            .remove_multivar("remote.origin.fetch", ".*")
            .unwrap();
        let workspace = IndependentReview::create(&config).unwrap();
        assert!(
            git2::Repository::open(&workspace.repo_root)
                .unwrap()
                .find_remote("origin")
                .unwrap()
                .fetch_refspecs()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn relative_origin_fetch_still_uses_the_invocation_repository_location() {
        let (caller, home, mut config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let upstream = home.path().join("upstream.git");
        git2::Repository::init_bare(&upstream).unwrap();
        let status = git_cmd()
            .args(["-c", "core.hooksPath=", "fetch", "--quiet"])
            .arg(caller.path())
            .arg("+HEAD:refs/heads/main")
            .current_dir(&upstream)
            .status()
            .unwrap();
        assert!(status.success());
        let relative = format!(
            "../{}/upstream.git",
            home.path().file_name().unwrap().to_str().unwrap()
        );
        git2::Repository::open(caller.path())
            .unwrap()
            .remote("origin", &relative)
            .unwrap();
        let workspace = IndependentReview::create(&config).unwrap();
        config.do_fetch = true;
        let copied_config = workspace.config(&config).unwrap();
        crate::git::Repository::open(&workspace.repo_root)
            .unwrap()
            .prepare_refs(&copied_config)
            .unwrap();
        let copied = git2::Repository::open(&workspace.repo_root).unwrap();
        assert!(copied.find_reference("refs/remotes/origin/main").is_ok());
        assert_eq!(
            git2::Repository::open(caller.path())
                .unwrap()
                .find_remote("origin")
                .unwrap()
                .url(),
            Some(relative.as_str())
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn non_utf8_origin_cannot_silently_skip_a_requested_fetch() {
        assert_non_utf8_origin_is_rejected(true).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn non_utf8_origin_has_an_explicit_setup_error_without_fetch_too() {
        assert_non_utf8_origin_is_rejected(false).await;
    }

    #[cfg(unix)]
    async fn assert_non_utf8_origin_is_rejected(fetch: bool) {
        use std::os::unix::ffi::OsStrExt;
        let (caller, home, mut config) = crate::checks::caller_guard_tests::fixture();
        let _home = crate::config::override_test_prview_home(home.path());
        let source = git2::Repository::open(caller.path()).unwrap();
        source.remote("origin", "placeholder").unwrap();
        let url = b"/definitely-unavailable/prview-origin-\xff.git";
        assert!(
            git_cmd()
                .args(["config", "--local", "remote.origin.url"])
                .arg(std::ffi::OsStr::from_bytes(url))
                .current_dir(caller.path())
                .status()
                .unwrap()
                .success()
        );
        let origin = source.find_remote("origin").unwrap();
        assert!(origin.url().is_none());
        assert_eq!(origin.url_bytes(), url);
        let head = source.head().unwrap().target().unwrap();
        source
            .reference("refs/remotes/origin/main", head, false, "stale fixture ref")
            .unwrap();
        // This control does not require an invalid-byte filename on APFS. The
        // native fetch must fail for this unavailable byte-valued URL; setup
        // must not turn it into a successful review of the stale copied ref.
        let native = git_cmd()
            .args(["fetch", "--quiet", "origin"])
            .current_dir(caller.path())
            .output()
            .unwrap();
        assert!(!native.status.success());
        let original_config = std::fs::read(source.path().join("config")).unwrap();
        config.remote_mode = true;
        config.target = Some("main".to_owned());
        config.bases = vec!["main".to_owned()];
        config.do_fetch = fetch;
        config.run_tests = false;
        config.run_lint = false;
        config.run_heuristics = false;
        config.skip_security = true;
        config.create_zip = false;
        let output = home.path().join("pack");
        config.output_dir = Some(output.clone());
        let result = crate::App::from_config(config).unwrap().run().await;
        let error = result
            .err()
            .expect("a byte-valued origin must not disappear and certify a stale copied target");
        assert!(
            format!("{error:#}").contains("origin URL is not valid UTF-8"),
            "explicit origin capability error: {error:#}"
        );
        assert!(
            !output.exists(),
            "unsupported origin must fail before publishing a pack"
        );
        assert_eq!(
            std::fs::read(source.path().join("config")).unwrap(),
            original_config
        );
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
