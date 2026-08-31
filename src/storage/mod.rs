//! Artifact index and retention policy
//!
//! Maintains `~/.prview/index.jsonl` (append-only JSONL) with metadata for
//! every run. Provides listing, filtering, pruning, and rebuild from disk.

use crate::config::{
    branch_storage_key, current_branch_name, find_repo_root_from, prview_home, repo_name_from_root,
};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    static TEST_FAIL_NEXT_INDEX_SAVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_FAIL_NEXT_PRUNE_STAGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn arm_test_index_save_failure() {
    TEST_FAIL_NEXT_INDEX_SAVE.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_test_index_save_failure() -> bool {
    TEST_FAIL_NEXT_INDEX_SAVE.with(|armed| armed.replace(false))
}

#[cfg(test)]
fn arm_test_prune_stage_failure() {
    TEST_FAIL_NEXT_PRUNE_STAGE.with(|armed| armed.set(true));
}

#[cfg(test)]
fn take_test_prune_stage_failure() -> bool {
    TEST_FAIL_NEXT_PRUNE_STAGE.with(|armed| armed.replace(false))
}

// ---------------------------------------------------------------------------
// RunEntry
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RunEntry {
    pub id: String,
    pub repo: String,
    pub branch: String,
    pub commit: String,
    pub path: PathBuf,
    pub created_at: String,
    pub quality_pass: bool,
    pub merge_status: String,
    pub policy_mode: String,
    pub checks_passed: usize,
    pub checks_failed: usize,
    pub files_changed: usize,
    pub size_bytes: u64,
    pub has_dashboard: bool,
}

// ---------------------------------------------------------------------------
// RetentionPolicy
// ---------------------------------------------------------------------------

pub struct RetentionPolicy {
    pub max_runs_per_branch: usize,
    pub max_runs_per_repo: usize,
    pub max_total_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_runs_per_branch: 20,
            max_runs_per_repo: 200,
            max_total_bytes: 5 * 1024 * 1024 * 1024, // 5 GB
        }
    }
}

// ---------------------------------------------------------------------------
// RunIndex
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RunIndex {
    entries: Vec<RunEntry>,
}

fn index_path() -> PathBuf {
    prview_home().join("index.jsonl")
}

fn lock_path() -> PathBuf {
    prview_home().join("index.jsonl.lock")
}

fn prune_trash_path() -> PathBuf {
    prview_home().join("prune-trash")
}

fn resolve_explicit_index_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        anyhow!(
            "Index path must include a parent directory: {}",
            path.display()
        )
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("Index path must include a file name: {}", path.display()))?;
    crate::paths::resolve_file_name_within(parent, file_name)
}

/// Parse index entries line-by-line, skipping (never truncating on) bad lines.
///
/// `map_while(Result::ok)` used to stop at the first line `BufRead::lines`
/// returns an `Err` for (e.g. non-UTF-8): every later run vanished from the
/// view, and the next `register_and_prune` save persisted that loss — permanent
/// data loss from one bad byte. Here an unreadable line is skipped with a warn
/// and iteration continues; an invalid-JSON line is skipped silently as before.
fn read_entries_skipping_bad_lines(file: fs::File, path: &Path) -> Vec<RunEntry> {
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                eprintln!(
                    "prview: skipping unreadable index line {} in {}: {err}",
                    idx + 1,
                    path.display()
                );
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<RunEntry>(&line) {
            entries.push(entry);
        }
    }
    entries
}

/// Parse every persisted index row or fail without manufacturing an empty
/// ledger. Writers and crash recovery use this path because silently dropping
/// one row and saving the remainder would turn a local corruption into durable
/// history loss.
fn read_entries_strict(file: fs::File, path: &Path) -> Result<Vec<RunEntry>> {
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line_number = idx + 1;
        let line = line.with_context(|| {
            format!(
                "Failed reading run index line {line_number} from {}",
                path.display()
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        entries.push(serde_json::from_str::<RunEntry>(&line).with_context(|| {
            format!(
                "Invalid run index JSON on line {line_number} in {}",
                path.display()
            )
        })?);
    }
    Ok(entries)
}

impl RunIndex {
    /// Load index from `~/.prview/index.jsonl`. Missing/corrupt lines are skipped.
    pub fn load() -> Self {
        let path = index_path();
        let entries = match fs::File::open(&path) {
            Ok(file) => read_entries_skipping_bad_lines(file, &path),
            Err(_) => Vec::new(),
        };
        Self { entries }
    }

    /// Load the canonical index for a read-modify-write transaction.
    ///
    /// A missing index is the expected first-run state. Every other open or
    /// parse error is fatal so publication never persists a fabricated empty
    /// ledger over historical rows it could not faithfully read.
    pub(crate) fn load_strict() -> Result<Self> {
        let path = resolve_explicit_index_path(&index_path())?;
        let entries = match fs::File::open(&path) {
            Ok(file) => read_entries_strict(file, &path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed opening run index {}", path.display()));
            }
        };
        Ok(Self { entries })
    }

    /// Load index from an explicit path. Missing/corrupt lines are skipped.
    pub fn load_from(path: &Path) -> Self {
        let resolved = match resolve_explicit_index_path(path) {
            Ok(path) => path,
            Err(_) => {
                return Self {
                    entries: Vec::new(),
                };
            }
        };
        let entries = match fs::File::open(&resolved) {
            Ok(file) => read_entries_skipping_bad_lines(file, &resolved),
            Err(_) => Vec::new(),
        };
        Self { entries }
    }

    /// Atomic save to an explicit path.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let resolved = resolve_explicit_index_path(path)?;
        self.save_resolved(&resolved)
    }

    /// Atomic save: write to tmp file then rename.
    pub fn save(&self) -> Result<()> {
        let path = resolve_explicit_index_path(&index_path())?;
        self.save_resolved(&path)
    }

    fn save_resolved(&self, path: &Path) -> Result<()> {
        #[cfg(test)]
        if take_test_index_save_failure() {
            bail!("injected index save failure");
        }
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("index path has no parent: {}", path.display()))?;
        let (temp, mut file) = create_owned_temp_file(parent, "index-jsonl")?;
        let write_result = (|| -> Result<()> {
            for entry in &self.entries {
                let line = serde_json::to_string(entry)?;
                writeln!(file, "{}", line)?;
            }
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        if let Err(error) = atomic_replace_file(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }

    pub fn append(&mut self, entry: RunEntry) {
        self.entries.push(entry);
    }

    pub fn entries(&self) -> &[RunEntry] {
        &self.entries
    }

    pub fn list_for_repo(&self, repo: &str) -> Vec<&RunEntry> {
        self.entries.iter().filter(|e| e.repo == repo).collect()
    }

    pub fn list_for_branch(&self, repo: &str, branch: &str) -> Vec<&RunEntry> {
        let branch_keys = branch_lookup_keys(branch);
        self.entries
            .iter()
            .filter(|e| e.repo == repo && branch_keys.contains(&e.branch))
            .collect()
    }

    pub fn latest(&self, repo: &str, branch: &str) -> Option<&RunEntry> {
        let branch_keys = branch_lookup_keys(branch);
        self.entries
            .iter()
            .rev()
            .find(|e| e.repo == repo && branch_keys.contains(&e.branch))
    }

    /// Remove entries whose path no longer exists on disk.
    pub fn remove_stale(&mut self) {
        self.entries.retain(|e| e.path.is_dir());
    }

    /// Rebuild index by scanning `~/.prview/runs/` and parsing `report.json`.
    pub fn rebuild() -> Self {
        let runs_dir = prview_home().join("runs");
        let mut entries = Vec::new();

        if !runs_dir.is_dir() {
            return Self { entries };
        }

        // runs/<repo>/<branch>/<run_id>/
        let repos = read_subdirs(&runs_dir);
        for repo_dir in repos {
            let repo_name = dir_name(&repo_dir);
            let branches = read_subdirs(&repo_dir);
            for branch_dir in branches {
                let branch_name = dir_name(&branch_dir);
                let runs = read_subdirs(&branch_dir);
                for run_dir in runs {
                    let id = dir_name(&run_dir);
                    // Skip "latest" symlink
                    if id == "latest" {
                        continue;
                    }
                    if let Some(entry) = entry_from_disk(&run_dir, &id, &repo_name, &branch_name) {
                        entries.push(entry);
                    }
                }
            }
        }

        // Sort by created_at ascending
        entries.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        Self { entries }
    }

    /// Prune runs exceeding retention limits.
    ///
    /// Returns paths of run directories to delete. Modifies `self.entries`
    /// to remove pruned entries. The caller is responsible for deleting the
    /// directories and calling `save()`.
    pub fn prune(&mut self, policy: &RetentionPolicy, current_run: &Path) -> Vec<PathBuf> {
        let mut protected: HashSet<PathBuf> = HashSet::new();
        protected.insert(current_run.to_path_buf());

        // Protect all "latest" symlink targets
        for entry in &self.entries {
            if let Some(parent) = entry.path.parent() {
                let latest = parent.join("latest");
                if let Ok(target) = fs::read_link(&latest) {
                    // Symlink is relative (just dirname), resolve against parent
                    let resolved = parent.join(target);
                    protected.insert(resolved);
                }
            }
        }

        let mut to_remove: HashSet<PathBuf> = HashSet::new();

        // 1. Per-branch cap
        let mut branch_groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            branch_groups
                .entry((e.repo.clone(), e.branch.clone()))
                .or_default()
                .push(i);
        }
        for indices in branch_groups.values() {
            if indices.len() > policy.max_runs_per_branch {
                let mut excess = indices.len() - policy.max_runs_per_branch;
                for &idx in indices {
                    if excess == 0 {
                        break;
                    }
                    let path = &self.entries[idx].path;
                    if !protected.contains(path) {
                        to_remove.insert(path.clone());
                        excess -= 1;
                    }
                }
            }
        }

        // 2. Per-repo cap
        let mut repo_groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in self.entries.iter().enumerate() {
            if !to_remove.contains(&e.path) {
                repo_groups.entry(e.repo.clone()).or_default().push(i);
            }
        }
        for indices in repo_groups.values() {
            if indices.len() > policy.max_runs_per_repo {
                let mut excess = indices.len() - policy.max_runs_per_repo;
                for &idx in indices {
                    if excess == 0 {
                        break;
                    }
                    let path = &self.entries[idx].path;
                    if !protected.contains(path) {
                        to_remove.insert(path.clone());
                        excess -= 1;
                    }
                }
            }
        }

        // 3. Global size cap
        let remaining: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !to_remove.contains(&e.path))
            .map(|(i, _)| i)
            .collect();
        let total_size: u64 = remaining.iter().map(|&i| self.entries[i].size_bytes).sum();
        if total_size > policy.max_total_bytes {
            let mut freed: u64 = 0;
            let needed = total_size - policy.max_total_bytes;
            for &idx in &remaining {
                if freed >= needed {
                    break;
                }
                let path = &self.entries[idx].path;
                if !protected.contains(path) {
                    freed += self.entries[idx].size_bytes;
                    to_remove.insert(path.clone());
                }
            }
        }

        self.entries.retain(|e| !to_remove.contains(&e.path));
        to_remove.into_iter().collect()
    }
}

// ---------------------------------------------------------------------------
// File lock
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct LockGuard {
    v2_file: fs::File,
    legacy_path: PathBuf,
    legacy_token: String,
}

/// Exclusive guard for the complete run-publication transaction.
///
/// Keeping this distinct from `LockGuard` makes it impossible for callers to
/// accidentally use an unrelated per-file lock as proof that `latest` and the
/// global run index are serialized together.
#[derive(Debug)]
pub(crate) struct RunPublicationLock {
    _guard: LockGuard,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Release v2 first. A new contender that enters this tiny interval still
        // sees our live legacy sentinel and backs off; an old contender does the
        // same. Removing the sentinel before unlocking v2 would instead let an
        // old binary enter while this guard still owned the new protocol.
        let _ = fs::File::unlock(&self.v2_file);
        if let Ok(mut file) = open_regular_lock_file(&self.legacy_path, false, false) {
            let mut content = String::new();
            if file.read_to_string(&mut content).is_ok() && content.trim() == self.legacy_token {
                let _ = fs::remove_file(&self.legacy_path);
            }
        }
    }
}

/// Acquire a file lock on index.jsonl.lock.
///
/// The pathname is persistent; ownership is the OS lock on its open handle.
/// Kernel release on process death avoids stale-lock deletion and its
/// three-racer TOCTOU entirely.
pub fn acquire_lock() -> Result<LockGuard> {
    acquire_lock_at(&lock_path())
}

/// Wait for the global publication lock while the run remains active.
///
/// Concurrent successful publishers must take this lock before changing
/// either the pack-level `latest` alias or `index.jsonl`. Unlike the lower-level
/// non-blocking lock primitive, this operation is cancellation-aware and has no
/// fixed timeout: kernel-owned locks cannot become stale after process death.
pub(crate) fn acquire_publication_lock(abort: impl Fn() -> bool) -> Result<RunPublicationLock> {
    loop {
        if abort() {
            return Err(crate::governor::Cancelled.into());
        }
        match acquire_lock() {
            Ok(guard) => return Ok(RunPublicationLock { _guard: guard }),
            Err(error) if lock_is_busy(&error) => {
                #[cfg(test)]
                publication_lock_wait_test_hook::observe();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod publication_lock_wait_test_hook {
    use std::cell::RefCell;
    use std::sync::mpsc::Sender;

    thread_local! {
        static WAITING: RefCell<Option<Sender<()>>> = const { RefCell::new(None) };
    }

    pub(crate) struct WaitGuard;

    impl WaitGuard {
        pub(crate) fn install(sender: Sender<()>) -> Self {
            WAITING.with(|slot| {
                assert!(
                    slot.borrow_mut().replace(sender).is_none(),
                    "nested lock-wait probe"
                );
            });
            Self
        }
    }

    impl Drop for WaitGuard {
        fn drop(&mut self) {
            WAITING.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    pub(crate) fn observe() {
        WAITING.with(|slot| {
            if let Some(sender) = slot.borrow_mut().take() {
                let _ = sender.send(());
            }
        });
    }
}

#[cfg(test)]
pub(crate) use publication_lock_wait_test_hook::WaitGuard as PublicationLockWaitGuard;

fn lock_is_busy(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .starts_with("Index lock held by another live process")
}

/// Acquire a file lock at an explicit path.
///
/// The original pathname remains a create-new PID sentinel understood by
/// pre-0.8 binaries. New binaries additionally serialize on a persistent v2
/// OS-lock inode. A live old owner therefore blocks a new contender even though
/// it never called File::try_lock, while new contenders cannot split ownership
/// by renaming a shared OS-lock pathname.
pub fn acquire_lock_at(path: &Path) -> Result<LockGuard> {
    let path = path.to_path_buf();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let token = format!(
        "{}:{}:{}",
        std::process::id(),
        u128::MAX,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let v2_path = v2_lock_path(&path);

    let mut v2_file = open_regular_lock_file(&v2_path, true, false)
        .with_context(|| format!("Failed to open lock {}", v2_path.display()))?;

    match v2_file.try_lock() {
        Ok(()) => {
            v2_file.set_len(0)?;
            v2_file.rewind()?;
            v2_file.write_all(token.as_bytes())?;
            v2_file.flush()?;
        }
        Err(fs::TryLockError::WouldBlock) => {
            let _ = v2_file.rewind();
            let mut owner = String::new();
            let _ = v2_file.read_to_string(&mut owner);
            bail!(
                "Index lock held by another live process ({}) at {}",
                owner.trim(),
                v2_path.display()
            )
        }
        Err(fs::TryLockError::Error(error)) => {
            return Err(error).with_context(|| format!("Failed to lock {}", v2_path.display()));
        }
    }

    acquire_legacy_sentinel(&path, &token)?;
    Ok(LockGuard {
        v2_file,
        legacy_path: path,
        legacy_token: token,
    })
}

fn v2_lock_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.v2",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("prview.lock")
    ))
}

/// Claim the pre-0.8 create-new sentinel without removing, renaming, or
/// rewriting an existing pathname during acquisition.
///
/// A stale legacy sentinel cannot be taken over safely while a pre-0.8 process
/// may still be running: that process can have observed the old token, pause,
/// and later rename or remove whichever sentinel replaced it. The v2 kernel
/// lock cannot protect against a binary that does not know it exists. Therefore
/// migration is deliberately fail-closed. An operator may remove a stale
/// sentinel only after establishing that no pre-0.8 publisher is still alive.
fn acquire_legacy_sentinel(path: &Path, token: &str) -> Result<()> {
    match open_regular_lock_file(path, false, true) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.flush()?;
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let mut file = open_regular_lock_file(path, false, false).with_context(|| {
                format!("Failed to open existing legacy lock {}", path.display())
            })?;
            let mut observed = String::new();
            file.read_to_string(&mut observed)?;
            if lock_is_stale(&observed) {
                bail!(
                    "Stale legacy index lock at {} ({}) requires manual removal after verifying no pre-0.8 prview publisher is running",
                    path.display(),
                    observed.trim()
                );
            }
            bail!(
                "Index lock held by another live process ({}) at {}",
                observed.trim(),
                path.display()
            )
        }
        Err(error) => {
            Err(error).with_context(|| format!("Failed to create lock {}", path.display()))
        }
    }
}

fn open_regular_lock_file(
    path: &Path,
    create: bool,
    create_new: bool,
) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .create_new(create_new)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(format!(
            "lock path is not a regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(std::io::Error::other(format!(
                "lock path has {} hard links; refusing shared inode: {}",
                metadata.nlink(),
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        if is_reparse_point(&metadata) {
            return Err(std::io::Error::other(format!(
                "lock path is a reparse point: {}",
                path.display()
            )));
        }
    }
    Ok(file)
}

/// Windows directory junctions and mount points are reparse points but are not
/// necessarily reported as Rust symlinks. Treat every reparse point as linked
/// storage so retention authority never follows or recursively deletes it.
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[cfg(windows)]
fn is_directory_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    is_reparse_point(metadata) && metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0
}

fn is_owned_regular_dir(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_dir()
        && !metadata.file_type().is_symlink()
        && !is_reparse_point(metadata)
}

fn is_owned_regular_file(metadata: &fs::Metadata) -> bool {
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || is_reparse_point(metadata)
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() == 1
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A legacy PID token is classified stale when its owner is gone or its age
/// exceeds the old protocol's one-hour recycling guard. Classification is
/// diagnostic only: existing legacy sentinels fail closed and require manual
/// recovery. New sentinels use a future timestamp so an old binary never
/// expires a legitimately long v2 hold while the owning process is alive.
const LOCK_STALE_MAX_AGE_NANOS: u128 = 3600 * 1_000_000_000;

fn lock_is_stale(content: &str) -> bool {
    let mut parts = content.trim().split(':');
    let pid = parts.next().and_then(|part| part.parse::<u32>().ok());
    let created_nanos = parts.next().and_then(|part| part.parse::<u128>().ok());
    let Some(pid) = pid else {
        return true;
    };
    if !is_process_alive(pid) {
        return true;
    }
    created_nanos.is_some_and(|created| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        now.saturating_sub(created) > LOCK_STALE_MAX_AGE_NANOS
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether a process is alive using the native platform probe.
///
/// Shared with the MCP run-liveness reader (`mcp::read::run_status`) which
/// derives deep-run status deterministically from a pid marker.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    // pid 0 is never a real owner: it is our unknown-pid sentinel, and
    // `kill(0, 0)` targets the *caller's whole process group* — always
    // succeeding, which would make a pid-0 marker an immortal "running".
    if pid == 0 {
        return false;
    }

    #[cfg(unix)]
    {
        unix_process_alive_with(|| {
            // kill(pid, 0) checks if a process exists without sending a signal.
            // SAFETY: signal 0 performs only the liveness/permission probe;
            // `pid` is a non-zero process identifier supplied by a marker.
            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error().raw_os_error())
            }
        })
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        // SAFETY: the call receives a non-zero PID, requests only the right to
        // wait on the process object, and does not inherit the returned handle.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return windows_open_failure_may_be_live(
                std::io::Error::last_os_error().raw_os_error(),
            );
        }

        // SAFETY: `handle` grants synchronization access and a zero timeout is
        // a non-blocking status probe of the process object's signaled state.
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        // SAFETY: `handle` was returned by `OpenProcess` above and is closed
        // exactly once after the wait, regardless of whether the wait worked.
        let _ = unsafe { CloseHandle(handle) };

        match wait {
            WAIT_OBJECT_0 => false,
            WAIT_TIMEOUT => true,
            // Any unexpected wait status is indeterminate. Retain the marker
            // rather than falsely declaring a possibly-live owner dead.
            _ => true,
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

#[cfg(unix)]
fn unix_process_alive_with(mut probe: impl FnMut() -> Result<(), Option<i32>>) -> bool {
    loop {
        match probe() {
            Ok(()) => return true,
            Err(Some(libc::EINTR)) => continue,
            Err(Some(libc::ESRCH)) => return false,
            // EPERM and every indeterminate probe failure retain ownership.
            // Only ESRCH is affirmative evidence that the process is gone.
            Err(_) => return true,
        }
    }
}

#[cfg(windows)]
fn windows_open_failure_may_be_live(error: Option<i32>) -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};

    // OpenProcess documents INVALID_PARAMETER for an invalid/nonexistent PID.
    // ACCESS_DENIED instead means the process can exist but is protected; all
    // other/unknown query failures are likewise indeterminate and conservative.
    if error == Some(ERROR_ACCESS_DENIED as i32) {
        return true;
    }
    error != Some(ERROR_INVALID_PARAMETER as i32)
}

/// Persist one directory's entries so a preceding create, rename or removal is
/// durable before a transaction advances to its next commit record.
fn fsync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let directory = fs::File::open(path)
            .with_context(|| format!("Failed to open directory {} for fsync", path.display()))?;
        directory
            .sync_all()
            .with_context(|| format!("Failed to fsync directory {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn fsync_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fsync_directory(parent)?;
    }
    Ok(())
}

fn fsync_rename_parents(source: &Path, destination: &Path) -> Result<()> {
    if let Some(source_parent) = source.parent() {
        fsync_directory(source_parent)?;
    }
    if destination.parent() != source.parent()
        && let Some(destination_parent) = destination.parent()
    {
        fsync_directory(destination_parent)?;
    }
    Ok(())
}

static TEMP_FILE_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Create a uniquely named, owned regular temp file without following links.
pub(crate) fn create_owned_temp_file(parent: &Path, prefix: &str) -> Result<(PathBuf, fs::File)> {
    fs::create_dir_all(parent)?;
    for _ in 0..16 {
        let nonce = TEMP_FILE_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = parent.join(format!(
            ".{prefix}.{}.{}.{nonce}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        match open_regular_lock_file(&path, false, true) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!(
        "failed to create a unique owned temp file in {}",
        parent.display()
    )
}

/// Atomically publish an owned temp file over its destination.
pub(crate) fn atomic_replace_file(temp: &Path, destination: &Path) -> Result<()> {
    fs::rename(temp, destination).with_context(|| {
        format!(
            "Failed to atomically replace {} with {}",
            destination.display(),
            temp.display()
        )
    })?;
    fsync_parent_dir(destination)?;
    Ok(())
}

fn read_subdirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string()
}

fn branch_lookup_keys(branch: &str) -> Vec<String> {
    let primary = branch_storage_key(branch);
    let legacy = branch.replace('/', "-");

    if primary == legacy {
        vec![primary]
    } else {
        vec![primary, legacy]
    }
}

/// Build RunEntry from an existing run directory by reading report.json.
fn entry_from_disk(run_dir: &Path, id: &str, repo: &str, branch: &str) -> Option<RunEntry> {
    let report_path = run_dir.join("report.json");
    let size = dir_size(run_dir);
    let has_dashboard = run_dir.join("dashboard.html").exists();

    // Try to extract data from report.json
    if let Ok(data) = fs::read_to_string(&report_path)
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&data)
    {
        let commit = val
            .pointer("/meta/range/target/commit")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let quality_pass = val
            .pointer("/gate/quality_pass")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let merge_status = match val
            .pointer("/gate/merge_recommendation")
            .and_then(|v| v.as_str())
        {
            Some("approve") => "ALLOW",
            Some("review_required") => "HOLD",
            Some("block") => "BLOCK",
            _ => val
                .pointer("/gate/verdict")
                .and_then(|v| v.as_str())
                .map(|verdict| match verdict {
                    "PASS" | "ALLOW" => "ALLOW",
                    "HOLD" | "CONDITIONAL" => "HOLD",
                    _ => "BLOCK",
                })
                .unwrap_or("BLOCK"),
        };
        let policy_mode = val
            .pointer("/gate/policy_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("shadow")
            .to_string();
        let checks_passed = val
            .pointer("/checks")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("PASS"))
                    .count()
            })
            .unwrap_or(0);
        // Count only FAIL/ERROR as failed, matching how a run is registered
        // (Failed|Error). `total - passed` wrongly folded WARN and SKIP into
        // failed, so a rebuild reported a different failed count than the
        // original registration for any run that warned or skipped a check.
        let checks_failed = val
            .pointer("/checks")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|c| {
                        matches!(
                            c.get("status").and_then(|s| s.as_str()),
                            Some("FAIL") | Some("ERROR")
                        )
                    })
                    .count()
            })
            .unwrap_or(0);
        let files_changed = val
            .pointer("/diff/stats/files_changed")
            .or_else(|| val.pointer("/diff/files_changed"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let created_at = val
            .pointer("/meta/generated_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        return Some(RunEntry {
            id: id.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            commit: crate::git::short_sha(&commit).to_string(),
            path: run_dir.to_path_buf(),
            created_at,
            quality_pass,
            merge_status: merge_status.to_string(),
            policy_mode,
            checks_passed,
            checks_failed,
            files_changed,
            size_bytes: size,
            has_dashboard,
        });
    }

    // Fallback: no report.json, use minimal data from filesystem
    let created_at = id_to_iso(id).unwrap_or_default();
    Some(RunEntry {
        id: id.to_string(),
        repo: repo.to_string(),
        branch: branch.to_string(),
        commit: String::new(),
        path: run_dir.to_path_buf(),
        created_at,
        quality_pass: false,
        merge_status: "N/A".to_string(),
        policy_mode: "shadow".to_string(),
        checks_passed: 0,
        checks_failed: 0,
        files_changed: 0,
        size_bytes: size,
        has_dashboard,
    })
}

/// Calculate total directory size in bytes.
fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Convert timestamp ID like "20260305-022829" to ISO 8601.
fn id_to_iso(id: &str) -> Option<String> {
    if id.len() >= 15 {
        let dt = chrono::NaiveDateTime::parse_from_str(id, "%Y%m%d-%H%M%S").ok()?;
        Some(dt.format("%Y-%m-%dT%H:%M:%S").to_string())
    } else {
        None
    }
}

/// Format bytes as human-readable size (e.g. "1.2M", "942K").
pub fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1}G", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1}M", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

// ---------------------------------------------------------------------------
// Subcommand: `prview runs`
// ---------------------------------------------------------------------------

pub struct RunsOpts {
    pub all: bool,
    pub branch: Option<String>,
    pub status: Option<String>,
    pub json: bool,
    pub rebuild: bool,
}

pub fn run_runs_command(opts: &RunsOpts) -> Result<()> {
    let index = if opts.rebuild {
        eprintln!("Rebuilding index from disk...");
        // Hold the lock across the disk scan AND the save so a concurrent
        // register_and_prune cannot have its freshly appended entry clobbered
        // by the rebuilt snapshot.
        let _lock = acquire_lock()?;
        let idx = RunIndex::rebuild();
        idx.save()?;
        eprintln!("Rebuilt index with {} entries", idx.entries().len());
        idx
    } else {
        // The stale-entry cleanup is a read-modify-write: load, drop entries
        // whose run directory is gone, then save. Serialize the whole cycle
        // under the index lock so it cannot overwrite an entry a concurrent
        // register_and_prune appended between our load and our save
        // (P1: lost-update race on index.jsonl).
        //
        // A pure reader (e.g. the MCP server) needs no lock: save() renames the
        // temp file atomically, so a reader always sees a complete index. When a
        // live writer already holds the lock we skip the opportunistic cleanup
        // and just display what is on disk, rather than blocking or failing a
        // read-oriented command.
        match acquire_lock() {
            Ok(_lock) => {
                let mut idx = RunIndex::load_strict()?;
                let before = idx.entries().len();
                idx.remove_stale();
                if idx.entries().len() < before {
                    idx.save()?;
                }
                idx
            }
            Err(_) => RunIndex::load(),
        }
    };

    let mut entries: Vec<&RunEntry> = if opts.all {
        index.entries().iter().collect()
    } else {
        // Detect current repo name from cwd
        let repo_name = detect_current_repo_name();
        if let Some(ref branch) = opts.branch {
            index.list_for_branch(&repo_name, branch)
        } else {
            index.list_for_repo(&repo_name)
        }
    };

    // Apply status filter
    if let Some(ref status) = opts.status {
        let want_pass = status.to_lowercase() == "pass" || status.to_lowercase() == "ok";
        entries.retain(|e| e.quality_pass == want_pass);
    }

    if opts.json {
        let json = serde_json::to_string_pretty(&entries)?;
        if let Err(err) = write_json_stdout(&json)
            && err.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(err.into());
        }
        return Ok(());
    }

    if entries.is_empty() {
        println!("No runs found.");
        return Ok(());
    }

    // Group by (repo, branch)
    print_runs_table(&entries);

    Ok(())
}

fn write_json_stdout(json: &str) -> std::io::Result<()> {
    use std::io::Write;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{json}")
}

fn print_runs_table(entries: &[&RunEntry]) {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<(String, String), Vec<&RunEntry>> = BTreeMap::new();
    for e in entries {
        groups
            .entry((e.repo.clone(), e.branch.clone()))
            .or_default()
            .push(e);
    }

    for ((repo, branch), runs) in &groups {
        println!("{} / {}", repo, branch);
        for run in runs.iter().rev() {
            let status = if run.quality_pass {
                "\u{2713}"
            } else {
                "\u{2717}"
            };
            let status_label = if run.quality_pass { "PASS" } else { "FAIL" };
            let checks_total = run.checks_passed + run.checks_failed;
            println!(
                "  {}  {} {}  {}/{} checks  {} files  {}",
                run.id,
                status,
                status_label,
                run.checks_passed,
                checks_total,
                run.files_changed,
                format_size(run.size_bytes),
            );
        }
        println!();
    }
}

fn detect_current_repo_name() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| find_repo_root_from(&cwd).ok())
        .map(|repo_root| repo_name_from_root(&repo_root))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        })
        .unwrap_or_else(|| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// Subcommand: `prview open`
// ---------------------------------------------------------------------------

pub struct OpenOpts {
    pub run_id: Option<String>,
    pub dir_only: bool,
}

pub fn run_open_command(opts: &OpenOpts) -> Result<()> {
    let index = RunIndex::load();
    let repo_name = detect_current_repo_name();
    let branch_name = detect_current_branch();

    let entry = if let Some(ref run_id) = opts.run_id {
        // Find by run ID (across all branches of current repo)
        index
            .entries()
            .iter()
            .find(|e| e.id == *run_id && e.repo == repo_name)
            .or_else(|| {
                // Try all repos
                index.entries().iter().find(|e| e.id == *run_id)
            })
    } else {
        // Latest for current repo+branch
        index.latest(&repo_name, &branch_name)
    };

    let entry = match entry {
        Some(e) => e,
        None => {
            if let Some(ref id) = opts.run_id {
                bail!("Run '{}' not found in index", id);
            }
            bail!(
                "No runs found for {} / {}. Run `prview` first or try `prview runs --all`",
                repo_name,
                branch_name
            );
        }
    };

    let dashboard = entry.path.join("dashboard.html");

    if opts.dir_only {
        println!("{}", entry.path.display());
        return Ok(());
    }

    if !dashboard.exists() {
        bail!(
            "Dashboard not found at {}. Use `prview open --dir` for the directory path.",
            dashboard.display()
        );
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg(&dashboard)
            .status()
            .context("Failed to open dashboard")?;
        ensure_opener_succeeded("open", &dashboard, status)?;
    }
    #[cfg(not(target_os = "macos"))]
    {
        // xdg-open on Linux
        let status = std::process::Command::new("xdg-open")
            .arg(&dashboard)
            .status()
            .context("Failed to open dashboard")?;
        ensure_opener_succeeded("xdg-open", &dashboard, status)?;
    }

    Ok(())
}

fn ensure_opener_succeeded(
    opener: &str,
    dashboard: &Path,
    status: std::process::ExitStatus,
) -> Result<()> {
    if status.success() {
        return Ok(());
    }

    match status.code() {
        Some(code) => bail!(
            "{} exited with status {} while opening {}",
            opener,
            code,
            dashboard.display()
        ),
        None => bail!(
            "{} terminated before opening {}",
            opener,
            dashboard.display()
        ),
    }
}

fn detect_current_branch() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| find_repo_root_from(&cwd).ok())
        .and_then(|repo_root| current_branch_name(&repo_root))
        .unwrap_or_else(|| "HEAD".to_string())
}

// ---------------------------------------------------------------------------
// Integration: register a run after generate()
// ---------------------------------------------------------------------------

/// Register a completed run in the index and prune old runs.
///
/// Standalone callers acquire the shared publication lock here. Artifact
/// generation uses `register_and_prune_locked` so it can include `latest` in
/// the same transaction.
/// Returns the number of pruned directories.
pub fn register_and_prune(
    out_dir: &Path,
    entry: RunEntry,
    emit_human_stdout: bool,
    abort: impl Fn() -> bool,
) -> Result<usize> {
    let publication = acquire_publication_lock(&abort)?;
    register_and_prune_with_policy_locked(
        &publication,
        out_dir,
        entry,
        emit_human_stdout,
        &RetentionPolicy::default(),
        abort,
    )
}

#[cfg(test)]
fn register_and_prune_with_policy(
    out_dir: &Path,
    entry: RunEntry,
    emit_human_stdout: bool,
    policy: &RetentionPolicy,
    abort: impl Fn() -> bool,
) -> Result<usize> {
    let publication = acquire_publication_lock(&abort)?;
    register_and_prune_with_policy_locked(
        &publication,
        out_dir,
        entry,
        emit_human_stdout,
        policy,
        abort,
    )
}

/// Register a completed run while the caller retains the shared publication
/// lock across both the `latest` alias and index mutations.
pub(crate) fn register_and_prune_locked(
    publication: &RunPublicationLock,
    out_dir: &Path,
    entry: RunEntry,
    emit_human_stdout: bool,
    abort: impl Fn() -> bool,
) -> Result<usize> {
    register_and_prune_with_policy_locked(
        publication,
        out_dir,
        entry,
        emit_human_stdout,
        &RetentionPolicy::default(),
        abort,
    )
}

fn register_and_prune_with_policy_locked(
    _publication: &RunPublicationLock,
    out_dir: &Path,
    entry: RunEntry,
    emit_human_stdout: bool,
    policy: &RetentionPolicy,
    abort: impl Fn() -> bool,
) -> Result<usize> {
    if abort() {
        bail!("publication aborted before the run index was committed");
    }
    let mut index = RunIndex::load_strict()?;
    // A previous process may have died after moving retained runs but before
    // committing their removal. Reconcile durable tombstone metadata against
    // the still-persisted index before starting another transaction.
    cleanup_committed_prunes(&index, &abort)?;

    let previous = index.clone();
    let current_entry = entry.clone();
    index.append(entry);
    let pruned = index.prune(policy, out_dir);
    let pruned_count = pruned.len();

    // A deletion cannot be rolled back. Move each predecessor atomically into
    // private prune-trash first; cancellation before the index commit restores
    // every move, so a cancelled publication cannot destroy historical proof.
    if abort() {
        bail!("publication aborted before the run index was committed");
    }
    let staged = match stage_pruned_paths(&pruned, &abort) {
        Ok(staged) => staged,
        Err(error) if !abort() => {
            // A custom --output-dir may be on another filesystem, where an
            // atomic rename into PRVIEW_HOME is impossible. Keep every old row
            // and publish the new one without retention rather than falling
            // back to non-transactional copy/delete or losing index truth.
            eprintln!(
                "prview: retention skipped; could not stage old runs transactionally: {error:#}"
            );
            let mut unpruned = previous.clone();
            unpruned.append(current_entry);
            if abort() {
                bail!("publication aborted before the run index was committed");
            }
            if let Err(error) = unpruned.save() {
                if let Err(rollback_error) = rollback_retention_transaction(&[], &previous) {
                    if is_unconfirmed_publication_rollback(&rollback_error) {
                        return Err(rollback_error).context(format!(
                            "Fallback run index commit failed and its previous state could not be confirmed: {error:#}"
                        ));
                    }
                    return Err(error).context(format!(
                        "Fallback run index commit failed and rollback also failed: {rollback_error:#}"
                    ));
                }
                return Err(error);
            }
            if abort() {
                rollback_retention_transaction(&[], &previous).context(
                    "Fallback publication was cancelled but its index rollback was not confirmed",
                )?;
                bail!("publication aborted before the run index was committed");
            }
            return Ok(0);
        }
        Err(error) => return Err(error),
    };
    if let Err(error) = index.save() {
        if let Err(rollback_error) = rollback_retention_transaction(&staged, &previous) {
            if is_unconfirmed_publication_rollback(&rollback_error) {
                return Err(rollback_error).context(format!(
                    "Run index commit failed and its previous state could not be confirmed: {error:#}"
                ));
            }
            return Err(error).context(format!(
                "Run index commit failed and retained-run rollback also failed: {rollback_error:#}"
            ));
        }
        return Err(error);
    }

    // After the index rename the new row is visible. Roll both the file and the
    // reversible directory moves back if cancellation won the final race.
    if abort() {
        rollback_retention_transaction(&staged, &previous)
            .context("Publication was cancelled but its index rollback was not fully confirmed")?;
        bail!("publication aborted before the run index was committed");
    }

    // The index rename above is the commit point. Persist that state for fast,
    // unambiguous restart cleanup; if a process dies in this tiny update window,
    // recovery still reconciles the staged marker against the committed index.
    if let Err(error) = mark_staged_prunes_committed(&staged) {
        // Marker publication is part of the retention transaction. The run
        // index rename is still reversible because physical deletion is
        // deferred: restore every predecessor first, then put the old index
        // back. If either rollback step fails, the durable markers still let a
        // later run recover conservatively without deleting uncertain evidence.
        rollback_retention_transaction(&staged, &previous)
            .context("Failed to roll back retention after commit-marker failure")?;
        return Err(error).context("Failed to commit retention transaction markers");
    }

    // The committed tombstones are physically removed at the beginning of the
    // next registration, before that run mutates its index. This keeps the
    // long recursive deletion outside the current publication transaction.

    if emit_human_stdout && pruned_count > 0 {
        use colored::Colorize;
        println!(
            "  {} Pruned {} old run{}",
            "\u{267b}".green(),
            pruned_count,
            if pruned_count == 1 { "" } else { "s" },
        );
    }

    Ok(pruned_count)
}

#[derive(Debug)]
struct StagedPrune {
    original: PathBuf,
    tombstone: PathBuf,
    transaction_dir: PathBuf,
}

const PRUNE_MANIFEST_FILE: &str = "transaction.json";
const PRUNE_PAYLOAD_DIR: &str = "run";
const PRUNE_TRANSACTION_PREFIX: &str = "transaction-";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PruneTransactionState {
    Staged,
    Committed,
}

/// A restart-readable prune transaction. Paths use native units rather than a
/// lossy display string, so an otherwise valid non-UTF-8 output path can still
/// be restored byte-for-byte on Unix (and code-unit-for-code-unit on Windows).
#[derive(Debug, Serialize, Deserialize)]
struct PruneTransactionManifest {
    schema: u8,
    original: DurablePath,
    state: PruneTransactionState,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "units", rename_all = "snake_case")]
enum DurablePath {
    #[cfg(unix)]
    UnixBytes(Vec<u8>),
    #[cfg(windows)]
    WindowsWide(Vec<u16>),
    #[cfg(not(any(unix, windows)))]
    Utf8(String),
}

impl DurablePath {
    #[cfg(unix)]
    fn capture(path: &Path) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        Ok(Self::UnixBytes(path.as_os_str().as_bytes().to_vec()))
    }

    #[cfg(windows)]
    fn capture(path: &Path) -> Result<Self> {
        use std::os::windows::ffi::OsStrExt;
        Ok(Self::WindowsWide(path.as_os_str().encode_wide().collect()))
    }

    #[cfg(not(any(unix, windows)))]
    fn capture(path: &Path) -> Result<Self> {
        path.to_str()
            .map(|path| Self::Utf8(path.to_owned()))
            .ok_or_else(|| anyhow!("retained run path is not valid UTF-8: {}", path.display()))
    }

    #[cfg(unix)]
    fn to_path_buf(&self) -> Result<PathBuf> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let Self::UnixBytes(bytes) = self;
        Ok(PathBuf::from(OsString::from_vec(bytes.clone())))
    }

    #[cfg(windows)]
    fn to_path_buf(&self) -> Result<PathBuf> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        let Self::WindowsWide(units) = self;
        Ok(PathBuf::from(OsString::from_wide(units)))
    }

    #[cfg(not(any(unix, windows)))]
    fn to_path_buf(&self) -> Result<PathBuf> {
        let Self::Utf8(path) = self;
        Ok(PathBuf::from(path))
    }
}

fn stage_pruned_paths(pruned: &[PathBuf], abort: &impl Fn() -> bool) -> Result<Vec<StagedPrune>> {
    #[cfg(test)]
    if take_test_prune_stage_failure() {
        bail!("injected prune staging failure");
    }
    let mut candidates = Vec::new();
    for path in pruned {
        match fs::symlink_metadata(path) {
            Ok(metadata) if is_owned_regular_dir(&metadata) => {
                validate_prune_payload_identity(path, path).with_context(|| {
                    format!(
                        "Refusing to stage an index path without owned pack identity: {}",
                        path.display()
                    )
                })?;
                candidates.push(path);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // A genuinely missing stale row has no payload to preserve.
            }
            Ok(_) => bail!(
                "Refusing to stage a non-directory or linked index path: {}",
                path.display()
            ),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to inspect retained run path {}", path.display())
                });
            }
        }
    }
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let trash = prune_trash_path();
    fs::create_dir_all(&trash)
        .with_context(|| format!("Failed to create prune trash {}", trash.display()))?;
    fsync_parent_dir(&trash)?;
    fsync_directory(&trash)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut staged = Vec::new();
    for (index, original) in candidates.into_iter().enumerate() {
        let transaction_dir = trash.join(format!(
            "{PRUNE_TRANSACTION_PREFIX}{}-{nonce}-{index}",
            std::process::id()
        ));
        let durable_original = match DurablePath::capture(original) {
            Ok(path) => path,
            Err(error) => {
                rollback_staged_prunes(&staged)?;
                return Err(error);
            }
        };
        let manifest = PruneTransactionManifest {
            schema: 1,
            original: durable_original,
            state: PruneTransactionState::Staged,
        };
        if let Err(error) = fs::create_dir(&transaction_dir) {
            rollback_staged_prunes(&staged)?;
            return Err(error).with_context(|| {
                format!(
                    "Failed to create prune transaction {}",
                    transaction_dir.display()
                )
            });
        }
        if let Err(error) = write_prune_manifest(&transaction_dir, &manifest) {
            let _ = fs::remove_dir_all(&transaction_dir);
            rollback_staged_prunes(&staged)?;
            return Err(error);
        }
        if let Err(error) = fsync_directory(&trash) {
            let _ = fs::remove_dir_all(&transaction_dir);
            rollback_staged_prunes(&staged)?;
            return Err(error).context("Failed to persist prune transaction directory");
        }

        let tombstone = transaction_dir.join(PRUNE_PAYLOAD_DIR);
        if let Err(error) = fs::rename(original, &tombstone) {
            let _ = fs::remove_dir_all(&transaction_dir);
            rollback_staged_prunes(&staged)?;
            return Err(error).with_context(|| {
                format!(
                    "Failed to stage retained run {} as {}",
                    original.display(),
                    tombstone.display()
                )
            });
        }
        staged.push(StagedPrune {
            original: original.clone(),
            tombstone,
            transaction_dir,
        });
        let staged_prune = staged.last().expect("the just-staged prune is present");
        if let Err(error) = fsync_rename_parents(&staged_prune.original, &staged_prune.tombstone) {
            rollback_staged_prunes(&staged)?;
            return Err(error).context("Failed to persist staged retention rename");
        }
        if abort() {
            rollback_staged_prunes(&staged)?;
            bail!("publication aborted before the run index was committed");
        }
    }
    Ok(staged)
}

fn rollback_staged_prunes(staged: &[StagedPrune]) -> Result<()> {
    for prune in staged.iter().rev() {
        if prune.tombstone.exists() && prune.original.exists() {
            bail!(
                "Cannot restore retained run {} because both it and tombstone {} exist",
                prune.original.display(),
                prune.tombstone.display()
            );
        }
        if prune.tombstone.exists() {
            fs::rename(&prune.tombstone, &prune.original).with_context(|| {
                format!(
                    "Failed to restore retained run {} from {}",
                    prune.original.display(),
                    prune.tombstone.display()
                )
            })?;
            fsync_rename_parents(&prune.tombstone, &prune.original)?;
        }
        if prune.transaction_dir.exists() {
            fs::remove_dir_all(&prune.transaction_dir).with_context(|| {
                format!(
                    "Failed to remove prune transaction {}",
                    prune.transaction_dir.display()
                )
            })?;
            fsync_parent_dir(&prune.transaction_dir)?;
        }
    }
    Ok(())
}

#[derive(Debug)]
struct UnconfirmedPublicationRollback {
    details: String,
}

impl std::fmt::Display for UnconfirmedPublicationRollback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "publication rollback is unconfirmed: {}",
            self.details
        )
    }
}

impl std::error::Error for UnconfirmedPublicationRollback {}

pub(crate) fn is_unconfirmed_publication_rollback(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<UnconfirmedPublicationRollback>()
            .is_some()
    })
}

/// Always attempt to restore the previous index even when moving a staged
/// predecessor back fails. If the index restore itself fails, callers must
/// retain the outer publication journal: only a later read of durable state can
/// decide whether the old or new publication won.
fn rollback_retention_transaction(staged: &[StagedPrune], previous: &RunIndex) -> Result<()> {
    let staged_error = rollback_staged_prunes(staged).err();
    let index_error = previous.save().err();
    match (staged_error, index_error) {
        (None, None) => Ok(()),
        (Some(error), None) => Err(error),
        (staged_error, Some(index_error)) => Err(anyhow!(UnconfirmedPublicationRollback {
            details: match staged_error {
                Some(staged_error) => format!(
                    "retained-run restore failed ({staged_error:#}); previous index restore failed ({index_error:#})"
                ),
                None => format!("previous index restore failed ({index_error:#})"),
            },
        })),
    }
}

fn mark_staged_prunes_committed(staged: &[StagedPrune]) -> Result<()> {
    for prune in staged {
        let mut manifest = read_prune_manifest(&prune.transaction_dir)?;
        manifest.state = PruneTransactionState::Committed;
        write_prune_manifest(&prune.transaction_dir, &manifest)?;
    }
    Ok(())
}

fn write_prune_manifest(transaction_dir: &Path, manifest: &PruneTransactionManifest) -> Result<()> {
    let path = transaction_dir.join(PRUNE_MANIFEST_FILE);
    let (temp, mut file) = create_owned_temp_file(transaction_dir, "prune-manifest")?;
    let write_result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, manifest)?;
        writeln!(file)?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = atomic_replace_file(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(error)
            .with_context(|| format!("Failed to publish prune manifest {}", path.display()));
    }
    fsync_directory(transaction_dir)?;
    Ok(())
}

fn read_prune_manifest(transaction_dir: &Path) -> Result<PruneTransactionManifest> {
    let transaction_metadata = fs::symlink_metadata(transaction_dir).with_context(|| {
        format!(
            "Failed to inspect prune transaction {}",
            transaction_dir.display()
        )
    })?;
    if !is_owned_regular_dir(&transaction_metadata) {
        bail!(
            "Prune transaction is not an owned directory: {}",
            transaction_dir.display()
        );
    }
    let path = transaction_dir.join(PRUNE_MANIFEST_FILE);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("Failed to inspect prune manifest {}", path.display()))?;
    if !is_owned_regular_file(&metadata) {
        bail!(
            "Prune manifest is not an owned regular file: {}",
            path.display()
        );
    }
    let manifest: PruneTransactionManifest = serde_json::from_reader(
        fs::File::open(&path)
            .with_context(|| format!("Failed to open prune manifest {}", path.display()))?,
    )
    .with_context(|| format!("Failed to parse prune manifest {}", path.display()))?;
    if manifest.schema != 1 {
        bail!(
            "Unsupported prune transaction schema {} in {}",
            manifest.schema,
            path.display()
        );
    }
    Ok(manifest)
}

fn cleanup_committed_prunes(index: &RunIndex, abort: &impl Fn() -> bool) -> Result<()> {
    let trash = prune_trash_path();
    let entries = match fs::read_dir(&trash) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read prune trash {}", trash.display()));
        }
    };
    let mut preserved_unknown = false;
    for entry in entries {
        if abort() {
            bail!("publication aborted while cleaning committed retention tombstones");
        }
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if is_owned_regular_dir(&metadata) {
            let manifest_path = path.join(PRUNE_MANIFEST_FILE);
            let has_manifest = fs::symlink_metadata(&manifest_path)
                .is_ok_and(|metadata| is_owned_regular_file(&metadata));
            if has_manifest {
                match read_prune_manifest(&path) {
                    Ok(manifest) => {
                        preserved_unknown |=
                            !reconcile_prune_transaction(&path, &manifest, index, abort)?;
                    }
                    Err(error) => {
                        eprintln!(
                            "prview: preserving prune transaction with unreadable manifest {}: {error:#}",
                            path.display()
                        );
                        preserved_unknown = true;
                    }
                }
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(PRUNE_TRANSACTION_PREFIX))
            {
                // New-format payload moves happen only after the manifest is
                // atomically published. A transaction with neither manifest
                // nor payload is creation/temp residue. If `run` exists, the
                // manifest may instead have been damaged after the rename;
                // preserve the evidence rather than inferring deletion authority.
                if fs::symlink_metadata(path.join(PRUNE_PAYLOAD_DIR)).is_ok() {
                    eprintln!(
                        "prview: preserving manifestless prune transaction with payload: {}",
                        path.display()
                    );
                    preserved_unknown = true;
                } else {
                    remove_dir_all_cancellable(&path, abort)?;
                }
            } else {
                // The old implementation moved a raw run directory here before
                // saving the index. A hard exit in that window leaves no marker,
                // so absence of a manifest is NOT proof of a committed delete.
                preserved_unknown |= !reconcile_legacy_prune(&path, index, abort)?;
            }
        } else if is_reparse_point(&metadata) {
            eprintln!(
                "prview: preserving linked prune-trash entry without traversing it: {}",
                path.display()
            );
            preserved_unknown = true;
        } else {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove prune tombstone {}", path.display()))?;
        }
    }
    if abort() {
        bail!("publication aborted while cleaning committed retention tombstones");
    }
    if preserved_unknown {
        return Ok(());
    }
    match fs::remove_dir(&trash) {
        Ok(()) => fsync_parent_dir(&trash)?,
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("Failed to remove empty prune trash"),
    }
    Ok(())
}

/// Recover a raw tombstone written by the pre-transaction format.
///
/// Completed packs carry their original absolute directory in RUN.json. Only
/// that path joined to a still-persisted index row is enough authority to move
/// the directory back. Missing/invalid identity is preserved fail-closed; a
/// future registration must never turn uncertainty into deletion.
fn reconcile_legacy_prune(
    tombstone: &Path,
    index: &RunIndex,
    abort: &impl Fn() -> bool,
) -> Result<bool> {
    let original = match read_prune_payload_original(tombstone) {
        Ok(original) => original,
        Err(error) => {
            eprintln!(
                "prview: preserving legacy prune tombstone with unknown transaction state {}: {error:#}",
                tombstone.display()
            );
            return Ok(false);
        }
    };

    let is_still_indexed = index.entries().iter().any(|entry| entry.path == original);
    if !is_still_indexed {
        eprintln!(
            "prview: preserving legacy prune tombstone without commit authority: {}",
            tombstone.display()
        );
        return Ok(false);
    }

    if abort() {
        bail!("publication aborted while restoring a legacy retention tombstone");
    }
    if original.exists() {
        eprintln!(
            "prview: preserving conflicting legacy prune tombstone {}; indexed path already exists at {}",
            tombstone.display(),
            original.display()
        );
        return Ok(false);
    }
    fs::rename(tombstone, &original).with_context(|| {
        format!(
            "Failed to recover retained run {} from legacy tombstone {}",
            original.display(),
            tombstone.display()
        )
    })?;
    fsync_rename_parents(tombstone, &original)?;
    Ok(true)
}

fn reconcile_prune_transaction(
    transaction_dir: &Path,
    manifest: &PruneTransactionManifest,
    index: &RunIndex,
    abort: &impl Fn() -> bool,
) -> Result<bool> {
    let original = manifest.original.to_path_buf()?;
    let tombstone = transaction_dir.join(PRUNE_PAYLOAD_DIR);
    if tombstone.exists()
        && let Err(error) = validate_prune_payload_identity(&tombstone, &original)
    {
        eprintln!(
            "prview: preserving prune transaction with mismatched payload identity {}: {error:#}",
            transaction_dir.display()
        );
        return Ok(false);
    }
    let is_still_indexed = index.entries().iter().any(|entry| entry.path == original);
    let must_restore = is_still_indexed || manifest.state == PruneTransactionState::Staged;

    if must_restore {
        if abort() {
            bail!("publication aborted while restoring an uncommitted retention tombstone");
        }
        if tombstone.exists() && original.exists() {
            bail!(
                "Cannot recover retained run {} because both it and tombstone {} exist",
                original.display(),
                tombstone.display()
            );
        }
        if tombstone.exists() {
            fs::rename(&tombstone, &original).with_context(|| {
                format!(
                    "Failed to recover retained run {} from {}",
                    original.display(),
                    tombstone.display()
                )
            })?;
            fsync_rename_parents(&tombstone, &original)?;
        } else if !original.exists() {
            bail!(
                "Indexed retained run {} is missing from both its original path and prune transaction {}",
                original.display(),
                transaction_dir.display()
            );
        }
    }

    // Deletion is allowed only for an explicit committed marker with no index
    // reference. A staged marker without a parsed row is ambiguous (the index
    // may be corrupt, or the process may have died after its rename), so it is
    // restored conservatively. One orphan pack is preferable to lost evidence.
    remove_dir_all_cancellable(transaction_dir, abort)?;
    fsync_parent_dir(transaction_dir)?;
    Ok(true)
}

fn validate_prune_payload_identity(payload: &Path, original: &Path) -> Result<()> {
    let recorded_root = read_prune_payload_original(payload)?;
    // RUN.json is a JSON/string surface and therefore records a lossy display
    // form for a native non-UTF-8 path. The durable manifest still owns exact
    // native bytes; comparing the recorded string to the same display form
    // preserves that recovery path while rejecting ordinary path mismatches.
    let expected_root = original.display().to_string();
    if recorded_root.as_os_str() != std::ffi::OsStr::new(&expected_root) {
        bail!(
            "Prune manifest path {} does not match payload identity {}",
            original.display(),
            recorded_root.display()
        );
    }
    Ok(())
}

fn read_prune_payload_original(payload: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(payload).with_context(|| {
        format!(
            "Prune transaction points to missing payload {}",
            payload.display()
        )
    })?;
    if !is_owned_regular_dir(&metadata) {
        bail!(
            "Prune transaction payload is not an owned directory: {}",
            payload.display()
        );
    }
    let summary_path = payload.join("00_summary");
    let summary_metadata = fs::symlink_metadata(&summary_path).with_context(|| {
        format!(
            "Prune transaction payload has no owned summary directory {}",
            summary_path.display()
        )
    })?;
    if !is_owned_regular_dir(&summary_metadata) {
        bail!(
            "Prune transaction summary is not an owned directory: {}",
            summary_path.display()
        );
    }
    let run_path = summary_path.join("RUN.json");
    let run_metadata = fs::symlink_metadata(&run_path).with_context(|| {
        format!(
            "Prune transaction payload has no identity {}",
            run_path.display()
        )
    })?;
    if !is_owned_regular_file(&run_metadata) {
        bail!(
            "Prune transaction identity is not an owned regular file: {}",
            run_path.display()
        );
    }
    let run: serde_json::Value = serde_json::from_slice(&fs::read(&run_path)?)
        .with_context(|| format!("Invalid prune payload identity {}", run_path.display()))?;
    let recorded_root = run
        .get("artifacts_root")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("Prune payload identity has no artifacts_root"))?;
    Ok(PathBuf::from(recorded_root))
}

fn remove_dir_all_cancellable(path: &Path, abort: &impl Fn() -> bool) -> Result<()> {
    let root_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Failed to inspect prune tombstone {}", path.display()))?;
    if !is_owned_regular_dir(&root_metadata) {
        bail!(
            "Refusing to recursively remove linked or non-directory prune tombstone: {}",
            path.display()
        );
    }
    for entry in fs::read_dir(path)
        .with_context(|| format!("Failed to read prune tombstone {}", path.display()))?
    {
        if abort() {
            bail!("publication aborted while cleaning committed retention tombstones");
        }
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if is_owned_regular_dir(&metadata) {
            remove_dir_all_cancellable(&child, abort)?;
        } else if is_reparse_point(&metadata) {
            // Never recurse through a Windows junction/mount-point. Removing
            // the reparse entry itself preserves its external target.
            #[cfg(windows)]
            if is_directory_reparse_point(&metadata) {
                fs::remove_dir(&child).with_context(|| {
                    format!(
                        "Failed to unlink prune tombstone junction {}",
                        child.display()
                    )
                })?;
            } else {
                fs::remove_file(&child).with_context(|| {
                    format!(
                        "Failed to unlink prune tombstone reparse point {}",
                        child.display()
                    )
                })?;
            }
        } else {
            fs::remove_file(&child).with_context(|| {
                format!("Failed to remove prune tombstone file {}", child.display())
            })?;
        }
    }
    if abort() {
        bail!("publication aborted while cleaning committed retention tombstones");
    }
    fs::remove_dir(path)
        .with_context(|| format!("Failed to remove prune tombstone {}", path.display()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;

        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: u32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;

        std::process::ExitStatus::from_raw(code)
    }

    fn make_entry(id: &str, repo: &str, branch: &str, size: u64) -> RunEntry {
        RunEntry {
            id: id.to_string(),
            repo: repo.to_string(),
            branch: branch.to_string(),
            commit: "abc1234".to_string(),
            path: PathBuf::from(format!("/tmp/test-runs/{}/{}/{}", repo, branch, id)),
            created_at: format!("2026-01-01T00:00:{}Z", id),
            quality_pass: true,
            merge_status: "ALLOW".to_string(),
            policy_mode: "shadow".to_string(),
            checks_passed: 3,
            checks_failed: 0,
            files_changed: 10,
            size_bytes: size,
            has_dashboard: true,
        }
    }

    #[test]
    fn test_load_save_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let idx_path = tmp.path().join("index.jsonl");

        let mut index = RunIndex { entries: vec![] };
        index.append(make_entry("001", "myrepo", "main", 1000));
        index.append(make_entry("002", "myrepo", "feat-x", 2000));
        index.save_to(&idx_path).unwrap();

        assert!(idx_path.exists());

        let loaded = RunIndex::load_from(&idx_path);
        assert_eq!(loaded.entries().len(), 2);
        assert_eq!(loaded.entries()[0].id, "001");
        assert_eq!(loaded.entries()[1].id, "002");
    }

    #[cfg(unix)]
    #[test]
    fn index_save_ignores_a_predictable_temp_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let protected = tmp.path().join("protected.txt");
        fs::write(&protected, "do-not-touch").unwrap();
        symlink(&protected, tmp.path().join("index.jsonl.tmp")).unwrap();
        let index_path = tmp.path().join("index.jsonl");
        let mut index = RunIndex { entries: vec![] };
        index.append(make_entry("001", "repo", "main", 1));

        index.save_to(&index_path).unwrap();

        assert_eq!(fs::read_to_string(&protected).unwrap(), "do-not-touch");
        assert_eq!(RunIndex::load_from(&index_path).entries().len(), 1);
    }

    #[test]
    fn load_skips_corrupt_line_without_truncating_and_save_preserves_survivors() {
        let tmp = tempfile::tempdir().unwrap();
        let idx_path = tmp.path().join("index.jsonl");

        // Three JSONL records; the middle line is a non-UTF-8 byte sequence that
        // `BufRead::lines` returns as `Err`. The old `map_while(Result::ok)`
        // stopped there, dropping record 3 — and the next save persisted the loss.
        let e1 = serde_json::to_string(&make_entry("001", "repo", "main", 1000)).unwrap();
        let e3 = serde_json::to_string(&make_entry("003", "repo", "main", 3000)).unwrap();
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(e1.as_bytes());
        bytes.push(b'\n');
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd]); // invalid UTF-8 line
        bytes.push(b'\n');
        bytes.extend_from_slice(e3.as_bytes());
        bytes.push(b'\n');
        fs::write(&idx_path, &bytes).unwrap();

        let loaded = RunIndex::load_from(&idx_path);
        let ids: Vec<&str> = loaded.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["001", "003"],
            "a corrupt line must skip only itself, not truncate the rest"
        );

        // Re-save the survivors and reload: no silent loss on the round-trip.
        let out_path = tmp.path().join("index2.jsonl");
        loaded.save_to(&out_path).unwrap();
        let reloaded = RunIndex::load_from(&out_path);
        let ids2: Vec<&str> = reloaded.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids2, vec!["001", "003"]);
    }

    #[test]
    fn strict_load_rejects_a_corrupt_index_without_rewriting_it() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let valid = serde_json::to_string(&make_entry("001", "repo", "main", 1000)).unwrap();
        let original = format!("{valid}\nnot-json\n");
        fs::write(index_path(), &original).unwrap();

        let error = match RunIndex::load_strict() {
            Ok(_) => panic!("transactional readers must reject a partial ledger"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("Invalid run index JSON"));
        assert_eq!(fs::read_to_string(index_path()).unwrap(), original);
    }

    #[test]
    fn strict_load_accepts_a_missing_first_run_index() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());

        assert!(RunIndex::load_strict().unwrap().entries().is_empty());
    }

    #[test]
    fn process_liveness_reports_zero_and_current_process_truthfully() {
        assert!(!is_process_alive(0));
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_liveness_reports_an_exited_child_as_dead() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn short-lived Windows child");
        let pid = child.id();
        assert!(child.wait().expect("wait for Windows child").success());

        // `Child` still owns the process handle here, so Windows cannot recycle
        // the PID before the probe observes its terminated exit code.
        assert!(!is_process_alive(pid));
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_liveness_keeps_inaccessible_owners_conservative() {
        use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};

        assert!(windows_open_failure_may_be_live(Some(
            ERROR_ACCESS_DENIED as i32
        )));
        assert!(!windows_open_failure_may_be_live(Some(
            ERROR_INVALID_PARAMETER as i32
        )));
        assert!(windows_open_failure_may_be_live(None));
    }

    #[test]
    fn test_list_for_repo_and_branch() {
        let mut index = RunIndex { entries: vec![] };
        index.append(make_entry("001", "repo-a", "main", 1000));
        index.append(make_entry("002", "repo-a", "feat", 1000));
        index.append(make_entry("003", "repo-b", "main", 1000));

        assert_eq!(index.list_for_repo("repo-a").len(), 2);
        assert_eq!(index.list_for_branch("repo-a", "main").len(), 1);
        assert_eq!(index.list_for_branch("repo-b", "main").len(), 1);
        assert_eq!(index.list_for_branch("repo-c", "main").len(), 0);
    }

    #[test]
    fn test_latest() {
        let mut index = RunIndex { entries: vec![] };
        index.append(make_entry("001", "repo", "main", 1000));
        index.append(make_entry("002", "repo", "main", 2000));

        let latest = index.latest("repo", "main").unwrap();
        assert_eq!(latest.id, "002");
    }

    #[test]
    fn test_prune_per_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("current");
        fs::create_dir_all(&current).unwrap();

        let mut index = RunIndex { entries: vec![] };
        let policy = RetentionPolicy {
            max_runs_per_branch: 3,
            max_runs_per_repo: 100,
            max_total_bytes: u64::MAX,
        };

        // Add 5 runs to same branch
        for i in 0..5 {
            let id = format!("{:03}", i);
            let dir = tmp.path().join(format!("run-{}", i));
            fs::create_dir_all(&dir).unwrap();
            let mut entry = make_entry(&id, "repo", "main", 1000);
            entry.path = dir;
            index.append(entry);
        }

        let pruned = index.prune(&policy, &current);
        // 5 - 3 = 2 should be pruned (oldest first)
        assert_eq!(pruned.len(), 2);
        assert_eq!(index.entries().len(), 3);
        // Remaining should be the 3 newest
        assert_eq!(index.entries()[0].id, "002");
    }

    #[test]
    fn test_prune_protects_current_run() {
        let tmp = tempfile::tempdir().unwrap();

        let mut index = RunIndex { entries: vec![] };
        let policy = RetentionPolicy {
            max_runs_per_branch: 1,
            max_runs_per_repo: 100,
            max_total_bytes: u64::MAX,
        };

        // Current run is oldest, should be protected
        let current = tmp.path().join("run-0");
        fs::create_dir_all(&current).unwrap();
        let mut entry0 = make_entry("000", "repo", "main", 1000);
        entry0.path = current.clone();
        index.append(entry0);

        let dir1 = tmp.path().join("run-1");
        fs::create_dir_all(&dir1).unwrap();
        let mut entry1 = make_entry("001", "repo", "main", 1000);
        entry1.path = dir1;
        index.append(entry1);

        let pruned = index.prune(&policy, &current);
        // Should prune run-1 (not current run-0 even though it's oldest)
        assert_eq!(pruned.len(), 1);
        assert!(index.entries().iter().any(|e| e.path == current));
    }

    #[test]
    fn test_prune_global_size() {
        let tmp = tempfile::tempdir().unwrap();
        let current = tmp.path().join("current");
        fs::create_dir_all(&current).unwrap();

        let mut index = RunIndex { entries: vec![] };
        let policy = RetentionPolicy {
            max_runs_per_branch: 100,
            max_runs_per_repo: 100,
            max_total_bytes: 2500, // 2.5KB limit
        };

        // 3 runs × 1000 bytes = 3000 > 2500
        for i in 0..3 {
            let dir = tmp.path().join(format!("run-{}", i));
            fs::create_dir_all(&dir).unwrap();
            let mut entry = make_entry(&format!("{:03}", i), "repo", "main", 1000);
            entry.path = dir;
            index.append(entry);
        }

        let pruned = index.prune(&policy, &current);
        // Need to free 500 bytes, one run of 1000 bytes is enough
        assert_eq!(pruned.len(), 1);
    }

    #[test]
    fn test_remove_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("existing");
        fs::create_dir_all(&existing).unwrap();

        let mut index = RunIndex { entries: vec![] };
        let mut e1 = make_entry("001", "repo", "main", 1000);
        e1.path = existing;
        let mut e2 = make_entry("002", "repo", "main", 1000);
        e2.path = PathBuf::from("/nonexistent/path/run-002");
        index.append(e1);
        index.append(e2);

        index.remove_stale();
        assert_eq!(index.entries().len(), 1);
        assert_eq!(index.entries()[0].id, "001");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500B");
        assert_eq!(format_size(1024), "1K");
        assert_eq!(format_size(965_100), "942K");
        assert_eq!(format_size(1_200_000), "1.1M");
        assert_eq!(format_size(5_368_709_120), "5.0G");
    }

    #[test]
    fn test_id_to_iso() {
        assert_eq!(
            id_to_iso("20260305-022829"),
            Some("2026-03-05T02:28:29".to_string())
        );
        assert_eq!(id_to_iso("short"), None);
    }

    #[test]
    fn test_entry_from_disk_reads_diff_stats_files_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("20260305-022829");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("report.json"),
            r#"{
                "meta":{"generated_at":"2026-03-05T02:28:29Z","range":{"target":{"commit":"abcdef1234567"}}},
                "gate":{"quality_pass":true,"allow_merge":true,"policy_mode":"warn"},
                "checks":[{"status":"PASS"},{"status":"FAIL"}],
                "diff":{"stats":{"files_changed":7}}
            }"#,
        )
        .unwrap();

        let entry = entry_from_disk(&run_dir, "20260305-022829", "repo", "main").unwrap();
        assert_eq!(entry.files_changed, 7);
        assert_eq!(entry.checks_passed, 1);
        assert_eq!(entry.checks_failed, 1);
    }

    #[test]
    fn test_entry_from_disk_counts_only_failed_and_errored_as_failed() {
        // A rebuild must classify checks the same way registration does: only
        // FAIL/ERROR are failed. WARN and SKIP are neither passed nor failed, so
        // `total - passed` would wrongly inflate the failed count.
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("20260305-022829");
        fs::create_dir_all(&run_dir).unwrap();
        fs::write(
            run_dir.join("report.json"),
            r#"{
                "meta":{"generated_at":"2026-03-05T02:28:29Z","range":{"target":{"commit":"abcdef1234567"}}},
                "gate":{"quality_pass":false,"allow_merge":false,"policy_mode":"warn"},
                "checks":[{"status":"PASS"},{"status":"WARN"},{"status":"SKIP"},{"status":"FAIL"},{"status":"ERROR"}],
                "diff":{"stats":{"files_changed":1}}
            }"#,
        )
        .unwrap();

        let entry = entry_from_disk(&run_dir, "20260305-022829", "repo", "main").unwrap();
        assert_eq!(entry.checks_passed, 1, "only PASS counts as passed");
        assert_eq!(
            entry.checks_failed, 2,
            "only FAIL and ERROR count as failed; WARN/SKIP must not inflate it"
        );
    }

    #[test]
    fn test_acquire_lock_rejects_live_owner_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("index.jsonl.lock");

        let guard = acquire_lock_at(&lock_file).unwrap();
        let second = acquire_lock_at(&lock_file).unwrap_err();
        assert!(second.to_string().contains("another live process"));
        drop(guard);
        assert!(!lock_file.exists(), "legacy sentinel is removed on release");
        assert!(
            super::v2_lock_path(&lock_file).exists(),
            "the v2 OS-lock inode remains stable across owners"
        );
        let _again = acquire_lock_at(&lock_file).expect("kernel releases the lock on drop");
    }

    #[test]
    fn a_live_legacy_owner_blocks_a_new_protocol_contender() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("index.jsonl.lock");
        let legacy = format!(
            "{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        fs::write(&lock_file, &legacy).unwrap();

        let error = acquire_lock_at(&lock_file).expect_err("live old owner must win");
        assert!(error.to_string().contains("another live process"));
        assert_eq!(fs::read_to_string(&lock_file).unwrap(), legacy);
    }

    #[test]
    fn a_stale_legacy_owner_fails_closed_without_mutating_the_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("index.jsonl.lock");
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&lock_file, "999999:1").unwrap();

        let error = acquire_lock_at(&lock_file)
            .expect_err("an unknown pre-v2 contender makes automatic takeover unsafe");
        assert!(error.to_string().contains("requires manual removal"));
        assert_eq!(
            fs::read_to_string(&lock_file).unwrap(),
            "999999:1",
            "failed migration must preserve the exact legacy evidence"
        );
        assert!(super::v2_lock_path(&lock_file).exists());

        // The failed contender released its kernel lock. Once an operator has
        // established that no old publisher survives and removes the sentinel,
        // the normal protocol can acquire immediately.
        fs::remove_file(&lock_file).unwrap();
        let _again = acquire_lock_at(&lock_file).expect("manual recovery unblocks v2 acquisition");
    }

    #[cfg(unix)]
    #[test]
    fn lock_protocol_never_truncates_symlink_targets() {
        use std::os::unix::fs::symlink;

        for link_v2 in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let protected = tmp.path().join("protected.txt");
            fs::write(&protected, "do-not-touch").unwrap();
            let legacy = tmp.path().join("index.jsonl.lock");
            let attacked = if link_v2 {
                super::v2_lock_path(&legacy)
            } else {
                legacy.clone()
            };
            symlink(&protected, &attacked).unwrap();

            acquire_lock_at(&legacy).expect_err("symlink lock paths fail closed");
            assert_eq!(fs::read_to_string(&protected).unwrap(), "do-not-touch");
        }
    }

    #[cfg(unix)]
    #[test]
    fn lock_protocol_never_truncates_hardlink_targets() {
        for link_v2 in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let protected = tmp.path().join("protected.txt");
            fs::write(&protected, "do-not-touch").unwrap();
            let legacy = tmp.path().join("index.jsonl.lock");
            let attacked = if link_v2 {
                super::v2_lock_path(&legacy)
            } else {
                legacy.clone()
            };
            fs::hard_link(&protected, &attacked).unwrap();

            acquire_lock_at(&legacy).expect_err("hardlinked lock paths fail closed");
            assert_eq!(fs::read_to_string(&protected).unwrap(), "do-not-touch");
            assert_eq!(fs::read_to_string(&attacked).unwrap(), "do-not-touch");
        }
    }

    #[test]
    fn three_contenders_never_share_lock_ownership() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("index.jsonl.lock");
        let owner = acquire_lock_at(&lock_file).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let contenders: Vec<_> = (0..2)
            .map(|_| {
                let path = lock_file.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    acquire_lock_at(&path).is_ok()
                })
            })
            .collect();
        barrier.wait();

        assert!(contenders.into_iter().all(|join| !join.join().unwrap()));
        drop(owner);
        acquire_lock_at(&lock_file).expect("one later owner acquires after release");
    }

    #[test]
    fn test_list_for_branch_matches_legacy_branch_keys() {
        let mut index = RunIndex { entries: vec![] };
        index.append(make_entry("001", "repo-a", "feature-user-auth", 1000));

        let matches = index.list_for_branch("repo-a", "feature/user-auth");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].branch, "feature-user-auth");
    }

    #[test]
    fn test_ensure_opener_succeeded_accepts_zero_exit() {
        let dashboard = Path::new("/tmp/dashboard.html");

        ensure_opener_succeeded("open", dashboard, exit_status(0)).expect("zero exit succeeds");
    }

    #[test]
    fn test_ensure_opener_succeeded_rejects_non_zero_exit() {
        let dashboard = Path::new("/tmp/dashboard.html");

        let err = ensure_opener_succeeded("open", dashboard, exit_status(7)).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("open exited with status 7"));
        assert!(msg.contains("/tmp/dashboard.html"));
    }

    // `run_runs_command` reaches the global index/lock via PRVIEW_HOME, so these
    // tests serialize env mutation. No other storage test uses the global paths
    // (they all pass explicit paths), so scoping PRVIEW_HOME here is safe.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_prview_home<R>(f: impl FnOnce(&Path) -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let prev = std::env::var("PRVIEW_HOME").ok();
        // SAFETY: serialized by ENV_LOCK; restored before returning.
        unsafe { std::env::set_var("PRVIEW_HOME", home.path()) };
        let result = f(home.path());
        match prev {
            Some(v) => unsafe { std::env::set_var("PRVIEW_HOME", v) },
            None => unsafe { std::env::remove_var("PRVIEW_HOME") },
        }
        result
    }

    fn runs_opts_all_json() -> RunsOpts {
        RunsOpts {
            all: true,
            branch: None,
            status: None,
            json: true,
            rebuild: false,
        }
    }

    fn live_entry(home: &Path, id: &str) -> RunEntry {
        let dir = home.join("runs/myrepo/main").join(id);
        fs::create_dir_all(dir.join("00_summary")).unwrap();
        fs::write(
            dir.join("00_summary/RUN.json"),
            serde_json::json!({"artifacts_root": dir}).to_string(),
        )
        .unwrap();
        let mut e = make_entry(id, "myrepo", "main", 100);
        e.path = dir;
        e
    }

    fn stale_entry(home: &Path, id: &str) -> RunEntry {
        // Path intentionally NOT created on disk → remove_stale should drop it.
        let mut e = make_entry(id, "myrepo", "main", 100);
        e.path = home.join("runs/myrepo/main").join(id);
        e
    }

    fn tight_branch_policy() -> RetentionPolicy {
        RetentionPolicy {
            max_runs_per_branch: 1,
            max_runs_per_repo: 200,
            max_total_bytes: 5 * 1024 * 1024 * 1024,
        }
    }

    fn abort_on_check(n: usize) -> impl Fn() -> bool {
        let hits = std::sync::atomic::AtomicUsize::new(0);
        move || hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 >= n
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_successful_publishers_keep_latest_and_index_in_one_order() {
        use std::sync::mpsc;
        use std::time::Duration;

        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            let branch_dir = first_dir.parent().unwrap().to_path_buf();

            let (first_advertised_tx, first_advertised_rx) = mpsc::channel();
            let (release_first_tx, release_first_rx) = mpsc::channel();
            let (second_started_tx, second_started_rx) = mpsc::channel();
            let (second_acquired_tx, second_acquired_rx) = mpsc::channel();

            let first_thread = std::thread::spawn(move || {
                let publication = super::acquire_publication_lock(|| false).unwrap();
                let transaction = crate::artifacts::git_artifacts::begin_latest_publication(
                    &publication,
                    &first_dir,
                )
                .unwrap();
                first_advertised_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                super::register_and_prune_locked(&publication, &first_dir, first, false, || false)
                    .unwrap();
                crate::artifacts::git_artifacts::finish_latest_publication(&transaction).unwrap();
            });

            first_advertised_rx.recv().unwrap();
            let second_thread = std::thread::spawn(move || {
                second_started_tx.send(()).unwrap();
                let publication = super::acquire_publication_lock(|| false).unwrap();
                second_acquired_tx.send(()).unwrap();
                let transaction = crate::artifacts::git_artifacts::begin_latest_publication(
                    &publication,
                    &second_dir,
                )
                .unwrap();
                super::register_and_prune_locked(&publication, &second_dir, second, false, || {
                    false
                })
                .unwrap();
                crate::artifacts::git_artifacts::finish_latest_publication(&transaction).unwrap();
            });

            second_started_rx.recv().unwrap();
            assert!(
                second_acquired_rx
                    .recv_timeout(Duration::from_millis(50))
                    .is_err(),
                "the second publisher must not swap latest while the first owns the index transaction"
            );
            release_first_tx.send(()).unwrap();
            first_thread.join().unwrap();
            second_acquired_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("second publisher acquires after the first transaction commits");
            second_thread.join().unwrap();

            let latest_target = fs::read_link(branch_dir.join("latest")).unwrap();
            let index = RunIndex::load();
            let indexed_latest = index.latest("myrepo", "main").unwrap();
            assert_eq!(branch_dir.join(latest_target), indexed_latest.path);
            assert_eq!(indexed_latest.id, "second");
        });
    }

    #[cfg(unix)]
    #[test]
    fn restart_reconciles_latest_to_the_index_on_both_sides_of_commit() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            let branch_dir = first_dir.parent().unwrap().to_path_buf();

            let publication = super::acquire_publication_lock(|| false).unwrap();
            let first_transaction =
                crate::artifacts::git_artifacts::begin_latest_publication(&publication, &first_dir)
                    .unwrap();
            super::register_and_prune_locked(&publication, &first_dir, first, false, || false)
                .unwrap();
            crate::artifacts::git_artifacts::finish_latest_publication(&first_transaction).unwrap();
            drop(publication);

            // Simulate death after alias swap but before the index commit.
            let publication = super::acquire_publication_lock(|| false).unwrap();
            let interrupted = crate::artifacts::git_artifacts::begin_latest_publication(
                &publication,
                &second_dir,
            )
            .unwrap();
            drop(interrupted);
            drop(publication);
            assert_eq!(
                fs::read_link(branch_dir.join("latest")).unwrap(),
                PathBuf::from("second")
            );

            let publication = super::acquire_publication_lock(|| false).unwrap();
            crate::artifacts::git_artifacts::recover_latest_publication(&publication).unwrap();
            assert_eq!(
                fs::read_link(branch_dir.join("latest")).unwrap(),
                PathBuf::from("first")
            );
            drop(publication);

            // Simulate death after the index commit but before journal cleanup.
            let publication = super::acquire_publication_lock(|| false).unwrap();
            let committed = crate::artifacts::git_artifacts::begin_latest_publication(
                &publication,
                &second_dir,
            )
            .unwrap();
            super::register_and_prune_locked(&publication, &second_dir, second, false, || false)
                .unwrap();
            drop(committed);
            drop(publication);

            let publication = super::acquire_publication_lock(|| false).unwrap();
            crate::artifacts::git_artifacts::recover_latest_publication(&publication).unwrap();
            assert_eq!(
                fs::read_link(branch_dir.join("latest")).unwrap(),
                PathBuf::from("second")
            );
            assert_eq!(
                RunIndex::load().latest("myrepo", "main").unwrap().id,
                "second"
            );
            assert!(
                !crate::config::prview_home()
                    .join("publication-transaction.json")
                    .exists()
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn restart_skips_a_stale_last_index_row_when_reconciling_latest() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let stale = stale_entry(home, "stale");
            RunIndex {
                entries: vec![first, stale],
            }
            .save()
            .unwrap();

            let interrupted = live_entry(home, "interrupted");
            let interrupted_dir = interrupted.path.clone();
            let branch_dir = first_dir.parent().unwrap().to_path_buf();
            std::os::unix::fs::symlink("first", branch_dir.join("latest")).unwrap();

            let publication = super::acquire_publication_lock(|| false).unwrap();
            let transaction = crate::artifacts::git_artifacts::begin_latest_publication(
                &publication,
                &interrupted_dir,
            )
            .unwrap();
            drop(transaction);
            assert_eq!(
                fs::read_link(branch_dir.join("latest")).unwrap(),
                PathBuf::from("interrupted")
            );

            crate::artifacts::git_artifacts::recover_latest_publication(&publication).unwrap();
            assert_eq!(
                fs::read_link(branch_dir.join("latest")).unwrap(),
                PathBuf::from("first"),
                "recovery must skip the missing last row and advertise the last live owned pack"
            );
        });
    }

    /// Cancel before the index rename must leave historical runs on disk and
    /// omit the new row — prune is not allowed to race the abort.
    #[test]
    fn register_abort_before_save_keeps_predecessor_evidence() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            super::register_and_prune_with_policy(
                &first_dir,
                first,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("predecessor published");

            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            let err = super::register_and_prune_with_policy(
                &second_dir,
                second,
                false,
                &tight_branch_policy(),
                abort_on_check(2),
            )
            .expect_err("abort before save");
            assert!(
                err.to_string().contains("publication aborted"),
                "got {err:#}"
            );

            let ids: Vec<String> = RunIndex::load()
                .entries()
                .iter()
                .map(|e| e.id.clone())
                .collect();
            assert_eq!(ids, vec!["first".to_string()]);
            assert!(
                first_dir.is_dir(),
                "predecessor directory must survive an abort before save"
            );
            assert!(
                second_dir.is_dir(),
                "the unpublished pack directory is not deleted by index abort"
            );
        });
    }

    /// Cancel after the index rename, before prune, must restore the previous
    /// index and keep the predecessor directory.
    #[test]
    fn register_abort_after_save_rolls_index_back_without_pruning() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            super::register_and_prune_with_policy(
                &first_dir,
                first,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("predecessor published");

            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            super::register_and_prune_with_policy(
                &second_dir,
                second,
                false,
                &tight_branch_policy(),
                abort_on_check(4),
            )
            .expect_err("abort after save");

            let ids: Vec<String> = RunIndex::load()
                .entries()
                .iter()
                .map(|e| e.id.clone())
                .collect();
            assert_eq!(
                ids,
                vec!["first".to_string()],
                "rolled-back index must not advertise the cancelled pack: {ids:?}"
            );
            assert!(
                first_dir.is_dir(),
                "predecessor evidence must not be deleted after a rolled-back save"
            );
        });
    }

    #[test]
    fn failed_previous_index_restore_is_classified_as_unconfirmed() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            super::register_and_prune_with_policy(
                &first_dir,
                first,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("predecessor published");

            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            let error = super::register_and_prune_with_policy(
                &second_dir,
                second,
                false,
                &tight_branch_policy(),
                || {
                    let new_index_is_visible = RunIndex::load()
                        .latest("myrepo", "main")
                        .is_some_and(|entry| entry.id == "second");
                    if new_index_is_visible {
                        super::arm_test_index_save_failure();
                        true
                    } else {
                        false
                    }
                },
            )
            .expect_err("the injected previous-index restore fails");

            assert!(
                super::is_unconfirmed_publication_rollback(&error),
                "caller must preserve the outer journal for an indeterminate index: {error:#}"
            );
            assert!(first_dir.is_dir(), "staged evidence was still restored");
            assert_eq!(
                RunIndex::load().latest("myrepo", "main").unwrap().id,
                "second",
                "the failed restore leaves the just-committed index as durable truth"
            );
        });
    }

    #[test]
    fn fallback_index_restore_failure_is_also_classified_as_unconfirmed() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            super::register_and_prune_with_policy(
                &first_dir,
                first,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("predecessor published");

            super::arm_test_prune_stage_failure();
            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            let error = super::register_and_prune_with_policy(
                &second_dir,
                second,
                false,
                &tight_branch_policy(),
                || {
                    let fallback_index_is_visible = RunIndex::load()
                        .latest("myrepo", "main")
                        .is_some_and(|entry| entry.id == "second");
                    if fallback_index_is_visible {
                        super::arm_test_index_save_failure();
                        true
                    } else {
                        false
                    }
                },
            )
            .expect_err("the fallback previous-index restore is injected to fail");

            assert!(
                super::is_unconfirmed_publication_rollback(&error),
                "fallback must preserve the outer journal too: {error:#}"
            );
            assert!(first_dir.is_dir());
            assert_eq!(
                RunIndex::load().latest("myrepo", "main").unwrap().id,
                "second"
            );
        });
    }

    #[test]
    fn retention_never_moves_an_index_path_without_pack_identity() {
        with_prview_home(|home| {
            let unrelated = home.join("operator-directory");
            fs::create_dir_all(&unrelated).unwrap();
            fs::write(unrelated.join("keep.txt"), "operator-owned").unwrap();
            let mut corrupt = make_entry("corrupt", "myrepo", "main", 100);
            corrupt.path = unrelated.clone();
            RunIndex {
                entries: vec![corrupt],
            }
            .save()
            .unwrap();

            let current = live_entry(home, "current");
            let current_dir = current.path.clone();
            super::register_and_prune_with_policy(
                &current_dir,
                current,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("invalid retention authority degrades to keep-all rows");

            assert_eq!(
                fs::read_to_string(unrelated.join("keep.txt")).unwrap(),
                "operator-owned"
            );
            assert!(
                RunIndex::load()
                    .entries()
                    .iter()
                    .any(|entry| entry.path == unrelated),
                "the untrusted row is preserved rather than destructively consumed"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn retention_never_follows_an_index_symlink_to_a_pack() {
        use std::os::unix::fs::symlink;

        with_prview_home(|home| {
            let target = live_entry(home, "target");
            let link = home.join("linked-pack");
            symlink(&target.path, &link).unwrap();
            let mut corrupt = make_entry("linked", "myrepo", "main", 100);
            corrupt.path = link.clone();
            RunIndex {
                entries: vec![corrupt],
            }
            .save()
            .unwrap();

            let current = live_entry(home, "current");
            let current_dir = current.path.clone();
            super::register_and_prune_with_policy(
                &current_dir,
                current,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("symlink retention authority degrades to keep-all rows");

            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(target.path.join("00_summary/RUN.json").is_file());
            assert!(
                RunIndex::load()
                    .entries()
                    .iter()
                    .any(|entry| entry.path == link)
            );
        });
    }

    /// Cancellation after the predecessor has been staged but before the
    /// index save restores its original path and leaves no prune tombstone.
    #[test]
    fn register_abort_during_prune_staging_restores_predecessor() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            super::register_and_prune_with_policy(
                &first_dir,
                first,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("predecessor published");

            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            super::register_and_prune_with_policy(
                &second_dir,
                second,
                false,
                &tight_branch_policy(),
                abort_on_check(3),
            )
            .expect_err("abort after staging");

            let ids: Vec<String> = RunIndex::load()
                .entries()
                .iter()
                .map(|entry| entry.id.clone())
                .collect();
            assert_eq!(ids, vec!["first".to_owned()]);
            assert!(first_dir.is_dir(), "staged predecessor must be restored");
            assert!(second_dir.is_dir());
            let trash = super::prune_trash_path();
            assert!(
                !trash.is_dir() || fs::read_dir(trash).unwrap().next().is_none(),
                "rollback leaves no committed prune tombstone"
            );
        });
    }

    /// A hard process exit loses the in-memory `StagedPrune` guard. On restart,
    /// a persisted index row is durable proof that the directory move never
    /// committed and the tombstone must be restored rather than deleted.
    #[test]
    fn restart_recovers_an_uncommitted_staged_prune() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            RunIndex {
                entries: vec![first],
            }
            .save()
            .unwrap();

            let staged = super::stage_pruned_paths(
                std::slice::from_ref(&first_dir),
                &abort_on_check(usize::MAX),
            )
            .expect("stage predecessor");
            assert!(!first_dir.exists(), "fixture simulates death after rename");
            assert_eq!(
                super::read_prune_manifest(&staged[0].transaction_dir)
                    .unwrap()
                    .state,
                super::PruneTransactionState::Staged
            );

            // Forget the in-memory transaction exactly as process death would.
            drop(staged);
            let persisted = RunIndex::load();
            super::cleanup_committed_prunes(&persisted, &abort_on_check(usize::MAX))
                .expect("restart restores indexed predecessor");

            assert!(first_dir.is_dir(), "indexed evidence must be restored");
            assert!(
                !super::prune_trash_path().exists(),
                "recovered transaction leaves no tombstone"
            );
        });
    }

    #[test]
    fn restart_preserves_a_prune_transaction_with_mismatched_payload_identity() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let staged = super::stage_pruned_paths(
                std::slice::from_ref(&first_dir),
                &abort_on_check(usize::MAX),
            )
            .expect("stage predecessor");
            let transaction_dir = staged[0].transaction_dir.clone();
            let payload = staged[0].tombstone.clone();
            let wrong_original = home.join("runs/myrepo/main/not-this-pack");
            let mut manifest = super::read_prune_manifest(&transaction_dir).unwrap();
            manifest.original = super::DurablePath::capture(&wrong_original).unwrap();
            super::write_prune_manifest(&transaction_dir, &manifest).unwrap();
            drop(staged);

            super::cleanup_committed_prunes(
                &RunIndex { entries: vec![] },
                &abort_on_check(usize::MAX),
            )
            .expect("mismatched transaction is quarantined without blocking publication");

            assert!(payload.join("00_summary/RUN.json").is_file());
            assert!(!wrong_original.exists());
            assert!(transaction_dir.is_dir());
        });
    }

    #[test]
    fn restart_preserves_a_manifestless_prune_transaction_with_payload() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let staged = super::stage_pruned_paths(
                std::slice::from_ref(&first_dir),
                &abort_on_check(usize::MAX),
            )
            .expect("stage predecessor");
            let transaction_dir = staged[0].transaction_dir.clone();
            let payload = staged[0].tombstone.clone();
            fs::remove_file(transaction_dir.join(super::PRUNE_MANIFEST_FILE)).unwrap();
            drop(staged);

            super::cleanup_committed_prunes(
                &RunIndex { entries: vec![] },
                &abort_on_check(usize::MAX),
            )
            .expect("manifestless payload is preserved without blocking publication");

            assert!(payload.join("00_summary/RUN.json").is_file());
            assert!(transaction_dir.is_dir());
        });
    }

    #[test]
    fn restart_preserves_a_prune_payload_with_invalid_manifest() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let staged = super::stage_pruned_paths(
                std::slice::from_ref(&first_dir),
                &abort_on_check(usize::MAX),
            )
            .expect("stage predecessor");
            let transaction_dir = staged[0].transaction_dir.clone();
            let payload = staged[0].tombstone.clone();
            fs::write(
                transaction_dir.join(super::PRUNE_MANIFEST_FILE),
                "not-json\n",
            )
            .unwrap();
            drop(staged);

            super::cleanup_committed_prunes(
                &RunIndex { entries: vec![] },
                &abort_on_check(usize::MAX),
            )
            .expect("invalid manifest is preserved without blocking publication");

            assert!(payload.join("00_summary/RUN.json").is_file());
            assert!(transaction_dir.is_dir());
        });
    }

    #[test]
    fn restart_propagates_an_unconfirmed_prune_recovery_mutation() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let staged = super::stage_pruned_paths(
                std::slice::from_ref(&first_dir),
                &abort_on_check(usize::MAX),
            )
            .expect("stage predecessor");
            fs::create_dir_all(&first_dir).unwrap();
            drop(staged);

            let error = super::cleanup_committed_prunes(
                &RunIndex { entries: vec![] },
                &abort_on_check(usize::MAX),
            )
            .expect_err("a live restore conflict must abort the publication transaction");

            assert!(error.to_string().contains("Cannot recover retained run"));
            assert!(first_dir.is_dir());
            assert!(super::prune_trash_path().is_dir());
        });
    }

    #[cfg(unix)]
    #[test]
    fn prune_manifest_ignores_a_predictable_temp_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let transaction = root.path().join("transaction");
        fs::create_dir(&transaction).unwrap();
        let protected = root.path().join("protected.txt");
        fs::write(&protected, "do-not-touch").unwrap();
        symlink(&protected, transaction.join("transaction.json.tmp")).unwrap();
        let manifest = super::PruneTransactionManifest {
            schema: 1,
            original: super::DurablePath::capture(&root.path().join("run")).unwrap(),
            state: super::PruneTransactionState::Staged,
        };

        super::write_prune_manifest(&transaction, &manifest).unwrap();

        assert_eq!(fs::read_to_string(&protected).unwrap(), "do-not-touch");
        assert_eq!(
            super::read_prune_manifest(&transaction).unwrap().state,
            super::PruneTransactionState::Staged
        );
    }

    /// Upgrade recovery: the previous binary staged the run directory itself,
    /// without a manifest. RUN.json plus the persisted index still identify the
    /// original path, so the fixed binary must restore that historical evidence.
    #[test]
    fn restart_recovers_an_uncommitted_legacy_raw_tombstone() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            fs::create_dir_all(first_dir.join("00_summary")).unwrap();
            fs::write(
                first_dir.join("00_summary/RUN.json"),
                serde_json::json!({ "artifacts_root": first_dir }).to_string(),
            )
            .unwrap();
            RunIndex {
                entries: vec![first],
            }
            .save()
            .unwrap();

            let trash = super::prune_trash_path();
            fs::create_dir_all(&trash).unwrap();
            let raw_tombstone = trash.join("prune-old-format");
            fs::rename(&first_dir, &raw_tombstone).unwrap();

            let persisted = RunIndex::load();
            super::cleanup_committed_prunes(&persisted, &abort_on_check(usize::MAX))
                .expect("restart restores legacy indexed predecessor");

            assert!(first_dir.is_dir(), "legacy indexed evidence is restored");
            assert!(!trash.exists());
        });
    }

    #[test]
    fn restart_preserves_an_unidentified_legacy_tombstone() {
        with_prview_home(|_| {
            let trash = super::prune_trash_path();
            let unknown = trash.join("prune-old-format");
            fs::create_dir_all(&unknown).unwrap();
            fs::write(unknown.join("evidence.txt"), "keep").unwrap();

            super::cleanup_committed_prunes(
                &RunIndex { entries: vec![] },
                &abort_on_check(usize::MAX),
            )
            .expect("unknown legacy state is preserved without blocking registration");

            assert!(unknown.join("evidence.txt").is_file());
        });
    }

    /// The opposite crash window is after the atomic index save but before the
    /// marker flips from staged to committed. Absence from a parsed index is not
    /// strong enough deletion authority (a corrupt row is skipped), so recovery
    /// keeps the pack as a safe orphan for a later rebuild/prune.
    #[test]
    fn restart_conservatively_restores_when_commit_marker_is_missing() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            let staged = super::stage_pruned_paths(
                std::slice::from_ref(&first_dir),
                &abort_on_check(usize::MAX),
            )
            .expect("stage predecessor");

            RunIndex { entries: vec![] }.save().unwrap();
            assert_eq!(
                super::read_prune_manifest(&staged[0].transaction_dir)
                    .unwrap()
                    .state,
                super::PruneTransactionState::Staged,
                "fixture dies before the marker update"
            );
            drop(staged);

            let persisted = RunIndex::load();
            super::cleanup_committed_prunes(&persisted, &abort_on_check(usize::MAX))
                .expect("restart preserves evidence while commit state is ambiguous");

            assert!(first_dir.is_dir());
            assert!(!super::prune_trash_path().exists());
        });
    }

    #[test]
    fn register_success_prunes_after_commit() {
        with_prview_home(|home| {
            let first = live_entry(home, "first");
            let first_dir = first.path.clone();
            super::register_and_prune_with_policy(
                &first_dir,
                first,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("predecessor published");

            let second = live_entry(home, "second");
            let second_dir = second.path.clone();
            super::register_and_prune_with_policy(
                &second_dir,
                second,
                false,
                &tight_branch_policy(),
                abort_on_check(usize::MAX),
            )
            .expect("successor published");

            let ids: Vec<String> = RunIndex::load()
                .entries()
                .iter()
                .map(|e| e.id.clone())
                .collect();
            assert_eq!(ids, vec!["second".to_string()]);
            assert!(
                !first_dir.is_dir(),
                "retention prune runs only after a committed index save"
            );
            assert!(second_dir.is_dir());
            assert!(
                super::prune_trash_path().is_dir(),
                "physical deletion is deferred as a committed tombstone"
            );

            let third = live_entry(home, "third");
            let third_dir = third.path.clone();
            super::register_and_prune_with_policy(
                &third_dir,
                third,
                false,
                &RetentionPolicy {
                    max_runs_per_branch: 10,
                    max_runs_per_repo: 200,
                    max_total_bytes: u64::MAX,
                },
                abort_on_check(usize::MAX),
            )
            .expect("next registration cleans committed tombstones");
            assert!(
                !super::prune_trash_path().exists(),
                "committed tombstones are removed before the next index mutation"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn committed_prune_cleanup_unlinks_symlink_without_following_target() {
        use std::os::unix::fs::symlink;

        with_prview_home(|home| {
            let protected = home.join("protected");
            fs::create_dir_all(&protected).unwrap();
            fs::write(protected.join("evidence.txt"), "keep").unwrap();

            let trash = super::prune_trash_path();
            fs::create_dir_all(&trash).unwrap();
            symlink(&protected, trash.join("prune-link")).unwrap();

            super::cleanup_committed_prunes(
                &RunIndex { entries: vec![] },
                &abort_on_check(usize::MAX),
            )
            .expect("cleanup unlinks tombstone symlink");
            assert!(protected.join("evidence.txt").is_file());
            assert!(!trash.exists());
        });
    }

    #[cfg(unix)]
    #[test]
    fn prune_identity_rejects_linked_summary_and_hardlinked_run_file() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("payload");
        let external_summary = tmp.path().join("external-summary");
        fs::create_dir(&payload).unwrap();
        fs::create_dir(&external_summary).unwrap();
        fs::write(
            external_summary.join("RUN.json"),
            serde_json::json!({"artifacts_root": payload.display().to_string()}).to_string(),
        )
        .unwrap();
        symlink(&external_summary, payload.join("00_summary")).unwrap();

        super::validate_prune_payload_identity(&payload, &payload)
            .expect_err("a linked authority component cannot identify a prune payload");
        fs::remove_file(payload.join("00_summary")).unwrap();

        let summary = payload.join("00_summary");
        fs::create_dir(&summary).unwrap();
        fs::hard_link(external_summary.join("RUN.json"), summary.join("RUN.json")).unwrap();
        super::validate_prune_payload_identity(&payload, &payload)
            .expect_err("a shared RUN inode is not an owned authority file");
        assert!(
            external_summary.join("RUN.json").is_file(),
            "failed authority checks preserve external evidence"
        );
    }

    #[cfg(windows)]
    #[test]
    fn prune_refuses_windows_junction_without_touching_target() {
        let protected = tempfile::tempdir().expect("protected target");
        fs::write(protected.path().join("evidence.txt"), "keep").unwrap();
        let holder = tempfile::tempdir().expect("junction holder");
        let junction = holder.path().join("indexed-run-junction");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                junction.to_str().expect("UTF-8 test path"),
                protected.path().to_str().expect("UTF-8 test path"),
            ])
            .status()
            .expect("create Windows junction");
        assert!(
            status.success(),
            "mklink /J must succeed for the regression"
        );

        let metadata = fs::symlink_metadata(&junction).expect("junction metadata");
        assert!(super::is_reparse_point(&metadata));
        assert!(!super::is_owned_regular_dir(&metadata));
        let error = super::stage_pruned_paths(&[junction.clone()], &|| false)
            .expect_err("an index row can never grant prune authority over a junction");
        assert!(
            error.to_string().contains("linked index path"),
            "got: {error:#}"
        );
        assert_eq!(
            fs::read_to_string(protected.path().join("evidence.txt")).unwrap(),
            "keep",
            "junction rejection must preserve the external target"
        );

        fs::remove_dir(&junction).expect("unlink test junction without traversing target");
    }

    #[cfg(windows)]
    #[test]
    fn committed_prune_cleanup_unlinks_child_junction_without_following_target() {
        let protected = tempfile::tempdir().expect("protected target");
        fs::write(protected.path().join("evidence.txt"), "keep").unwrap();
        let holder = tempfile::tempdir().expect("transaction holder");
        let transaction = holder.path().join("transaction");
        fs::create_dir(&transaction).unwrap();
        let junction = transaction.join("child-junction");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                junction.to_str().expect("UTF-8 test path"),
                protected.path().to_str().expect("UTF-8 test path"),
            ])
            .status()
            .expect("create Windows junction");
        assert!(
            status.success(),
            "mklink /J must succeed for the regression"
        );

        let metadata = fs::symlink_metadata(&junction).expect("junction metadata");
        assert!(super::is_directory_reparse_point(&metadata));
        super::remove_dir_all_cancellable(&transaction, &|| false)
            .expect("cleanup unlinks the child junction itself");
        assert!(!transaction.exists());
        assert_eq!(
            fs::read_to_string(protected.path().join("evidence.txt")).unwrap(),
            "keep",
            "recursive cleanup must never enter the junction target"
        );
    }

    #[test]
    fn run_runs_command_cleanup_removes_stale_and_keeps_live() {
        with_prview_home(|home| {
            let mut index = RunIndex { entries: vec![] };
            index.append(live_entry(home, "live-001"));
            index.append(stale_entry(home, "stale-002"));
            index.save().unwrap();

            run_runs_command(&runs_opts_all_json()).expect("runs command");

            // The stale-entry cleanup is a locked read-modify-write: the live
            // entry survives, the stale one is dropped, and the change is
            // persisted.
            let reloaded = RunIndex::load();
            let ids: Vec<&str> = reloaded.entries().iter().map(|e| e.id.as_str()).collect();
            assert!(
                ids.contains(&"live-001"),
                "live entry must survive: {ids:?}"
            );
            assert!(
                !ids.contains(&"stale-002"),
                "stale entry must be pruned: {ids:?}"
            );
        });
    }

    #[test]
    fn run_runs_command_skips_cleanup_write_when_lock_is_held() {
        with_prview_home(|home| {
            let mut index = RunIndex { entries: vec![] };
            index.append(live_entry(home, "live-001"));
            index.append(stale_entry(home, "stale-002"));
            index.save().unwrap();

            // A concurrent writer owns the index lock. `runs` must not perform an
            // unlocked cleanup write; it degrades to a read and leaves the index
            // (including the not-yet-pruned stale entry) untouched.
            let _held = acquire_lock().expect("acquire lock");
            run_runs_command(&runs_opts_all_json()).expect("runs command stays read-only");

            let reloaded = RunIndex::load();
            let ids: Vec<&str> = reloaded.entries().iter().map(|e| e.id.as_str()).collect();
            assert!(
                ids.contains(&"stale-002"),
                "cleanup must be gated on the lock; stale entry must remain: {ids:?}"
            );
        });
    }
}
