//! Run-scoped exact-leaf output accounting. External Check::run has no scope.
use crate::checks::caller_integrity::CallerGuard;
use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
struct Context {
    guard: Arc<CallerGuard>,
    pack_root: Option<PathBuf>,
}
thread_local! { static ACTIVE: RefCell<Option<Context>> = const { RefCell::new(None) }; }

pub(crate) struct Scope(Option<Context>);
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
pub(crate) fn enter(guard: Arc<CallerGuard>, pack_root: Option<PathBuf>) -> Scope {
    Scope(ACTIVE.with(|slot| slot.replace(Some(Context { guard, pack_root }))))
}
fn current() -> Option<Context> {
    ACTIVE.with(|slot| slot.borrow().clone())
}
fn allows_pack_leaf(context: &Context, path: &Path) -> bool {
    context.pack_root.as_ref().is_some_and(|root| {
        let Ok(root) = std::fs::canonicalize(root) else {
            return false;
        };
        let Some(parent) = path.parent() else {
            return false;
        };
        std::fs::canonicalize(parent).is_ok_and(|parent| parent.starts_with(root))
    })
}

pub(crate) fn create(path: &Path) -> io::Result<std::fs::File> {
    match current() {
        Some(context) => {
            context
                .guard
                .create_output(path, allows_pack_leaf(&context, path), open_output)
        }
        None => std::fs::File::create(path),
    }
}
fn open_output(path: &Path, new: bool) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(new)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0x1 | 0x2); // original producer leaf cannot be replaced while held
    }
    options.open(path)
}

/// Called by an actual typed create-new API, not by a directory scan.
pub(crate) fn create_shared_new_with(
    path: &Path,
    create: impl FnOnce(&Path) -> io::Result<std::fs::File>,
) -> io::Result<std::fs::File> {
    match current() {
        Some(context) => context.guard.create_output(path, true, |path, new| {
            if !new {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "shared create-new destination already owned",
                ));
            }
            create(path)
        }),
        None => create(path),
    }
}
pub(crate) fn create_shared_new(path: &Path) -> io::Result<std::fs::File> {
    create_shared_new_with(path, |path| open_output(path, true))
}

/// Role validation stays at the actual index/cache/latest/lock API. This
/// primitive only checks the baseline/last-own preimage and tracked collision.
pub(crate) fn admit_validated_mutable(path: &Path) -> anyhow::Result<()> {
    if let Some(context) = current() {
        context.guard.admit_mutable_output(path)?;
    }
    Ok(())
}
pub(crate) fn is_owned(path: &Path) -> bool {
    current().is_some_and(|context| context.guard.owns_output(path))
}
#[cfg(unix)]
pub(crate) fn is_mutable_owned(path: &Path) -> bool {
    current().is_some_and(|context| context.guard.owns_mutable_output(path))
}
pub(crate) fn active_guard() -> Option<Arc<CallerGuard>> {
    current().map(|context| context.guard)
}

pub(crate) fn write_locked_publication_file(
    path: &Path,
    file: &Arc<std::sync::Mutex<std::fs::File>>,
    previous: &[u8],
    bytes: &[u8],
) -> anyhow::Result<()> {
    if let Some(context) = current() {
        context
            .guard
            .write_locked_publication_file(path, file, previous, bytes)?;
    } else {
        use std::io::{Seek, Write};
        let mut file = file
            .lock()
            .map_err(|_| io::Error::other("poisoned publication handle"))?;
        file.rewind()?;
        file.set_len(0)?;
        file.write_all(bytes)?;
        file.flush()?;
    }
    Ok(())
}
pub(crate) fn finish_producer(path: &Path, file: &mut std::fs::File) -> io::Result<()> {
    // Nonguarded callers can supply write-only fixture files; never read them.
    if let Some(context) = current() {
        context
            .guard
            .finish_output_producer(path, file)
            .map_err(io::Error::other)?;
    }
    Ok(())
}

/// A single external file producer must receive its exact leaf before spawn.
pub(crate) fn reserve_external_leaf(path: &Path) -> io::Result<std::fs::File> {
    let file = create(path)?;
    file.sync_all()?;
    Ok(file)
}

pub(crate) fn namespace_source_visible(path: &Path) -> anyhow::Result<bool> {
    match current() {
        Some(context) => context.guard.namespace_source_visible(path),
        None => Ok(false),
    }
}

#[cfg(unix)]
pub(crate) fn create_latest_link(path: &Path, target: &std::ffi::OsStr) -> io::Result<()> {
    match current() {
        Some(context) => context.guard.create_latest_link(path, target),
        None => std::os::unix::fs::symlink(target, path),
    }
}

pub(crate) fn remove_owned_after_abort(path: &Path) -> io::Result<()> {
    match current() {
        Some(context) => context.guard.remove_owned_after_abort(path),
        None => std::fs::remove_file(path),
    }
}
pub(crate) fn write_incomplete_after_abort(path: &Path, bytes: &[u8]) -> io::Result<()> {
    match current() {
        Some(context)
            if allows_pack_leaf(&context, path) && path.ends_with("00_summary/INCOMPLETE.json") =>
        {
            context.guard.write_incomplete_after_abort(path, bytes)
        }
        Some(_) => Err(io::Error::other("no exact incomplete-marker capability")),
        None => std::fs::write(path, bytes),
    }
}

pub(crate) mod fs {
    use super::*;
    pub use std::fs::*;

    pub fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
        let path = path.as_ref();
        match super::current() {
            Some(context) => context.guard.write_output(
                path,
                super::allows_pack_leaf(&context, path),
                contents.as_ref(),
                super::open_output,
            ),
            None => std::fs::write(path, contents),
        }
    }

    pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
        match super::current() {
            Some(context) => context.guard.remove_output(path.as_ref()),
            None => std::fs::remove_file(path),
        }
    }

    pub fn rename(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> io::Result<()> {
        match super::current() {
            Some(context) => context
                .guard
                .rename_output(source.as_ref(), destination.as_ref()),
            None => std::fs::rename(source, destination),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue69_owned_output_internal_home_keeps_foreign_siblings_observable() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join(".internal-home/runs/new-pack");
        std::fs::create_dir_all(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let own = pack.join("report.json");
        fs::write(&own, br#"{"own":"output"}"#).unwrap();
        guard.verify_sync("after-own-output", None).unwrap();
        let foreign = pack.join("user-notes.txt");
        std::fs::write(&foreign, "foreign userdata\n").unwrap();
        assert!(guard.verify_sync("after-foreign-sibling", None).is_err());
        assert!(
            guard.ensure_valid().is_err(),
            "foreign drift must remain sticky"
        );
        assert_eq!(
            std::fs::read_to_string(foreign).unwrap(),
            "foreign userdata\n"
        );
        assert_eq!(std::fs::read(own).unwrap(), br#"{"own":"output"}"#);
    }
    #[test]
    fn issue69_owned_output_mutable_foreign_edit_is_not_filtered() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let state = root.path().join("shared-state.json");
        let original = br#"{"status":"passed"}"#;
        std::fs::write(&state, original).unwrap();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let _scope = enter(Arc::clone(&guard), None);
        admit_validated_mutable(&state).unwrap();
        std::fs::write(&state, br#"{"status":"foreign"}"#).unwrap();
        let rejected = guard.verify_sync("after-external-edit", None).is_err();
        std::fs::write(&state, original).unwrap();
        assert!(
            rejected,
            "admitted mutable data must still be observed at boundaries"
        );
        assert!(
            guard.ensure_valid().is_err(),
            "restoration cannot erase first drift"
        );
    }

    #[test]
    fn issue69_owned_output_deleted_initial_userdata_cannot_be_adopted() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let pack = root.path().join("existing-parent");
        std::fs::create_dir(&pack).unwrap();
        let leaf = pack.join("report.json");
        std::fs::write(&leaf, "initial userdata\n").unwrap();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        std::fs::remove_file(&leaf).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack));
        let result = fs::write(&leaf, "own replacement\n");
        assert!(
            result.is_err(),
            "missing baseline userdata must not become fresh owned output"
        );
        assert!(
            !leaf.exists(),
            "the broker must not write before refusing provenance drift"
        );
        assert!(
            guard.ensure_valid().is_err(),
            "userdata deletion must latch before io::Error"
        );
    }

    #[test]
    fn issue69_owned_output_unavailable_root_is_sticky_after_restore() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let holder = tempfile::tempdir().unwrap();
        let moved = holder.path().join("temporarily-moved-caller");
        std::fs::rename(root.path(), &moved).unwrap();
        let failed = guard.verify_sync("unavailable-root", None).is_err();
        let sticky = guard.ensure_valid().is_err();
        std::fs::rename(&moved, root.path()).unwrap();
        assert!(
            failed,
            "the deterministic unavailable root must fail observation"
        );
        assert!(sticky, "unknown caller observation must latch immediately");
        assert!(guard.verify_sync("restored-root", None).is_err());
    }
    #[test]
    fn issue69_owned_output_legal_mutable_rewrite_keeps_foreign_edits_observable() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let state = root.path().join("shared-state.json");
        std::fs::write(&state, br#"{"status":"before"}"#).unwrap();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let _scope = enter(Arc::clone(&guard), None);
        admit_validated_mutable(&state).unwrap();
        let own = br#"{"status":"own rewrite"}"#;
        fs::write(&state, own).unwrap();
        guard.verify_sync("after-own-mutable-write", None).unwrap();
        std::fs::write(&state, br#"{"status":"foreign edit"}"#).unwrap();
        let rejected = guard
            .verify_sync("after-foreign-mutable-edit", None)
            .is_err();
        std::fs::write(&state, own).unwrap();
        assert!(rejected);
        assert!(guard.ensure_valid().is_err());
        assert_eq!(std::fs::read(state).unwrap(), own);
    }
    #[test]
    fn issue69_owned_output_rename_cannot_adopt_deleted_initial_userdata() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let pack = root.path().join("existing-parent");
        std::fs::create_dir(&pack).unwrap();
        let destination = pack.join("report.json");
        std::fs::write(&destination, "initial userdata\n").unwrap();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        std::fs::remove_file(&destination).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let staged = pack.join("own-stage.json");
        fs::write(&staged, "own replacement\n").unwrap();
        let result = fs::rename(&staged, &destination);
        assert!(
            result.is_err(),
            "rename must not adopt missing baseline userdata"
        );
        assert!(
            !destination.exists(),
            "rename must refuse before replacing userdata provenance"
        );
        assert!(staged.exists());
        assert!(guard.ensure_valid().is_err());
    }

    #[test]
    fn issue69_owned_output_rename_foreign_collision_stays_sticky() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join("new-pack");
        std::fs::create_dir(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let staged = pack.join("own-stage.json");
        fs::write(&staged, "own replacement\n").unwrap();
        let foreign = pack.join("user-data.json");
        std::fs::write(&foreign, "foreign userdata\n").unwrap();
        let result = fs::rename(&staged, &foreign);
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            "foreign userdata\n"
        );
        std::fs::remove_file(&foreign).unwrap();
        assert!(
            guard.ensure_valid().is_err(),
            "observed foreign collision must latch before returning io::Error"
        );
        assert!(guard.verify_sync("after-collision-removed", None).is_err());
    }

    #[test]
    fn issue69_owned_output_remove_foreign_collision_stays_sticky() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join("new-pack");
        std::fs::create_dir(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let foreign = pack.join("user-data.json");
        std::fs::write(&foreign, "foreign userdata\n").unwrap();
        let result = fs::remove_file(&foreign);
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&foreign).unwrap(),
            "foreign userdata\n"
        );
        std::fs::remove_file(&foreign).unwrap();
        assert!(
            guard.ensure_valid().is_err(),
            "observed foreign collision must latch before returning io::Error"
        );
        assert!(guard.verify_sync("after-collision-removed", None).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn issue69_owned_output_dangling_foreign_symlink_removal_stays_sticky() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join("new-pack");
        std::fs::create_dir(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let foreign = pack.join("user-link");
        std::os::unix::fs::symlink("missing-target", &foreign).unwrap();
        let result = fs::remove_file(&foreign);
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_link(&foreign).unwrap(),
            Path::new("missing-target")
        );
        std::fs::remove_file(&foreign).unwrap();
        assert!(guard.ensure_valid().is_err());
        assert!(
            guard
                .verify_sync("after-foreign-symlink-removed", None)
                .is_err()
        );
    }

    #[test]
    fn issue69_owned_output_external_removal_with_unavailable_root_stays_sticky() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let external = tempfile::tempdir().unwrap();
        let _scope = enter(Arc::clone(&guard), Some(external.path().to_path_buf()));
        let own = external.path().join("own-output.txt");
        fs::write(&own, "own output\n").unwrap();
        let moved_container = tempfile::tempdir().unwrap();
        let moved = moved_container.path().join("moved-caller");
        std::fs::rename(root.path(), &moved).unwrap();
        let result = fs::remove_file(&own);
        std::fs::rename(&moved, root.path()).unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&own).unwrap(), "own output\n");
        assert!(guard.ensure_valid().is_err());
        assert!(guard.verify_sync("after-restored-caller", None).is_err());
    }
    #[test]
    fn issue69_owned_output_external_mutable_readmission_keeps_last_own_preimage() {
        let (_root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let state = diagnostics.path().join("external-shared-state.json");
        std::fs::write(&state, br#"{"status":"initial"}"#).unwrap();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let _scope = enter(Arc::clone(&guard), None);
        admit_validated_mutable(&state).unwrap();
        let foreign = br#"{"status":"foreign"}"#;
        std::fs::write(&state, foreign).unwrap();
        let readmission = admit_validated_mutable(&state);
        assert!(
            readmission.is_err(),
            "an existing external role must not rebase its last-own preimage"
        );
        assert_eq!(std::fs::read(&state).unwrap(), foreign);
        std::fs::write(&state, br#"{"status":"initial"}"#).unwrap();
        assert!(guard.ensure_valid().is_err());
        assert!(
            guard
                .verify_sync("after-external-state-restored", None)
                .is_err()
        );
    }
    #[test]
    fn issue72_review_completed_output_foreign_edits_are_protected() {
        for action in ["verify", "rewrite", "rename", "remove", "abort-remove"] {
            let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
            let guard =
                CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
            let pack = root.path().join("pack");
            std::fs::create_dir(&pack).unwrap();
            let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
            let leaf = pack.join("report.json");
            fs::write(&leaf, b"own complete output").unwrap();
            guard.verify_sync("own-output-completed", None).unwrap();
            std::fs::write(&leaf, b"foreign edit").unwrap();
            let rejected = match action {
                "verify" => guard.verify_sync("foreign-completed-output", None).is_err(),
                "rewrite" => fs::write(&leaf, b"overwrite").is_err(),
                "rename" => fs::rename(&leaf, pack.join("moved.json")).is_err(),
                "remove" => fs::remove_file(&leaf).is_err(),
                "abort-remove" => {
                    std::fs::write(root.path().join("source.txt"), b"foreign source").unwrap();
                    assert!(guard.verify_sync("source-drift", None).is_err());
                    remove_owned_after_abort(&leaf).is_err()
                }
                _ => unreachable!(),
            };
            assert!(
                rejected,
                "completed output foreign edit accepted by {action}"
            );
            assert_eq!(
                std::fs::read(&leaf).unwrap(),
                b"foreign edit",
                "{action} replaced foreign bytes"
            );
            assert!(guard.ensure_valid().is_err());
        }
    }

    #[test]
    fn issue72_review_existing_create_new_retains_already_exists() {
        for internal in [false, true] {
            let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
            let path = if internal {
                root.path().join("initial-user-file")
            } else {
                diagnostics.path().join("external-legacy-lock")
            };
            std::fs::write(&path, b"preexisting owner").unwrap();
            let guard =
                CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
            let _scope = enter(Arc::clone(&guard), None);
            let error = create_shared_new(&path).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(&path).unwrap(), b"preexisting owner");
            guard.ensure_valid().unwrap();
            guard
                .verify_sync("unchanged-existing-collision", None)
                .unwrap();
        }
    }

    #[test]
    fn issue72_review_guarded_cache_overwrite_preserves_admitted_destination() {
        let (_root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let cache = crate::cache::Cache::with_dir(diagnostics.path().join("cache"), true);
        let dir = diagnostics.path().join("cache/check");
        std::fs::create_dir_all(&dir).unwrap();
        for number in 0..6 {
            let path = dir.join(format!("key-{number}"));
            std::fs::write(&path, "passed\n").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(
                    std::time::SystemTime::UNIX_EPOCH
                        + std::time::Duration::from_secs(1000 + number),
                )
                .unwrap();
        }
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let _scope = enter(Arc::clone(&guard), None);
        let result = cache.set("check", "key-0", "warning", Some("new output"), None);
        assert!(
            result.is_ok(),
            "own eviction invalidated admitted destination: {result:?}"
        );
        assert_eq!(cache.get("check", "key-0").unwrap().status, "warning");
        guard
            .verify_sync("after-guarded-cache-overwrite", None)
            .unwrap();
    }
    #[test]
    fn issue72_review_followup_completed_reopen_can_start_and_finish_new_producer() {
        use std::io::Write;
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join("reopen-pack");
        std::fs::create_dir(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let leaf = pack.join("report.json");
        fs::write(&leaf, b"first completed bytes").unwrap();
        let mut file = create(&leaf).unwrap();
        file.write_all(b"second producer bytes").unwrap();
        guard
            .verify_sync("while-reopened-own-producer", None)
            .unwrap();
        finish_producer(&leaf, &mut file).unwrap();
        guard
            .verify_sync("after-reopened-own-producer", None)
            .unwrap();
        assert_eq!(std::fs::read(&leaf).unwrap(), b"second producer bytes");
        std::fs::write(&leaf, b"foreign replacement").unwrap();
        assert!(
            guard
                .verify_sync("foreign-after-producer-completed", None)
                .is_err()
        );
    }

    #[test]
    fn issue72_review_followup_owned_create_new_preserves_kind_and_completed_state() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join("create-new-pack");
        std::fs::create_dir(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let leaf = pack.join("owned-file.json");
        fs::write(&leaf, b"complete own bytes").unwrap();
        assert_eq!(
            create_shared_new(&leaf).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&leaf).unwrap(), b"complete own bytes");
        guard
            .verify_sync("after-failed-own-create-new", None)
            .unwrap();
        std::fs::write(&leaf, b"foreign edit after rejected collision").unwrap();
        assert!(
            guard
                .verify_sync("failed-create-new-must-not-reopen-producer", None)
                .is_err()
        );
    }
    #[test]
    fn issue72_review_cache_current_guard_entries_can_be_evicted() {
        let (_root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let cache = crate::cache::Cache::with_dir(diagnostics.path().join("cache"), true);
        let _scope = enter(Arc::clone(&guard), None);
        for number in 0..7 {
            cache
                .set(
                    "check",
                    &format!("current-key-{number}"),
                    "passed",
                    None,
                    None,
                )
                .unwrap();
            guard
                .verify_sync("after-current-guard-cache-entry", None)
                .unwrap();
        }
        assert!(cache.get("check", "current-key-6").is_some());
    }

    #[test]
    fn issue72_review_producer_and_absent_lifecycles_keep_foreign_resurrection_visible() {
        use std::io::Write;
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join("lifecycle-pack");
        std::fs::create_dir(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let leaf = pack.join("active.log");
        let mut file = create(&leaf).unwrap();
        file.write_all(b"first active bytes").unwrap();
        guard
            .verify_sync("while-own-producer-active", None)
            .unwrap();
        file.write_all(b" and more").unwrap();
        finish_producer(&leaf, &mut file).unwrap();
        drop(file);
        guard
            .verify_sync("after-own-producer-completion", None)
            .unwrap();
        fs::remove_file(&leaf).unwrap();
        guard.verify_sync("after-owned-file-removed", None).unwrap();
        let mut recreated = create_shared_new(&leaf).unwrap();
        recreated.write_all(b"legal own recreation").unwrap();
        finish_producer(&leaf, &mut recreated).unwrap();
        drop(recreated);
        guard
            .verify_sync("after-owned-file-recreated", None)
            .unwrap();
        fs::remove_file(&leaf).unwrap();
        std::fs::write(&leaf, b"foreign resurrection").unwrap();
        assert!(
            guard
                .verify_sync("after-foreign-resurrection", None)
                .is_err()
        );
        assert_eq!(std::fs::read(&leaf).unwrap(), b"foreign resurrection");
        assert!(create_shared_new(&leaf).is_err());
        assert_eq!(std::fs::read(&leaf).unwrap(), b"foreign resurrection");
    }

    #[cfg(windows)]
    #[test]
    fn issue72_review_windows_producer_handle_prevents_leaf_replacement() {
        use std::io::Write;
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let pack = root.path().join("producer-pack");
        std::fs::create_dir(&pack).unwrap();
        let _scope = enter(Arc::clone(&guard), Some(pack.clone()));
        let leaf = pack.join("active.log");
        let replacement = pack.join("replacement.log");
        fs::write(&replacement, b"other own leaf").unwrap();
        let mut file = create(&leaf).unwrap();
        file.write_all(b"original handle bytes").unwrap();
        assert!(std::fs::remove_file(&leaf).is_err());
        assert!(std::fs::rename(&replacement, &leaf).is_err());
        guard
            .verify_sync("while-windows-producer-active", None)
            .unwrap();
        finish_producer(&leaf, &mut file).unwrap();
        drop(file);
        fs::remove_file(&leaf).unwrap();
        guard
            .verify_sync("after-windows-producer-close-and-unlink", None)
            .unwrap();
    }
    #[test]
    fn issue72_review_existing_tracked_cache_alias_is_refused_before_write() {
        let (root, diagnostics, config) = crate::checks::caller_guard_tests::fixture();
        let cache_root = root.path().join(".internal-cache");
        let dir = cache_root.join("check");
        std::fs::create_dir_all(&dir).unwrap();
        let tracked = dir.join("CACHEKEY");
        let requested = dir.join("cachekey");
        std::fs::write(&tracked, b"passed\n").unwrap();
        std::fs::write(
            root.path().join(".gitignore"),
            ".generated/\n.internal-cache/\n",
        )
        .unwrap();
        let repo = git2::Repository::open(root.path()).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new(".gitignore")).unwrap();
        index
            .add_path(std::path::Path::new(".internal-cache/check/CACHEKEY"))
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "tracked cache fixture",
            &tree,
            &[&parent],
        )
        .unwrap();
        let aliases_existing = requested.exists();
        eprintln!("tracked cache fixture: case_insensitive_existing_alias={aliases_existing}");
        let guard =
            CallerGuard::capture_sync(config.repo_root, diagnostics.path().join("diagnostics"));
        let _scope = enter(Arc::clone(&guard), None);
        let cache = crate::cache::Cache::with_dir(cache_root, true);
        let result = cache.set(
            "check",
            "cachekey",
            "warning",
            Some("own cache output"),
            None,
        );
        if aliases_existing {
            assert!(
                result.is_err(),
                "existing tracked alias must be refused before writing"
            );
        } else {
            result.expect("distinct case-sensitive untracked leaf remains allowed");
            assert_eq!(cache.get("check", "cachekey").unwrap().status, "warning");
        }
        assert_eq!(
            std::fs::read(&tracked).unwrap(),
            b"passed\n",
            "tracked bytes must never be overwritten"
        );
        guard
            .verify_sync("after-tracked-cache-alias-refusal", None)
            .unwrap();
        cache
            .set("check", "unrelated-own-key", "passed", None, None)
            .unwrap();
        guard
            .verify_sync("after-unrelated-own-cache-write", None)
            .unwrap();
    }
}
