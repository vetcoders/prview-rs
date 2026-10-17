//! Strict caller boundary observations. No file restoration or continuous watcher.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SourceEntry {
    path: String,
    kind: &'static str,
    mode: u32,
    sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct CallerToken {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    head_ref: Option<String>,
    head_oid: Option<String>,
    raw_head_sha256: Option<String>,
    raw_index_sha256: Option<String>,
    sources: BTreeMap<String, SourceEntry>,
}

/// A fatal dispatch error, distinct from operator/deadline cancellation.
#[derive(Clone, Debug)]
pub(crate) struct CallerDriftError {
    detail: String,
    pub(crate) receipt: Option<PathBuf>,
}
impl std::fmt::Display for CallerDriftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "caller drift: {}", self.detail)?;
        if let Some(path) = &self.receipt {
            write!(f, "; diagnostic: {}", path.display())?;
        }
        Ok(())
    }
}
impl std::error::Error for CallerDriftError {}

#[derive(Serialize)]
struct DriftReceipt<'a> {
    schema_version: u8,
    reason: &'static str,
    phase: &'a str,
    check: Option<&'a str>,
    caller_root: &'a Path,
    expected: Option<&'a CallerToken>,
    observed: Option<&'a CallerToken>,
    observation_error: Option<&'a str>,
    changed_paths: Vec<&'a str>,
}

pub(crate) struct CallerGuard {
    root: PathBuf,
    diagnostics: PathBuf,
    baseline: std::result::Result<CallerToken, String>,
    first: Mutex<Option<CallerDriftError>>,
    observation_gate: Mutex<()>,
    owned_outputs: Mutex<BTreeMap<PathBuf, OwnedLeaf>>,
}

#[derive(Clone)]
struct OwnedLeaf {
    mutable: bool,
    expected: Option<SourceEntry>,
}

impl CallerGuard {
    pub(crate) async fn capture(root: PathBuf, diagnostics: PathBuf) -> Arc<Self> {
        let capture_root = root.clone();
        let baseline = tokio::task::spawn_blocking(move || {
            capture_token(&capture_root, &BTreeSet::new()).map_err(|error| format!("{error:#}"))
        })
        .await
        .unwrap_or_else(|error| Err(format!("caller observation worker failed: {error}")));
        Self::from_observation(root, diagnostics, baseline)
    }

    pub(crate) fn capture_sync(root: PathBuf, diagnostics: PathBuf) -> Arc<Self> {
        let baseline = capture_token(&root, &BTreeSet::new()).map_err(|error| format!("{error:#}"));
        Self::from_observation(root, diagnostics, baseline)
    }

    fn from_observation(
        root: PathBuf,
        diagnostics: PathBuf,
        baseline: std::result::Result<CallerToken, String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            root,
            diagnostics,
            baseline,
            first: Mutex::new(None),
            observation_gate: Mutex::new(()),
            owned_outputs: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn owned_keys(&self) -> Result<BTreeSet<String>> {
        let root = fs::canonicalize(&self.root)?;
        let owned = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Ok(owned
            .keys()
            .filter_map(|path| path.strip_prefix(&root).ok())
            .map(path_key)
            .collect())
    }

    /// The caller already validated a typed MCP reservation. Seed only its
    /// exact launcher leaves before a first observation can read active logs.
    pub(crate) fn seed_launcher_outputs(&self, paths: &[PathBuf]) -> Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut owned = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for path in paths {
            let path = output_leaf_path(path)?;
            ensure!(
                !self.output_is_tracked(&path)?,
                "tracked source cannot be a launcher output: {}",
                path.display()
            );
            owned.insert(
                path,
                OwnedLeaf {
                    mutable: false,
                    expected: None,
                },
            );
        }
        Ok(())
    }

    pub(crate) fn capture_sync_with_launcher(
        root: PathBuf,
        diagnostics: PathBuf,
        paths: &[PathBuf],
    ) -> Result<Arc<Self>> {
        let normalized: Vec<_> = paths
            .iter()
            .map(|p| output_leaf_path(p))
            .collect::<Result<_>>()?;
        let canonical_root = fs::canonicalize(&root)?;
        let keys = normalized
            .iter()
            .filter_map(|p| p.strip_prefix(&canonical_root).ok())
            .map(path_key)
            .collect();
        let baseline = capture_token(&root, &keys).map_err(|error| format!("{error:#}"));
        let guard = Self::from_observation(root, diagnostics, baseline);
        guard.seed_launcher_outputs(&normalized)?;
        Ok(guard)
    }

    fn output_is_tracked(&self, path: &Path) -> Result<bool> {
        let root = fs::canonicalize(&self.root)?;
        let Ok(relative) = path.strip_prefix(&root) else {
            return Ok(false);
        };
        let repo = git2::Repository::open(&root)?;
        Ok(tracked_paths(&repo)?
            .iter()
            .any(|bytes| git_path(bytes).is_ok_and(|p| p == relative)))
    }

    pub(crate) fn owns_output(&self, path: &Path) -> bool {
        output_leaf_path(path).is_ok_and(|p| {
            self.owned_outputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&p)
        })
    }

    pub(crate) fn owns_mutable_output(&self, path: &Path) -> bool {
        output_leaf_path(path).is_ok_and(|path| {
            self.owned_outputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&path)
                .is_some_and(|leaf| leaf.mutable)
        })
    }

    /// New output admission and exclusive open are serialized with capture.
    /// The capability is supplied by a pack leaf or a typed shared writer.
    pub(crate) fn create_output(
        &self,
        path: &Path,
        allow_new: bool,
        open: impl FnOnce(&Path, bool) -> std::io::Result<File>,
    ) -> std::io::Result<File> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.create_output_locked(path, allow_new, open)
    }

    fn create_output_locked(
        &self,
        path: &Path,
        allow_new: bool,
        open: impl FnOnce(&Path, bool) -> std::io::Result<File>,
    ) -> std::io::Result<File> {
        self.ensure_valid().map_err(std::io::Error::other)?;
        let path = output_leaf_path(path).map_err(|error| {
            std::io::Error::other(self.latch_output_unknown("output-path-resolution", error))
        })?;
        if self.output_is_tracked(&path).map_err(|error| {
            std::io::Error::other(self.latch_output_unknown("output-tracked-observation", error))
        })? {
            return Err(std::io::Error::other(format!(
                "refusing tracked output collision: {}",
                path.display()
            )));
        }
        let mut owned = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let known = owned.contains_key(&path);
        if !known
            && self.baseline_contains_output(&path).map_err(|error| {
                std::io::Error::other(self.latch_output_unknown("new-output-baseline", error))
            })?
            && fs::symlink_metadata(&path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            return Err(std::io::Error::other(self.latch_output_unknown(
                "new-output-baseline",
                "initial caller userdata disappeared before output creation",
            )));
        }
        if let Some(leaf) = owned.get(&path) {
            self.check_mutable_preimage(&path, leaf)
                .map_err(std::io::Error::other)?;
        }
        if !known && fs::symlink_metadata(&path).is_ok() {
            drop(owned);
            self.reject_new_foreign_collision(&path)
                .map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(format!(
                "unowned output collision: {}",
                path.display()
            )));
        }
        if !known && !allow_new {
            return Err(std::io::Error::other(format!(
                "no exact output capability for {}",
                path.display()
            )));
        }
        let mut file = open(&path, !known)?;
        if known {
            if let Some(leaf) = owned.get(&path)
                && leaf.mutable
            {
                let expected = leaf
                    .expected
                    .as_ref()
                    .ok_or_else(|| std::io::Error::other("mutable file preimage is absent"))?;
                let observed = hash_file(file.try_clone()?).map_err(|error| {
                    std::io::Error::other(self.latch_output_unknown("output-handle-read", error))
                })?;
                if expected.sha256.as_ref() != Some(&observed.0) || expected.mode != observed.1 {
                    return Err(std::io::Error::other(self.latch_output_unknown(
                        "output-handle-preimage",
                        "opened mutable handle differs from expected preimage",
                    )));
                }
            }
            let metadata = file.metadata()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let at_path = fs::symlink_metadata(&path)?;
                if metadata.nlink() != 1
                    || metadata.dev() != at_path.dev()
                    || metadata.ino() != at_path.ino()
                {
                    return Err(std::io::Error::other(
                        "output handle is shared or no longer names its claimed leaf",
                    ));
                }
            }
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
        }
        owned.entry(path).or_insert(OwnedLeaf {
            mutable: false,
            expected: None,
        });
        Ok(file)
    }

    pub(crate) fn write_output(
        &self,
        path: &Path,
        allow_new: bool,
        bytes: &[u8],
        open: impl FnOnce(&Path, bool) -> std::io::Result<File>,
    ) -> std::io::Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut file = self.create_output_locked(path, allow_new, open)?;
        let controlled_mode = mode(&file.metadata()?);
        file.write_all(bytes)?;
        // Trust the writer's supplied bytes, never a fresh pathname postimage.
        let path = output_leaf_path(path).map_err(|error| {
            std::io::Error::other(self.latch_output_unknown("output-path-resolution", error))
        })?;
        let expected = SourceEntry {
            path: output_entry_name(&self.root, &path).map_err(std::io::Error::other)?,
            kind: "file",
            mode: controlled_mode,
            sha256: Some(hex(&Sha256::digest(bytes))),
        };
        if let Some(leaf) = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&path)
        {
            leaf.expected = Some(expected);
        }
        Ok(())
    }

    /// Admit a validated shared-state leaf without rebasing its preimage.
    /// The format/role check belongs to the actual cache/index/latest API.
    pub(crate) fn admit_mutable_output(&self, path: &Path) -> Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ensure_valid()?;
        let path = output_leaf_path(path)
            .map_err(|error| self.latch_output_unknown("shared-output-resolution", error))?;
        ensure!(
            !self
                .output_is_tracked(&path)
                .map_err(|error| self.latch_output_unknown("shared-output-tracked", error))?,
            "tracked shared-state collision: {}",
            path.display()
        );
        let actual = output_state(&self.root, &path)
            .map_err(|error| self.latch_output_unknown("shared-output-read", error))?;
        let root = fs::canonicalize(&self.root)
            .map_err(|error| self.latch_output_unknown("output-root-observation", error))?;
        let key = path.strip_prefix(&root).ok().map(path_key);
        let mut owned = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = owned.get(&path);
        let expected = match previous {
            Some(leaf) if leaf.mutable => leaf.expected.clone(),
            Some(_) => actual.clone(), // already-created own output, not caller source
            None => key
                .as_ref()
                .and_then(|key| self.baseline.as_ref().ok()?.sources.get(key).cloned()),
        };
        // Ignored/external shared state is outside the source token. Caller
        // visible state must equal the baseline or the last own postimage.
        let visible =
            key.as_ref().is_some_and(|key| {
                self.baseline
                    .as_ref()
                    .ok()
                    .is_some_and(|b| b.sources.contains_key(key))
            }) || (key.is_some() && actual.is_some() && !git_ignored_output(&root, &path)?);
        if (visible || previous.is_some_and(|leaf| leaf.mutable)) && expected != actual {
            drop(owned);
            return self.reject_observation(
                "shared-output-preimage",
                None,
                Err(format!(
                    "shared output preimage changed: {}",
                    path.display()
                )),
            );
        }
        owned.insert(
            path,
            OwnedLeaf {
                mutable: true,
                expected: actual,
            },
        );
        Ok(())
    }

    fn latch_output_unknown(&self, phase: &str, error: impl std::fmt::Display) -> anyhow::Error {
        match self.reject_observation(phase, None, Err(error.to_string())) {
            Err(error) => error,
            Ok(()) => anyhow::anyhow!("unknown caller output observation: {error}"),
        }
    }

    fn baseline_contains_output(&self, path: &Path) -> Result<bool> {
        let root = fs::canonicalize(&self.root)
            .map_err(|error| self.latch_output_unknown("output-root-observation", error))?;
        Ok(path.strip_prefix(&root).ok().is_some_and(|relative| {
            self.baseline
                .as_ref()
                .ok()
                .is_some_and(|baseline| baseline.sources.contains_key(&path_key(relative)))
        }))
    }

    fn check_mutable_preimage(&self, path: &Path, leaf: &OwnedLeaf) -> Result<()> {
        if leaf.mutable
            && leaf.expected
                != output_state(&self.root, path)
                    .map_err(|error| self.latch_output_unknown("mutable-output-read", error))?
        {
            return self.reject_observation(
                "shared-output-preimage",
                None,
                Err(format!(
                    "registered output preimage changed: {}",
                    path.display()
                )),
            );
        }
        Ok(())
    }

    fn reject_new_foreign_collision(&self, path: &Path) -> Result<()> {
        let root = fs::canonicalize(&self.root)
            .map_err(|error| self.latch_output_unknown("output-root-observation", error))?;
        let Ok(relative) = path.strip_prefix(&root) else {
            return Ok(());
        };
        if !git_ignored_output(&root, path)?
            && self
                .baseline
                .as_ref()
                .ok()
                .is_some_and(|baseline| !baseline.sources.contains_key(&path_key(relative)))
        {
            return self.reject_observation(
                "new-output-collision",
                None,
                Err(format!(
                    "foreign leaf appeared after caller baseline: {}",
                    path.display()
                )),
            );
        }
        Ok(())
    }

    pub(crate) fn remove_output(&self, path: &Path) -> std::io::Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ensure_valid().map_err(std::io::Error::other)?;
        let path = output_leaf_path(path).map_err(|error| {
            std::io::Error::other(self.latch_output_unknown("output-path-resolution", error))
        })?;
        ensure_untracked_output(self, &path).map_err(std::io::Error::other)?;
        let mut owned = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(leaf) = owned.get(&path) {
            self.check_mutable_preimage(&path, leaf)
                .map_err(std::io::Error::other)?;
        }
        if !owned.contains_key(&path) {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(error),
                Err(error) => {
                    return Err(std::io::Error::other(
                        self.latch_output_unknown("output-removal-observation", error),
                    ));
                }
                Ok(_) => {}
            }
            drop(owned);
            self.reject_new_foreign_collision(&path)
                .map_err(std::io::Error::other)?;
            return Err(std::io::Error::other(format!(
                "refusing unowned output removal: {}",
                path.display()
            )));
        }
        fs::remove_file(&path)?;
        if let Some(leaf) = owned.get_mut(&path) {
            leaf.expected = None;
        }
        Ok(())
    }

    pub(crate) fn rename_output(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ensure_valid().map_err(std::io::Error::other)?;
        let source = output_leaf_path(source).map_err(|error| {
            std::io::Error::other(self.latch_output_unknown("output-source-resolution", error))
        })?;
        let destination = output_leaf_path(destination).map_err(|error| {
            std::io::Error::other(self.latch_output_unknown("output-destination-resolution", error))
        })?;
        if self.output_is_tracked(&destination).map_err(|error| {
            std::io::Error::other(self.latch_output_unknown("output-tracked-observation", error))
        })? {
            return Err(std::io::Error::other("tracked output rename collision"));
        }
        let mut owned = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        ensure_untracked_output(self, &source).map_err(std::io::Error::other)?;
        if let Some(leaf) = owned.get(&source) {
            self.check_mutable_preimage(&source, leaf)
                .map_err(std::io::Error::other)?;
        }
        if let Some(leaf) = owned.get(&destination) {
            self.check_mutable_preimage(&destination, leaf)
                .map_err(std::io::Error::other)?;
        }
        if !owned.contains_key(&source) {
            return Err(std::io::Error::other("unowned output rename source"));
        }
        if !owned.contains_key(&destination) {
            if self
                .baseline_contains_output(&destination)
                .map_err(|error| {
                    std::io::Error::other(
                        self.latch_output_unknown("rename-output-baseline", error),
                    )
                })?
                && fs::symlink_metadata(&destination)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                return Err(std::io::Error::other(self.latch_output_unknown(
                    "rename-output-baseline",
                    "initial caller userdata disappeared before output rename",
                )));
            }
            if fs::symlink_metadata(&destination).is_ok() {
                drop(owned);
                self.reject_new_foreign_collision(&destination)
                    .map_err(std::io::Error::other)?;
                return Err(std::io::Error::other("unowned output rename destination"));
            }
        }
        let mut intended = owned.get(&source).and_then(|leaf| leaf.expected.clone());
        if owned.get(&destination).is_some_and(|leaf| leaf.mutable) && intended.is_none() {
            return Err(std::io::Error::other(
                "shared replacement has no writer-supplied intended bytes",
            ));
        }
        if let Some(entry) = intended.as_mut() {
            entry.path =
                output_entry_name(&self.root, &destination).map_err(std::io::Error::other)?;
        }
        fs::rename(&source, &destination)?;
        let destination_leaf = owned.entry(destination.clone()).or_insert(OwnedLeaf {
            mutable: false,
            expected: None,
        });
        destination_leaf.expected = intended;
        if let Some(leaf) = owned.get_mut(&source) {
            leaf.expected = None;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn create_latest_link(
        &self,
        path: &Path,
        target: &std::ffi::OsStr,
    ) -> std::io::Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.ensure_valid().map_err(std::io::Error::other)?;
        let path = output_leaf_path(path).map_err(std::io::Error::other)?;
        ensure_untracked_output(self, &path).map_err(std::io::Error::other)?;
        if self
            .baseline_contains_output(&path)
            .map_err(std::io::Error::other)?
        {
            return Err(std::io::Error::other(
                "latest staging leaf collides with baseline data",
            ));
        }
        if fs::symlink_metadata(&path).is_ok() {
            self.reject_new_foreign_collision(&path)
                .map_err(std::io::Error::other)?;
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        std::os::unix::fs::symlink(target, &path)?;
        let entry = SourceEntry {
            path: output_entry_name(&self.root, &path).map_err(std::io::Error::other)?,
            kind: "symlink",
            mode: mode(&fs::symlink_metadata(&path)?),
            sha256: Some(hex(&Sha256::digest(target.as_encoded_bytes()))),
        };
        self.owned_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                path,
                OwnedLeaf {
                    mutable: false,
                    expected: Some(entry),
                },
            );
        Ok(())
    }

    /// Undo only this run's already-advertised latest transaction. Sticky
    /// drift remains fatal; registered alias and journal postimages must still
    /// match before any undo, so a foreign writer's state is never adopted.
    #[cfg(unix)]
    pub(crate) fn rollback_latest_after_drift(
        &self,
        out_dir: &Path,
        previous: Option<&std::ffi::OsStr>,
        journal: &Path,
    ) -> Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            self.ensure_valid().is_err(),
            "drift rollback requires an existing sticky failure"
        );
        let parent = out_dir
            .parent()
            .context("latest rollback has no publication parent")?;
        let latest = output_leaf_path(&parent.join("latest"))?;
        let journal = output_leaf_path(journal)?;
        ensure_untracked_output(self, &latest)?;
        ensure_untracked_output(self, &journal)?;
        let mut owned = self.owned_outputs.lock().unwrap_or_else(|e| e.into_inner());
        for path in [&latest, &journal] {
            let leaf = owned
                .get(path)
                .context("rollback path has no exact transaction ownership")?;
            ensure!(
                leaf.mutable,
                "rollback path is not an admitted transaction role"
            );
            self.check_mutable_preimage(path, leaf)?;
        }
        let current = fs::read_link(&latest)?;
        ensure!(
            Some(current.as_os_str()) == out_dir.file_name(),
            "latest no longer advertises this transaction; foreign alias retained"
        );
        let expected = if let Some(previous) = previous {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let staged = output_leaf_path(
                &parent.join(format!(".latest.rollback.{}.{nonce}", std::process::id())),
            )?;
            ensure_untracked_output(self, &staged)?;
            ensure!(
                !self.baseline_contains_output(&staged)?,
                "rollback stage collides with baseline userdata"
            );
            std::os::unix::fs::symlink(previous, &staged)?;
            owned.insert(
                staged.clone(),
                OwnedLeaf {
                    mutable: false,
                    expected: None,
                },
            );
            let expected = SourceEntry {
                path: output_entry_name(&self.root, &latest)?,
                kind: "symlink",
                mode: mode(&fs::symlink_metadata(&staged)?),
                sha256: Some(hex(&Sha256::digest(previous.as_encoded_bytes()))),
            };
            if let Err(error) = fs::rename(&staged, &latest) {
                let _ = fs::remove_file(&staged);
                return Err(error.into());
            }
            Some(expected)
        } else {
            fs::remove_file(&latest)?;
            None
        };
        if let Some(leaf) = owned.get_mut(&latest) {
            leaf.expected = expected;
        }
        File::open(parent)?.sync_all()?;
        fs::remove_file(&journal)?;
        if let Some(leaf) = owned.get_mut(&journal) {
            leaf.expected = None;
        }
        if let Some(parent) = journal.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    pub(crate) fn remove_owned_after_abort(&self, path: &Path) -> std::io::Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = output_leaf_path(path).map_err(std::io::Error::other)?;
        ensure_untracked_output(self, &path).map_err(std::io::Error::other)?;
        let mut owned = self.owned_outputs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(leaf) = owned.get(&path) {
            self.check_mutable_preimage(&path, leaf)
                .map_err(std::io::Error::other)?;
        }
        if !owned.contains_key(&path) {
            return match fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(e),
                _ => Err(std::io::Error::other("abort cleanup refuses unowned leaf")),
            };
        }
        fs::remove_file(&path)?;
        if let Some(leaf) = owned.get_mut(&path) {
            leaf.expected = None;
        }
        Ok(())
    }

    pub(crate) fn write_incomplete_after_abort(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let path = output_leaf_path(path).map_err(std::io::Error::other)?;
        ensure_untracked_output(self, &path).map_err(std::io::Error::other)?;
        if self
            .baseline_contains_output(&path)
            .map_err(std::io::Error::other)?
        {
            return Err(std::io::Error::other(
                "incomplete marker collides with baseline userdata",
            ));
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        self.owned_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                path,
                OwnedLeaf {
                    mutable: false,
                    expected: None,
                },
            );
        file.write_all(bytes)?;
        file.sync_all()
    }

    pub(crate) fn verify_sync(&self, phase: &'static str, check: Option<&str>) -> Result<()> {
        let _permit = self
            .observation_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.verify_blocking(phase, check)
    }

    pub(crate) fn ensure_valid(&self) -> Result<()> {
        match self
            .first
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            Some(error) => Err(error.clone().into()),
            None => Ok(()),
        }
    }

    pub(crate) async fn verify(
        self: &Arc<Self>,
        phase: &'static str,
        check: Option<&str>,
    ) -> Result<()> {
        self.verify_with(phase, check, move |guard, check| {
            guard.verify_blocking(phase, check.as_deref())
        })
        .await
    }

    async fn verify_with(
        self: &Arc<Self>,
        phase: &'static str,
        check: Option<&str>,
        observe: impl FnOnce(Arc<Self>, Option<String>) -> Result<()> + Send + 'static,
    ) -> Result<()> {
        self.ensure_valid()?;
        let guard = Arc::clone(self);
        let worker_check = check.map(str::to_owned);
        let outcome = tokio::task::spawn_blocking(move || {
            // Native reads and durable recording complete even if their async
            // waiter is dropped. No mutex is held across an await.
            let _permit = guard
                .observation_gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            observe(Arc::clone(&guard), worker_check)
        })
        .await;
        match outcome {
            Ok(result) => result,
            // Unknown observation is as terminal as a measured mismatch. A
            // later clean observation cannot erase this worker failure.
            Err(error) => self.reject_observation(
                phase,
                check,
                Err(format!("caller observation worker failed: {error}")),
            ),
        }
    }

    fn verify_blocking(&self, phase: &str, check: Option<&str>) -> Result<()> {
        self.ensure_valid()?;
        let owned = self
            .owned_keys()
            .map_err(|error| self.latch_output_unknown("owned-output-keys", error))?;
        let leaves = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        for (path, leaf) in &leaves {
            self.check_mutable_preimage(path, leaf)?;
        }
        let observed = match &self.baseline {
            Ok(_) => capture_token(&self.root, &owned).map_err(|error| format!("{error:#}")),
            Err(error) => Err(format!("initial caller observation unavailable: {error}")),
        };
        let mut expected = self.baseline.as_ref().ok().cloned();
        if let Some(expected) = expected.as_mut() {
            expected.sources.retain(|key, _| !owned.contains(key));
        }
        if expected.is_some() && expected.as_ref() == observed.as_ref().ok() {
            return Ok(());
        }
        self.reject_observation(phase, check, observed)
    }

    fn reject_observation(
        &self,
        phase: &str,
        check: Option<&str>,
        observed: std::result::Result<CallerToken, String>,
    ) -> Result<()> {
        let expected = self.baseline.as_ref().ok();
        let observed_token = observed.as_ref().ok();
        let mut changed_paths = BTreeSet::new();
        if let (Some(before), Some(after)) = (expected, observed_token) {
            for key in before.sources.keys().chain(after.sources.keys()) {
                if before.sources.get(key) != after.sources.get(key) {
                    let entry = before
                        .sources
                        .get(key)
                        .or_else(|| after.sources.get(key))
                        .unwrap();
                    changed_paths.insert(entry.path.as_str());
                }
            }
        }
        let receipt = DriftReceipt {
            schema_version: 1,
            reason: "caller-drift",
            phase,
            check,
            caller_root: &self.root,
            expected,
            observed: observed_token,
            observation_error: observed.as_ref().err().map(String::as_str),
            changed_paths: changed_paths.into_iter().collect(),
        };
        let mut first = self.first.lock().unwrap_or_else(|error| error.into_inner());
        if first.is_none() {
            let mut error = CallerDriftError {
                detail: match observed.as_ref().err() {
                    Some(reason) => format!("{phase}: {reason}"),
                    None => format!("{phase}: caller identity or Git-reviewable source changed"),
                },
                receipt: None,
            };
            match persist_receipt(&self.diagnostics, &receipt) {
                Ok(path) => error.receipt = Some(path),
                Err(failure) => error.detail.push_str(&format!(
                    "; diagnostic retention not confirmed: {failure:#}"
                )),
            }
            *first = Some(error);
        }
        Err(first.as_ref().unwrap().clone().into())
    }
}

pub(crate) fn diagnostic_parent(root: &Path, configured: PathBuf) -> PathBuf {
    let Ok(caller) = fs::canonicalize(root) else {
        return configured;
    };
    if planned_path(&configured).is_ok_and(|path| path.starts_with(&caller)) {
        let fallback = std::env::temp_dir().join("prview-caller-diagnostics");
        if planned_path(&fallback).is_ok_and(|path| !path.starts_with(&caller)) {
            return fallback;
        }
    }
    configured // persist_receipt reports honest unretained evidence if unsafe
}

fn persist_receipt(parent: &Path, receipt: &DriftReceipt<'_>) -> Result<PathBuf> {
    // A diagnostic must not itself write into the observed caller. If the
    // configured home is inside it, retain the fatal error honestly without
    // pretending that durable evidence was saved.
    let caller = fs::canonicalize(receipt.caller_root)?;
    ensure!(
        !planned_path(parent)?.starts_with(&caller),
        "caller-drift diagnostics must be outside the caller checkout"
    );
    fs::create_dir_all(parent).context("create caller-drift diagnostic root")?;
    let directory = tempfile::Builder::new()
        .prefix("caller-drift-")
        .tempdir_in(parent)?;
    let path = directory.path().join("CALLER_DRIFT.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    serde_json::to_writer_pretty(&mut file, receipt)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    #[cfg(unix)]
    File::open(directory.path())?.sync_all()?;
    let _retained = directory.keep();
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(path)
}

/// Does a planned namespace contain Git-reviewable caller data? This is a
/// writer/scratch planning query, never a global output-dir restriction.
pub(crate) fn namespace_source_visible(root: &Path, output: &Path) -> Result<bool> {
    let caller = fs::canonicalize(root)?;
    let output = planned_path(output)?;
    let Ok(relative) = output.strip_prefix(&caller) else {
        return Ok(false);
    };
    if relative.as_os_str().is_empty() {
        return Ok(true);
    }
    let repo = git2::Repository::open(&caller)?;
    for bytes in tracked_paths(&repo)? {
        let tracked = git_path(&bytes)?;
        if tracked.starts_with(relative) || relative.starts_with(&tracked) {
            return Ok(true);
        }
    }
    let mut directory = relative.as_os_str().to_os_string();
    directory.push(std::path::MAIN_SEPARATOR_STR);
    Ok(!repo.is_path_ignored(Path::new(&directory))?)
}

pub(crate) fn planned_path(path: &Path) -> Result<PathBuf> {
    let unresolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut absolute = PathBuf::new();
    for component in unresolved.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Do not guess how a missing component, or a symlink that
                // cannot be resolved, will behave when a later writer runs.
                let prefix = fs::canonicalize(&absolute).context(
                    "output parent traversal requires an existing, resolvable directory",
                )?;
                ensure!(
                    prefix.is_dir(),
                    "output parent traversal crosses a non-directory"
                );
                absolute = prefix.parent().unwrap_or(&prefix).to_path_buf();
            }
            component => absolute.push(component.as_os_str()),
        }
    }
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("output path cannot be resolved before allocation")?
                        .to_os_string(),
                );
                ancestor = ancestor
                    .parent()
                    .context("output path has no existing ancestor")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut result = fs::canonicalize(ancestor)?;
    for component in missing.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}

fn tracked_paths(repo: &git2::Repository) -> Result<BTreeSet<Vec<u8>>> {
    let index = repo.index()?;
    let mut paths = BTreeSet::new();
    for entry in index.iter() {
        ensure!(
            entry.mode != 0o160000,
            "caller observation unsupported for gitlink {:?}",
            entry.path
        );
        paths.insert(entry.path);
    }
    let head = match repo.head() {
        Ok(head) => Some(head),
        Err(error)
            if matches!(
                error.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(head) = head {
        let tree = head.peel_to_tree()?;
        let mut unsupported = false;
        tree.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
            if entry.kind() == Some(git2::ObjectType::Commit) {
                unsupported = true;
                return git2::TreeWalkResult::Skip;
            }
            if entry.kind() == Some(git2::ObjectType::Blob) {
                let mut path = prefix.as_bytes().to_vec();
                path.extend_from_slice(entry.name_bytes());
                paths.insert(path);
            }
            git2::TreeWalkResult::Ok
        })?;
        ensure!(
            !unsupported,
            "caller observation unsupported for gitlinks in HEAD"
        );
    }
    Ok(paths)
}

fn capture_token(root: &Path, owned: &BTreeSet<String>) -> Result<CallerToken> {
    let root = fs::canonicalize(root).context("resolve caller root")?;
    let repo = git2::Repository::open(&root).context("open caller repository")?;
    let workdir = repo
        .workdir()
        .context("caller repository has no worktree")?;
    ensure!(
        fs::canonicalize(workdir)? == root,
        "caller root is not its Git worktree root"
    );
    let git_dir = fs::canonicalize(repo.path())?;
    let common_dir = fs::canonicalize(repo.commondir())?;
    let raw_head_sha256 = hash_admin_file(&git_dir.join("HEAD"))?;
    let raw_index_sha256 = hash_admin_file(&git_dir.join("index"))?;
    let head = match repo.find_reference("HEAD") {
        Ok(reference) => Some(reference),
        Err(error) if error.code() == git2::ErrorCode::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let head_ref = head
        .as_ref()
        .and_then(|head| head.symbolic_target())
        .map(str::to_owned);
    let resolved = match repo.head() {
        Ok(reference) => Some(reference),
        Err(error)
            if matches!(
                error.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    let head_oid = resolved
        .as_ref()
        .and_then(|reference| reference.target())
        .map(|oid| oid.to_string());
    let tracked = tracked_paths(&repo)?;
    let mut paths = tracked.clone();
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .update_index(false);
    for entry in repo.statuses(Some(&mut options))?.iter() {
        if entry.status().contains(git2::Status::WT_NEW) {
            paths.insert(entry.path_bytes().to_vec());
        }
    }
    let mut sources = BTreeMap::new();
    for bytes in paths {
        let key = hex(&bytes);
        // Active owned raw captures must be removed before any source read.
        // Tracked HEAD/index paths remain source even if a writer claims them.
        if owned.contains(&key) && !tracked.contains(&bytes) {
            continue;
        }
        let relative = git_path(&bytes)?;
        sources.insert(key, source_entry(&root, &relative)?);
    }
    let end_head = match repo.head() {
        Ok(reference) => reference.target().map(|oid| oid.to_string()),
        Err(error)
            if matches!(
                error.code(),
                git2::ErrorCode::UnbornBranch | git2::ErrorCode::NotFound
            ) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        head_oid == end_head
            && raw_head_sha256 == hash_admin_file(&git_dir.join("HEAD"))?
            && raw_index_sha256 == hash_admin_file(&git_dir.join("index"))?,
        "caller Git identity changed during observation"
    );
    Ok(CallerToken {
        root,
        git_dir,
        common_dir,
        head_ref,
        head_oid,
        raw_head_sha256,
        raw_index_sha256,
        sources,
    })
}

fn ensure_untracked_output(guard: &CallerGuard, path: &Path) -> Result<()> {
    ensure!(
        !guard
            .output_is_tracked(path)
            .map_err(|error| guard.latch_output_unknown("output-tracked-observation", error))?,
        "tracked output collision: {}",
        path.display()
    );
    Ok(())
}

fn output_leaf_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute.parent().context("output leaf has no parent")?;
    Ok(planned_path(parent)?.join(absolute.file_name().context("output has no leaf name")?))
}

fn path_key(path: &Path) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hex(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    {
        hex(path.to_string_lossy().as_bytes())
    }
}

fn output_entry_name(root: &Path, path: &Path) -> Result<String> {
    let root = fs::canonicalize(root)?;
    let relative = path
        .strip_prefix(&root)
        .unwrap_or_else(|_| Path::new(path.file_name().unwrap_or_default()));
    Ok(format!("{relative:?}"))
}

fn output_state(root: &Path, path: &Path) -> Result<Option<SourceEntry>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let root = fs::canonicalize(root)?;
    if let Ok(relative) = path.strip_prefix(&root) {
        return Ok(Some(source_entry(&root, relative)?));
    }
    Ok(Some(source_entry(
        path.parent().context("output parent")?,
        Path::new(path.file_name().context("output name")?),
    )?))
}

fn git_ignored_output(root: &Path, path: &Path) -> Result<bool> {
    let Ok(relative) = path.strip_prefix(root) else {
        return Ok(true);
    };
    Ok(git2::Repository::open(root)?.is_path_ignored(relative)?)
}

fn git_path(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(std::str::from_utf8(bytes).context("non-UTF8 Git path unsupported")?);
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "invalid caller Git path"
    );
    Ok(path)
}

fn source_entry(root: &Path, relative: &Path) -> Result<SourceEntry> {
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SourceEntry {
                    path: format!("{relative:?}"),
                    kind: "missing",
                    mode: 0,
                    sha256: None,
                });
            }
            Err(error) => {
                return Err(error).with_context(|| format!("read caller source {relative:?}"));
            }
        };
        if metadata.file_type().is_symlink() {
            return Ok(SourceEntry {
                path: format!("{relative:?}"),
                kind: if components.peek().is_some() {
                    "ancestor-symlink"
                } else {
                    "symlink"
                },
                mode: mode(&metadata),
                sha256: Some(hex(&Sha256::digest(
                    fs::read_link(&current)?.as_os_str().as_encoded_bytes(),
                ))),
            });
        }
        if components.peek().is_some() {
            ensure!(
                metadata.is_dir(),
                "caller source ancestor is not a directory"
            );
            continue;
        }
        ensure!(
            metadata.is_file(),
            "unsupported caller source file type at {relative:?}"
        );
        let file = open_source_file(root, relative)?;
        let (sha256, observed_mode) = hash_file(file)?;
        return Ok(SourceEntry {
            path: format!("{relative:?}"),
            kind: "file",
            mode: observed_mode,
            sha256: Some(sha256),
        });
    }
    bail!("empty caller source path")
}

fn open_source_file(root: &Path, relative: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let mut parent = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let name = std::ffi::CString::new(component.as_os_str().as_bytes())?;
            let flags = libc::O_RDONLY
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK
                | if components.peek().is_some() {
                    libc::O_DIRECTORY
                } else {
                    0
                };
            // The parent handle anchors each component; a replaced directory
            // cannot redirect a later source read through an outside symlink.
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            parent = unsafe { File::from_raw_fd(fd) };
        }
        Ok(parent)
    }
    #[cfg(not(unix))]
    {
        crate::paths::open_file_within(root, relative)
    }
}

fn hash_admin_file(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file(),
        "caller Git metadata is not a regular file: {}",
        path.display()
    );
    let parent = path.parent().context("Git metadata parent missing")?;
    let name = Path::new(path.file_name().context("Git metadata filename missing")?);
    Ok(Some(hash_file(open_source_file(parent, name)?)?.0))
}

fn hash_file(mut file: File) -> Result<(String, u32)> {
    let before = file.metadata()?;
    ensure!(before.is_file(), "caller source is not a regular file");
    let digest = hash_bounded(&mut file, before.len())?;
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len()
            && before.modified()? == after.modified()?
            && mode(&before) == mode(&after),
        "caller source changed while hashing"
    );
    Ok((digest, mode(&before)))
}

fn hash_bounded(reader: &mut impl Read, initial_len: u64) -> Result<String> {
    let limit = initial_len
        .checked_add(1)
        .context("caller source size cannot be bounded")?;
    let mut bounded = reader.take(limit);
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut consumed = 0;
    loop {
        let length = bounded.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        consumed += length as u64;
        digest.update(&buffer[..length]);
    }
    ensure!(
        consumed == initial_len,
        "caller source size changed while hashing"
    );
    Ok(hex(&digest.finalize()))
}
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(DIGITS[usize::from(byte >> 4)]),
                char::from(DIGITS[usize::from(byte & 15)]),
            ]
        })
        .collect()
}
#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}
#[cfg(not(unix))]
fn mode(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::caller_guard_tests::fixture;

    #[tokio::test]
    async fn issue69_caller_guard_first_mismatch_survives_restoration() {
        let (root, home, _) = fixture();
        let guard = CallerGuard::capture(root.path().into(), home.path().join("diagnostics")).await;
        fs::write(root.path().join("source.txt"), "modified\n").unwrap();
        let first = guard.verify("after-check", Some("mock")).await.unwrap_err();
        let first = first.downcast_ref::<CallerDriftError>().unwrap();
        let receipt = first.receipt.as_ref().unwrap();
        let bytes = fs::read(receipt).unwrap();
        let evidence: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(evidence["reason"], "caller-drift");
        assert!(!evidence["changed_paths"].as_array().unwrap().is_empty());
        fs::write(root.path().join("source.txt"), "original\n").unwrap();
        let second = guard.verify("after-restore", None).await.unwrap_err();
        assert_eq!(
            second
                .downcast_ref::<CallerDriftError>()
                .unwrap()
                .receipt
                .as_ref(),
            Some(receipt)
        );
        assert_eq!(fs::read(receipt).unwrap(), bytes);
        assert_eq!(
            fs::read_dir(home.path().join("diagnostics"))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn issue69_caller_guard_worker_failure_is_sticky() {
        let (root, home, _) = fixture();
        let guard = CallerGuard::capture(root.path().into(), home.path().join("diagnostics")).await;
        let error = guard
            .verify_with("after-check", Some("mock"), |_, _| {
                panic!("synthetic observer failure")
            })
            .await
            .unwrap_err();
        let evidence = error.downcast_ref::<CallerDriftError>().unwrap();
        assert!(evidence.receipt.as_ref().unwrap().is_file());
        assert!(error.to_string().contains("worker failed"));
        assert!(guard.ensure_valid().is_err());
        assert!(guard.verify("later-clean", None).await.is_err());
    }

    #[tokio::test]
    async fn issue69_caller_guard_unreadable_initial_repository_refuses() {
        let (root, home, _) = fixture();
        let missing = root.path().join("not-a-repository");
        fs::create_dir(&missing).unwrap();
        let guard = CallerGuard::capture(missing, home.path().join("diagnostics")).await;
        let error = guard.verify("before-checks", None).await.unwrap_err();
        let receipt = error
            .downcast_ref::<CallerDriftError>()
            .unwrap()
            .receipt
            .as_ref()
            .unwrap();
        let evidence: serde_json::Value =
            serde_json::from_slice(&fs::read(receipt).unwrap()).unwrap();
        assert!(evidence["expected"].is_null());
        assert!(
            evidence["observation_error"]
                .as_str()
                .unwrap()
                .contains("initial caller observation unavailable")
        );
        assert!(guard.ensure_valid().is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn issue69_caller_guard_symlink_observes_link_without_outside_contents() {
        let (root, home, _) = fixture();
        let outside = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let target = outside.path().join("outside.txt");
        fs::write(&target, "outside old\n").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("link")).unwrap();
        let guard = CallerGuard::capture(root.path().into(), home.path().join("diagnostics")).await;
        fs::write(&target, "outside new\n").unwrap();
        guard.verify("unchanged-link", None).await.unwrap();
        fs::remove_file(root.path().join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("different"), root.path().join("link"))
            .unwrap();
        assert!(guard.verify("retargeted-link", None).await.is_err());
    }

    #[test]
    fn issue69_caller_guard_growth_reader_is_bounded() {
        struct Growing {
            consumed: u64,
        }
        impl Read for Growing {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                bytes.fill(b'x');
                self.consumed += bytes.len() as u64;
                Ok(bytes.len())
            }
        }
        let mut reader = Growing { consumed: 0 };
        assert!(hash_bounded(&mut reader, 65537).is_err());
        assert_eq!(
            reader.consumed, 65538,
            "never wait for EOF from a growing source"
        );
    }
    #[tokio::test]
    async fn issue69_caller_guard_diagnostics_inside_caller_are_honestly_unretained() {
        let (root, _home, _) = fixture();
        let diagnostics = root.path().join(".generated/diagnostics");
        let guard = CallerGuard::capture(root.path().into(), diagnostics.clone()).await;
        fs::write(root.path().join("source.txt"), "modified\n").unwrap();
        let error = guard.verify("after-check", None).await.unwrap_err();
        assert!(
            error
                .downcast_ref::<CallerDriftError>()
                .unwrap()
                .receipt
                .is_none()
        );
        assert!(error.to_string().contains("diagnostics must be outside"));
        assert!(!diagnostics.exists());
        assert!(guard.ensure_valid().is_err());
    }
    #[test]
    fn issue69_caller_guard_output_parent_traversal_is_readonly_and_unambiguous() {
        let (root, home, _) = crate::checks::caller_guard_tests::fixture();
        let source = root.path().join("source.txt");
        let before = fs::read(&source).unwrap();
        let ambiguous = root.path().join(".generated/new/../../source.txt");
        assert!(planned_path(&ambiguous).is_err());
        assert!(!root.path().join(".generated").exists());
        assert_eq!(fs::read(&source).unwrap(), before);
        let existing_parent = home.path().join("existing");
        fs::create_dir(&existing_parent).unwrap();
        assert_eq!(
            planned_path(&existing_parent.join("../new-pack")).unwrap(),
            fs::canonicalize(home.path()).unwrap().join("new-pack")
        );
        assert!(!home.path().join("new-pack").exists());
        assert!(planned_path(&source.join("../new-pack")).is_err());
    }
}
