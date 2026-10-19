//! Strict caller boundary observations. No file restoration or continuous watcher.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

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
    output_source_roots: Mutex<BTreeSet<PathBuf>>,
}

#[derive(Clone)]
struct OwnedLeaf {
    mutable: bool,
    state: OwnedState,
    held_publication_file: Option<Weak<Mutex<File>>>,
}

#[derive(Clone)]
enum OwnedState {
    ActiveProducer,
    Complete(SourceEntry),
    Absent,
}
impl OwnedState {
    fn settled(expected: Option<SourceEntry>) -> Self {
        expected.map(Self::Complete).unwrap_or(Self::Absent)
    }
    fn expected(&self) -> Option<&SourceEntry> {
        match self {
            Self::Complete(entry) => Some(entry),
            Self::ActiveProducer | Self::Absent => None,
        }
    }
}
impl OwnedLeaf {
    fn active() -> Self {
        Self {
            mutable: false,
            state: OwnedState::ActiveProducer,
            held_publication_file: None,
        }
    }
    fn settled(mutable: bool, expected: Option<SourceEntry>) -> Self {
        Self {
            mutable,
            state: OwnedState::settled(expected),
            held_publication_file: None,
        }
    }
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
            output_source_roots: Mutex::new(BTreeSet::new()),
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
            owned.insert(path, OwnedLeaf::active());
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

    /// Additional read-only identity roots for concrete output collision checks.
    /// Their unrelated source is not a preservation baseline for this run.
    pub(crate) fn protect_output_source(&self, root: PathBuf) {
        self.output_source_roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(root);
    }

    fn output_is_tracked(&self, path: &Path) -> Result<bool> {
        if tracked_output_at(&self.root, path)? {
            return Ok(true);
        }
        let roots = self
            .output_source_roots
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for root in roots.iter() {
            // This run does not own any write in its original caller, including
            // an untracked destination reached through an external alias.
            if output_leaf_path(path)?.starts_with(root) || tracked_output_at(root, path)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn namespace_source_visible(&self, path: &Path) -> Result<bool> {
        if namespace_source_visible(&self.root, path)? {
            return Ok(true);
        }
        let physical = planned_path(path)?;
        let roots = self
            .output_source_roots
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Ok(roots
            .iter()
            .any(|root| physical.starts_with(root) || root.starts_with(&physical)))
    }

    pub(crate) fn owns_output(&self, path: &Path) -> bool {
        output_leaf_path(path).is_ok_and(|p| {
            self.owned_outputs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&p)
        })
    }

    #[cfg(unix)]
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
        let registered = owned.contains_key(&path);
        let known = owned
            .get(&path)
            .is_some_and(|leaf| !matches!(leaf.state, OwnedState::Absent));
        if !registered
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
            self.check_owned_preimage(&path, leaf)
                .map_err(std::io::Error::other)?;
        }
        if !registered && fs::symlink_metadata(&path).is_ok() {
            drop(owned);
            self.reject_new_foreign_collision(&path)
                .map_err(std::io::Error::other)?;
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("unowned output collision: {}", path.display()),
            ));
        }
        if !registered && !allow_new {
            return Err(std::io::Error::other(format!(
                "no exact output capability for {}",
                path.display()
            )));
        }
        let mut file = open(&path, !known)?;
        if known {
            if let Some(leaf) = owned.get(&path)
                && let Some(expected) = leaf.state.expected()
            {
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = file.metadata()?;
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
            if let Some(leaf) = owned.get_mut(&path) {
                leaf.state = OwnedState::ActiveProducer;
            }
            file.seek(SeekFrom::Start(0))?;
            file.set_len(0)?;
        }
        owned
            .entry(path)
            .and_modify(|leaf| leaf.state = OwnedState::ActiveProducer)
            .or_insert(OwnedLeaf::active());
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
            leaf.state = OwnedState::Complete(expected);
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
        self.admit_mutable_output_locked(path, None)
    }

    fn admit_mutable_output_locked(
        &self,
        path: &Path,
        held: Option<&Arc<Mutex<File>>>,
    ) -> Result<()> {
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
        let actual = match held {
            Some(file) => held_publication_state(&self.root, &path, file),
            None => output_state(&self.root, &path),
        }
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
            Some(leaf) if !matches!(leaf.state, OwnedState::ActiveProducer) => {
                leaf.state.expected().cloned()
            }
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
        if (visible
            || previous.is_some_and(|leaf| !matches!(leaf.state, OwnedState::ActiveProducer)))
            && expected != actual
        {
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
        let mut leaf = OwnedLeaf::settled(true, actual);
        leaf.held_publication_file = held.map(Arc::downgrade);
        owned.insert(path, leaf);
        Ok(())
    }

    /// The publication rail supplies its already-locked original handle and
    /// the exact role-validated prior bytes. No second content handle is opened.
    pub(crate) fn write_locked_publication_file(
        &self,
        path: &Path,
        file: &Arc<Mutex<File>>,
        previous: &[u8],
        bytes: &[u8],
    ) -> Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.admit_mutable_output_locked(path, Some(file))?;
        let path = output_leaf_path(path)?;
        let mut owned = self.owned_outputs.lock().unwrap_or_else(|e| e.into_inner());
        let leaf = owned
            .get_mut(&path)
            .context("publication lock was not admitted")?;
        self.check_owned_preimage(&path, leaf)?;
        if leaf
            .state
            .expected()
            .and_then(|entry| entry.sha256.as_ref())
            != Some(&hex(&Sha256::digest(previous)))
        {
            return Err(self.latch_output_unknown(
                "publication-lock-preimage",
                "publication lock changed after its role was validated",
            ));
        }
        let mut file = file.lock().map_err(|_| {
            self.latch_output_unknown("publication-lock-handle", "poisoned held handle")
        })?;
        let controlled_mode = mode(&file.metadata()?);
        file.rewind()?;
        file.set_len(0)?;
        file.write_all(bytes)?;
        file.flush()?;
        leaf.state = OwnedState::Complete(SourceEntry {
            path: output_entry_name(&self.root, &path)?,
            kind: "file",
            mode: controlled_mode,
            sha256: Some(hex(&Sha256::digest(bytes))),
        });
        Ok(())
    }

    /// Seal only the original producer handle, after the owner has finished.
    /// Active streams have no completed postimage; completed/absent leaves do.
    pub(crate) fn finish_output_producer(&self, path: &Path, file: &mut File) -> Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.ensure_valid()?;
        let path =
            output_leaf_path(path).map_err(|e| self.latch_output_unknown("producer-path", e))?;
        ensure_untracked_output(self, &path)?;
        let mut owned = self.owned_outputs.lock().unwrap_or_else(|e| e.into_inner());
        let leaf = owned
            .get_mut(&path)
            .context("producer has no exact output claim")?;
        if !matches!(leaf.state, OwnedState::ActiveProducer) {
            return self.check_owned_preimage(&path, leaf);
        }
        let entry = producer_file_state(&self.root, &path, file)
            .map_err(|e| self.latch_output_unknown("producer-completion", e))?;
        leaf.state = OwnedState::Complete(entry);
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

    fn check_owned_preimage(&self, path: &Path, leaf: &OwnedLeaf) -> Result<()> {
        if matches!(leaf.state, OwnedState::ActiveProducer) {
            return Ok(());
        }
        let actual = match leaf.held_publication_file.as_ref().and_then(Weak::upgrade) {
            Some(file) => held_publication_state(&self.root, path, &file),
            None => output_state(&self.root, path),
        }
        .map_err(|error| self.latch_output_unknown("owned-output-read", error))?;
        if leaf.state.expected() != actual.as_ref() {
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
            self.check_owned_preimage(&path, leaf)
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
            leaf.state = OwnedState::Absent;
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
            self.check_owned_preimage(&source, leaf)
                .map_err(std::io::Error::other)?;
        }
        if let Some(leaf) = owned.get(&destination) {
            self.check_owned_preimage(&destination, leaf)
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
        let mut intended = owned
            .get(&source)
            .and_then(|leaf| leaf.state.expected().cloned());
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
        let destination_leaf = owned
            .entry(destination.clone())
            .or_insert(OwnedLeaf::active());
        destination_leaf.state = match intended {
            Some(entry) => OwnedState::Complete(entry),
            None => OwnedState::ActiveProducer,
        };
        if let Some(leaf) = owned.get_mut(&source) {
            leaf.state = OwnedState::Absent;
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
            .insert(path, OwnedLeaf::settled(false, Some(entry)));
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
            self.check_owned_preimage(path, leaf)?;
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
            owned.insert(staged.clone(), OwnedLeaf::active());
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
            if let Some(leaf) = owned.get_mut(&staged) {
                leaf.state = OwnedState::Absent;
            }
            Some(expected)
        } else {
            fs::remove_file(&latest)?;
            None
        };
        if let Some(leaf) = owned.get_mut(&latest) {
            leaf.state = OwnedState::settled(expected);
        }
        File::open(parent)?.sync_all()?;
        fs::remove_file(&journal)?;
        if let Some(leaf) = owned.get_mut(&journal) {
            leaf.state = OwnedState::Absent;
        }
        if let Some(parent) = journal.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// Revoke only this run's still-owned index row after a late fatal drift.
    /// The publication lock stays held; no predecessor is resurrected.
    pub(crate) fn revoke_publication_index_after_drift(
        &self,
        _publication: &crate::storage::RunPublicationLock,
        path: &Path,
        out_dir: &Path,
    ) -> Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            self.ensure_valid().is_err(),
            "late revocation requires sticky drift"
        );
        let path = output_leaf_path(path)?;
        ensure!(
            path.file_name() == Some(std::ffi::OsStr::new("index.jsonl")),
            "not a publication index role"
        );
        ensure_untracked_output(self, &path)?;
        let parent = path.parent().context("publication index parent")?;
        let mut owned = self.owned_outputs.lock().unwrap_or_else(|e| e.into_inner());
        let leaf = owned
            .get(&path)
            .context("index has no last-own publication state")?;
        ensure!(leaf.mutable, "index is not a mutable publication role");
        let length = fs::symlink_metadata(&path)?.len();
        self.check_owned_preimage(&path, leaf)?;
        let expected = leaf
            .state
            .expected()
            .context("published index has no completed bytes")?;
        ensure!(
            expected.kind == "file",
            "published index is not a regular file"
        );
        let file = open_source_file(parent, Path::new(path.file_name().context("index leaf")?))?;
        let mut before = Vec::new();
        file.take(length.checked_add(1).context("index size overflow")?)
            .read_to_end(&mut before)?;
        ensure!(
            before.len() as u64 == length
                && expected.sha256.as_ref() == Some(&hex(&Sha256::digest(&before))),
            "index changed while deriving late revocation"
        );
        let (after, removed) = crate::storage::remove_run_row_bytes(&before, out_dir)?;
        if removed == 0 {
            return Ok(());
        }
        let mut staged = None;
        for attempt in 0..16 {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos();
            let candidate = output_leaf_path(&parent.join(format!(
                ".caller-drift-index.{}.{nonce}.{attempt}.tmp",
                std::process::id()
            )))?;
            ensure_untracked_output(self, &candidate)?;
            ensure!(
                !self.baseline_contains_output(&candidate)?,
                "index revoke stage collides with initial userdata"
            );
            match fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => {
                    staged = Some((candidate, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let (staged, mut file) =
            staged.context("could not allocate exact index revocation stage")?;
        owned.insert(staged.clone(), OwnedLeaf::active());
        let controlled_mode = mode(&file.metadata()?);
        file.write_all(&after)?;
        file.sync_all()?;
        drop(file);
        let intended = SourceEntry {
            path: output_entry_name(&self.root, &path)?,
            kind: "file",
            mode: controlled_mode,
            sha256: Some(hex(&Sha256::digest(&after))),
        };
        if let Some(leaf) = owned.get_mut(&staged) {
            let mut staged_intent = intended.clone();
            staged_intent.path = output_entry_name(&self.root, &staged)?;
            leaf.state = OwnedState::Complete(staged_intent);
        }
        self.check_owned_preimage(
            &staged,
            owned
                .get(&staged)
                .context("index revoke stage disappeared")?,
        )?;
        self.check_owned_preimage(&path, owned.get(&path).context("index role disappeared")?)?;
        fs::rename(&staged, &path)?;
        if let Some(leaf) = owned.get_mut(&path) {
            leaf.state = OwnedState::Complete(intended);
        }
        if let Some(leaf) = owned.get_mut(&staged) {
            leaf.state = OwnedState::Absent;
        }
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn revoke_committed_latest_after_drift(
        &self,
        _publication: &crate::storage::RunPublicationLock,
        out_dir: &Path,
    ) -> Result<()> {
        let _gate = self
            .observation_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            self.ensure_valid().is_err(),
            "late revocation requires sticky drift"
        );
        let parent = out_dir.parent().context("committed publication parent")?;
        let latest = output_leaf_path(&parent.join("latest"))?;
        ensure_untracked_output(self, &latest)?;
        let mut owned = self.owned_outputs.lock().unwrap_or_else(|e| e.into_inner());
        let leaf = owned
            .get(&latest)
            .context("latest has no last-own publication state")?;
        ensure!(leaf.mutable, "latest is not a mutable publication role");
        self.check_owned_preimage(&latest, leaf)?;
        if matches!(leaf.state, OwnedState::Absent) {
            return Ok(());
        }
        ensure!(
            Some(fs::read_link(&latest)?.as_os_str()) == out_dir.file_name(),
            "latest no longer advertises this run; foreign alias retained"
        );
        fs::remove_file(&latest)?;
        if let Some(leaf) = owned.get_mut(&latest) {
            leaf.state = OwnedState::Absent;
        }
        File::open(parent)?.sync_all()?;
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
            self.check_owned_preimage(&path, leaf)
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
            leaf.state = OwnedState::Absent;
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
            .insert(path.clone(), OwnedLeaf::active());
        file.write_all(bytes)?;
        file.sync_all()?;
        if let Some(leaf) = self
            .owned_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&path)
        {
            leaf.state = OwnedState::Complete(SourceEntry {
                path: output_entry_name(&self.root, &path).map_err(std::io::Error::other)?,
                kind: "file",
                mode: mode(&file.metadata()?),
                sha256: Some(hex(&Sha256::digest(bytes))),
            });
        }
        Ok(())
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
            self.check_owned_preimage(path, leaf)?;
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
            let retained = self
                .namespace_source_visible(&self.diagnostics)
                .and_then(|visible| {
                    ensure!(
                        !visible,
                        "caller diagnostic namespace redirects into protected source"
                    );
                    persist_receipt(&self.diagnostics, &receipt)
                });
            match retained {
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
    for bytes in tracked_paths(&repo)?.paths {
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

struct TrackedPaths {
    paths: BTreeSet<Vec<u8>>,
    gitlinks: BTreeSet<Vec<u8>>,
}

fn tracked_paths(repo: &git2::Repository) -> Result<TrackedPaths> {
    let index = repo.index()?;
    let mut paths = BTreeSet::new();
    let mut gitlinks = BTreeSet::new();
    for entry in index.iter() {
        if entry.mode == 0o160000 {
            gitlinks.insert(entry.path.clone());
        }
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
        tree.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
            if matches!(
                entry.kind(),
                Some(git2::ObjectType::Blob | git2::ObjectType::Commit)
            ) {
                let mut path = prefix.as_bytes().to_vec();
                path.extend_from_slice(entry.name_bytes());
                if entry.kind() == Some(git2::ObjectType::Commit) {
                    gitlinks.insert(path.clone());
                }
                paths.insert(path);
            }
            git2::TreeWalkResult::Ok
        })?;
    }
    Ok(TrackedPaths { paths, gitlinks })
}

fn capture_token(root: &Path, owned: &BTreeSet<String>) -> Result<CallerToken> {
    capture_token_at_depth(root, owned, 0)
}

const MAX_CALLER_GITLINK_DEPTH: usize = 16;

/// Never discover a parent repository or follow a gitlink/ancestor symlink.
/// Missing, file and symlink replacements use the ordinary source observation.
fn gitlink_directory(root: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        current.push(component.as_os_str());
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() {
            return Ok(None);
        }
        if !metadata.is_dir() {
            ensure!(
                components.peek().is_none(),
                "gitlink ancestor is not a directory"
            );
            return Ok(None);
        }
    }
    ensure!(
        fs::canonicalize(&current)? == current,
        "gitlink directory resolves through an alias"
    );
    Ok(Some(current))
}

fn gitlink_repository(directory: &Path) -> Result<Option<git2::Repository>> {
    let marker = match fs::symlink_metadata(directory.join(".git")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                fs::read_dir(directory)?.next().is_none(),
                "nonempty gitlink directory has no repository: {}",
                directory.display()
            );
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        !marker.file_type().is_symlink() && (marker.is_file() || marker.is_dir()),
        "unsupported gitlink Git marker: {}",
        directory.display()
    );
    let repo = git2::Repository::open(directory)?;
    ensure!(
        fs::canonicalize(directory)? == directory
            && repo.workdir().map(fs::canonicalize).transpose()?.as_deref() == Some(directory),
        "gitlink repository worktree does not match its caller path"
    );
    Ok(Some(repo))
}

fn gitlink_entry(root: &Path, relative: &Path, depth: usize) -> Result<SourceEntry> {
    let Some(directory) = gitlink_directory(root, relative)? else {
        return source_entry(root, relative);
    };
    let mode = mode(&fs::symlink_metadata(&directory)?);
    let Some(repo) = gitlink_repository(&directory)? else {
        return Ok(SourceEntry {
            path: source_entry_name(relative),
            kind: "gitlink-uninitialized",
            mode,
            sha256: None,
        });
    };
    let git_dir = fs::canonicalize(repo.path())?;
    let common_dir = fs::canonicalize(repo.commondir())?;
    let marker = fs::symlink_metadata(directory.join(".git"))?;
    let pointer = if marker.is_file() {
        Some(source_entry(&directory, Path::new(".git"))?)
    } else {
        None
    };
    // No owned-output exemptions inside a gitlink. Its operator checkout need
    // not equal the pin; preserve and compare its actual initial dirty state.
    let token = capture_token_at_depth(&directory, &BTreeSet::new(), depth + 1)?;
    ensure!(
        token.root == directory
            && token.git_dir == git_dir
            && token.common_dir == common_dir
            && gitlink_directory(root, relative)?.as_ref() == Some(&directory),
        "gitlink repository identity changed during observation"
    );
    if let Some(before) = &pointer {
        ensure!(
            *before == source_entry(&directory, Path::new(".git"))?,
            "gitlink Git pointer changed during observation"
        );
    }
    Ok(SourceEntry {
        path: source_entry_name(relative),
        kind: "gitlink",
        mode,
        sha256: Some(hex(&Sha256::digest(serde_json::to_vec(&(token, pointer))?))),
    })
}

/// The pre-open physical-identity check includes nested tracked leaves, not
/// only their containing gitlink directories (hardlinks and case aliases).
fn protected_tracked_paths(root: &Path, depth: usize) -> Result<Vec<PathBuf>> {
    ensure!(
        depth <= MAX_CALLER_GITLINK_DEPTH,
        "caller gitlink nesting limit exceeded"
    );
    let repo = git2::Repository::open(root)?;
    let tracked = tracked_paths(&repo)?;
    let mut paths = Vec::new();
    for bytes in &tracked.paths {
        let relative = git_path(bytes)?;
        paths.push(root.join(&relative));
        if tracked.gitlinks.contains(bytes)
            && let Some(directory) = gitlink_directory(root, &relative)?
            && gitlink_repository(&directory)?.is_some()
        {
            paths.extend(protected_tracked_paths(&directory, depth + 1)?);
        }
    }
    Ok(paths)
}

fn capture_token_at_depth(
    root: &Path,
    owned: &BTreeSet<String>,
    depth: usize,
) -> Result<CallerToken> {
    ensure!(
        depth <= MAX_CALLER_GITLINK_DEPTH,
        "caller gitlink nesting limit exceeded"
    );
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
    let mut paths = tracked.paths.clone();
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
        if owned.contains(&key) && !tracked.paths.contains(&bytes) {
            continue;
        }
        let relative = git_path(&bytes)?;
        let entry = if tracked.gitlinks.contains(&bytes) {
            gitlink_entry(&root, &relative, depth)?
        } else {
            source_entry(&root, &relative)?
        };
        sources.insert(key, entry);
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

#[cfg(unix)]
fn existing_leaf_identity(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn existing_leaf_identity(path: &Path) -> std::io::Result<(u64, u64)> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    };
    let file = fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((
        u64::from(information.dwVolumeSerialNumber),
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    ))
}

fn tracked_output_at(root: &Path, path: &Path) -> Result<bool> {
    let root = fs::canonicalize(root)?;
    let repo = git2::Repository::open(&root)?;
    let tracked = tracked_paths(&repo)?;
    if let Ok(relative) = path.strip_prefix(&root)
        && tracked.paths.iter().any(|bytes| {
            git_path(bytes).is_ok_and(|p| {
                p == relative || (tracked.gitlinks.contains(bytes) && relative.starts_with(p))
            })
        })
    {
        return Ok(true);
    }
    #[cfg(any(unix, windows))]
    {
        let identity = match existing_leaf_identity(path) {
            Ok(identity) => identity,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        for tracked_path in protected_tracked_paths(&root, 0)? {
            match existing_leaf_identity(&tracked_path) {
                Ok(tracked_identity) if tracked_identity == identity => return Ok(true),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(false)
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
        hex(git_relative_text(path).as_bytes())
    }
}

#[cfg(not(unix))]
fn git_relative_text(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
fn source_entry_name(path: &Path) -> String {
    #[cfg(unix)]
    {
        format!("{path:?}")
    }
    #[cfg(not(unix))]
    {
        format!("{:?}", git_relative_text(path))
    }
}

fn output_entry_name(root: &Path, path: &Path) -> Result<String> {
    let root = fs::canonicalize(root)?;
    let relative = path
        .strip_prefix(&root)
        .unwrap_or_else(|_| Path::new(path.file_name().unwrap_or_default()));
    Ok(source_entry_name(relative))
}

fn held_publication_state(
    root: &Path,
    path: &Path,
    held: &Arc<Mutex<File>>,
) -> Result<Option<SourceEntry>> {
    let mut file = held
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned publication handle"))?;
    Ok(Some(producer_file_state(root, path, &mut file)?))
}

fn producer_file_state(root: &Path, path: &Path, file: &mut File) -> Result<SourceEntry> {
    let before = file.metadata()?;
    let at_path = fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && at_path.is_file() && !at_path.file_type().is_symlink(),
        "producer path is not its regular leaf"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.nlink() == 1 && before.dev() == at_path.dev() && before.ino() == at_path.ino(),
            "producer pathname no longer names its original handle"
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            at_path.file_attributes() & 0x400 == 0,
            "producer path is a reparse point"
        );
        // A held v2 lock was opened without FILE_SHARE_DELETE, so its leaf
        // cannot be removed/replaced while this original handle is alive.
        ensure!(
            before.len() == at_path.len() && before.modified()? == at_path.modified()?,
            "producer path metadata changed"
        );
    }
    let position = file.stream_position()?;
    let observation = (|| -> Result<String> {
        file.rewind()?;
        hash_bounded(file, before.len())
    })();
    let restored = file.seek(SeekFrom::Start(position));
    let digest = observation?;
    restored?;
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len()
            && before.modified()? == after.modified()?
            && mode(&before) == mode(&after),
        "producer changed while completing"
    );
    Ok(SourceEntry {
        path: output_entry_name(root, path)?,
        kind: "file",
        mode: mode(&before),
        sha256: Some(digest),
    })
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
                    path: source_entry_name(relative),
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
                path: source_entry_name(relative),
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
            path: source_entry_name(relative),
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

    fn gitlink_fixture() -> (tempfile::TempDir, tempfile::TempDir, git2::Oid) {
        let (root, home, _) = fixture();
        let child = git2::Repository::init(root.path().join("backend")).unwrap();
        fs::write(root.path().join("backend/source.rs"), "pinned source\n").unwrap();
        let pin = commit_gitlink_child(&child);
        let repo = git2::Repository::open(root.path()).unwrap();
        fs::write(
            root.path().join(".gitmodules"),
            "[submodule \"backend\"]\n\tpath = backend\n\turl = ../fixture-source\n",
        )
        .unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitmodules")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let mut builder = repo.treebuilder(Some(&tree)).unwrap();
        builder.insert("backend", pin, 0o160000).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        index.read_tree(&tree).unwrap();
        index.write().unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "pin backend", &tree, &[&parent])
            .unwrap();
        (root, home, pin)
    }

    fn commit_gitlink_child(repo: &git2::Repository) -> git2::Oid {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("source.rs")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().ok().map(|head| head.peel_to_commit().unwrap());
        let parents: Vec<_> = parent.iter().collect();
        let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "child source", &tree, &parents)
            .unwrap()
    }

    #[test]
    fn issue69_gitlink_guard_accepts_stable_pinned_checkout_and_operator_dirty_state() {
        let (root, home, pin) = gitlink_fixture();
        let child = git2::Repository::open(root.path().join("backend")).unwrap();
        fs::write(root.path().join("backend/source.rs"), "operator commit\n").unwrap();
        assert_ne!(commit_gitlink_child(&child), pin);
        fs::write(
            root.path().join("backend/source.rs"),
            "operator dirty bytes\n",
        )
        .unwrap();
        fs::write(
            root.path().join("backend/userdata.rs"),
            "untracked baseline\n",
        )
        .unwrap();
        let guard = CallerGuard::capture_sync(root.path().into(), home.path().join("diagnostics"));
        guard.verify_sync("before-checks", None).unwrap();
        guard.verify_sync("after-checks", None).unwrap();
        assert_eq!(
            fs::read(root.path().join("backend/source.rs")).unwrap(),
            b"operator dirty bytes\n"
        );
    }

    #[test]
    fn issue69_gitlink_guard_refuses_foreign_source_head_and_index_drift() {
        for mutation in [
            "tracked",
            "untracked-new",
            "untracked-edit",
            "head",
            "index",
        ] {
            let (root, home, _) = gitlink_fixture();
            let backend = root.path().join("backend");
            fs::write(backend.join("userdata.rs"), "before\n").unwrap();
            let guard =
                CallerGuard::capture_sync(root.path().into(), home.path().join("diagnostics"));
            guard.verify_sync("before-checks", None).unwrap();
            let child = git2::Repository::open(&backend).unwrap();
            match mutation {
                "tracked" => fs::write(backend.join("source.rs"), "foreign\n").unwrap(),
                "untracked-new" => fs::write(backend.join("new.rs"), "foreign\n").unwrap(),
                "untracked-edit" => fs::write(backend.join("userdata.rs"), "foreign\n").unwrap(),
                "head" => child
                    .set_head_detached(child.head().unwrap().target().unwrap())
                    .unwrap(),
                "index" => {
                    let mut index = child.index().unwrap();
                    index.remove_path(Path::new("source.rs")).unwrap();
                    index.write().unwrap();
                }
                _ => unreachable!(),
            }
            let error = guard.verify_sync("after-foreign-edit", None).unwrap_err();
            assert!(
                error
                    .downcast_ref::<CallerDriftError>()
                    .unwrap()
                    .receipt
                    .as_ref()
                    .unwrap()
                    .is_file(),
                "{mutation}"
            );
            assert!(guard.verify_sync("later", None).is_err(), "{mutation}");
            if mutation == "tracked" {
                assert_eq!(fs::read(backend.join("source.rs")).unwrap(), b"foreign\n");
            }
        }
    }

    #[test]
    fn issue69_gitlink_guard_accepts_missing_and_empty_but_observes_initialization() {
        for empty in [false, true] {
            let (root, home, _) = gitlink_fixture();
            let backend = root.path().join("backend");
            fs::rename(&backend, home.path().join("parked-backend")).unwrap();
            if empty {
                fs::create_dir(&backend).unwrap();
            }
            let guard =
                CallerGuard::capture_sync(root.path().into(), home.path().join("diagnostics"));
            guard.verify_sync("before-checks", None).unwrap();
            if empty {
                fs::remove_dir(&backend).unwrap();
            }
            fs::rename(home.path().join("parked-backend"), &backend).unwrap();
            assert!(guard.verify_sync("after-initialization", None).is_err());
        }
    }

    #[test]
    fn issue69_gitlink_guard_refuses_owned_outputs_and_nested_tracked_alias() {
        let (root, home, _) = gitlink_fixture();
        let guard = CallerGuard::capture_sync(root.path().into(), home.path().join("diagnostics"));
        guard.verify_sync("before-checks", None).unwrap();
        let alias = home.path().join("source-alias");
        fs::hard_link(root.path().join("backend/source.rs"), &alias).unwrap();
        assert!(
            guard
                .seed_launcher_outputs(std::slice::from_ref(&alias))
                .is_err()
        );
        assert!(!guard.owns_output(&alias));
        for path in [
            root.path().join("backend/source.rs"),
            root.path().join("backend/new.rs"),
            alias,
        ] {
            assert!(
                guard
                    .create_output(&path, true, |_, _| panic!("tracked writer must never open"))
                    .is_err()
            );
        }
        assert_eq!(
            fs::read(root.path().join("backend/source.rs")).unwrap(),
            b"pinned source\n"
        );
    }

    #[test]
    fn issue69_gitlink_guard_head_only_gitlink_observes_regular_replacement() {
        let (root, home, _) = gitlink_fixture();
        fs::rename(
            root.path().join("backend"),
            home.path().join("parked-backend"),
        )
        .unwrap();
        fs::write(root.path().join("backend"), "staged regular replacement\n").unwrap();
        let repo = git2::Repository::open(root.path()).unwrap();
        let mut index = repo.index().unwrap();
        index.remove_path(Path::new("backend")).unwrap();
        index.add_path(Path::new("backend")).unwrap();
        index.write().unwrap();
        let guard = CallerGuard::capture_sync(root.path().into(), home.path().join("diagnostics"));
        guard.verify_sync("before-checks", None).unwrap();
        fs::write(root.path().join("backend"), "foreign replacement edit\n").unwrap();
        assert!(guard.verify_sync("after-foreign-edit", None).is_err());
    }

    #[test]
    fn issue69_gitlink_guard_nested_sources_and_alias_are_protected() {
        let (root, home, _) = gitlink_fixture();
        let backend = root.path().join("backend");
        let nested = backend.join("nested");
        let child = git2::Repository::init(&nested).unwrap();
        fs::write(nested.join("source.rs"), "nested pinned source\n").unwrap();
        let pin = commit_gitlink_child(&child);
        fs::write(
            backend.join(".gitmodules"),
            "[submodule \"nested\"]\n\tpath = nested\n\turl = ../nested-source\n",
        )
        .unwrap();
        let repo = git2::Repository::open(&backend).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitmodules")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let mut builder = repo.treebuilder(Some(&tree)).unwrap();
        builder.insert("nested", pin, 0o160000).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        index.read_tree(&tree).unwrap();
        index.write().unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "pin nested", &tree, &[&parent])
            .unwrap();
        let guard = CallerGuard::capture_sync(root.path().into(), home.path().join("diagnostics"));
        guard.verify_sync("before-checks", None).unwrap();
        let alias = home.path().join("nested-alias");
        fs::hard_link(nested.join("source.rs"), &alias).unwrap();
        assert!(
            guard
                .seed_launcher_outputs(std::slice::from_ref(&alias))
                .is_err()
        );
        assert!(!guard.owns_output(&alias));
        assert!(
            guard
                .create_output(&alias, true, |_, _| panic!(
                    "nested alias writer must never open"
                ))
                .is_err()
        );
        fs::write(nested.join("source.rs"), "foreign nested edit\n").unwrap();
        assert!(guard.verify_sync("after-foreign-edit", None).is_err());
        assert_eq!(
            fs::read(nested.join("source.rs")).unwrap(),
            b"foreign nested edit\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn issue69_gitlink_guard_observes_symlink_replacement_without_following_target() {
        let (root, home, _) = gitlink_fixture();
        let backend = root.path().join("backend");
        fs::rename(&backend, home.path().join("parked-backend")).unwrap();
        let outside = home.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("source.rs"), "outside source\n").unwrap();
        std::os::unix::fs::symlink(&outside, &backend).unwrap();
        let guard = CallerGuard::capture_sync(root.path().into(), home.path().join("diagnostics"));
        guard.verify_sync("before-checks", None).unwrap();
        fs::write(outside.join("source.rs"), "outside changed\n").unwrap();
        guard
            .verify_sync("outside-is-not-caller-source", None)
            .unwrap();
        fs::remove_file(&backend).unwrap();
        std::os::unix::fs::symlink(home.path().join("different"), &backend).unwrap();
        assert!(guard.verify_sync("after-link-retarget", None).is_err());
    }

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
