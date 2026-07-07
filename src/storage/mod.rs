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
use std::io::{BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};

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

pub struct RunIndex {
    entries: Vec<RunEntry>,
}

fn index_path() -> PathBuf {
    prview_home().join("index.jsonl")
}

fn lock_path() -> PathBuf {
    prview_home().join("index.jsonl.lock")
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
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp = resolved.with_extension("jsonl.tmp");
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("Cannot create {}", tmp.display()))?;
            for entry in &self.entries {
                let line = serde_json::to_string(entry)?;
                writeln!(f, "{}", line)?;
            }
            f.flush()?;
        }
        fs::rename(&tmp, &resolved)?;
        Ok(())
    }

    /// Atomic save: write to tmp file then rename.
    pub fn save(&self) -> Result<()> {
        let path = resolve_explicit_index_path(&index_path())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut f = fs::File::create(&tmp)
                .with_context(|| format!("Cannot create {}", tmp.display()))?;
            for entry in &self.entries {
                let line = serde_json::to_string(entry)?;
                writeln!(f, "{}", line)?;
            }
            f.flush()?;
        }
        fs::rename(&tmp, &path)?;
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
    path: PathBuf,
    token: String,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Ok(content) = fs::read_to_string(&self.path)
            && content.trim() == self.token
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Acquire a file lock on index.jsonl.lock.
///
/// The lock file is created with `create_new(true)` so acquisition is atomic.
/// A lock is only considered stale when the owning PID is no longer alive.
pub fn acquire_lock() -> Result<LockGuard> {
    acquire_lock_at(&lock_path())
}

/// Acquire a file lock at an explicit path.
pub fn acquire_lock_at(path: &Path) -> Result<LockGuard> {
    let path = path.to_path_buf();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let token = format!(
        "{}:{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    for _ in 0..3 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(token.as_bytes())?;
                file.flush()?;
                return Ok(LockGuard { path, token });
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let content = fs::read_to_string(&path).unwrap_or_default();
                if lock_is_stale(&content) {
                    // Claim the stale lock atomically instead of removing it in
                    // place. A blind remove is a TOCTOU: between reading the dead
                    // pid and deleting the file, another process can clear the
                    // stale lock and create a fresh live one, which we would then
                    // destroy. Rename is atomic, so only one racer moves the file;
                    // we verify we moved the exact stale file we inspected before
                    // discarding it, and restore it otherwise.
                    let claim = path.with_file_name(format!(
                        "{}.claim.{}",
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("index.jsonl.lock"),
                        token.replace(':', "-")
                    ));
                    match fs::rename(&path, &claim) {
                        Ok(()) => {
                            let moved = fs::read_to_string(&claim).unwrap_or_default();
                            if moved.trim() == content.trim() {
                                // The file we claimed is the stale one we validated.
                                let _ = fs::remove_file(&claim);
                                continue;
                            }
                            // A newer lock replaced the stale file after our read;
                            // put it back and treat the owner as live.
                            let _ = fs::rename(&claim, &path);
                        }
                        // Another racer already cleared it — retry from scratch.
                        Err(rename_err) if rename_err.kind() == ErrorKind::NotFound => continue,
                        Err(rename_err) => {
                            return Err(rename_err).with_context(|| {
                                format!("Failed to claim stale lock {}", path.display())
                            });
                        }
                    }
                }

                bail!(
                    "Index lock held by another live process ({}). If this is stale, delete {}",
                    content.trim(),
                    path.display()
                );
            }
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to create lock {}", path.display()));
            }
        }
    }

    bail!(
        "Failed to acquire index lock after clearing stale lock races at {}",
        path.display()
    );
}

fn lock_is_stale(content: &str) -> bool {
    let pid = content
        .trim()
        .split(':')
        .next()
        .and_then(|part| part.parse::<u32>().ok());

    match pid {
        Some(pid) => !is_process_alive(pid),
        None => true,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether a process is alive via `kill(pid, 0)`.
///
/// Shared with the MCP run-liveness reader (`mcp::read::run_status`) which
/// derives deep-run status deterministically from a pid marker.
pub(crate) fn is_process_alive(pid: u32) -> bool {
    // kill(pid, 0) checks if process exists without sending signal
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }

    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
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
                let mut idx = RunIndex::load();
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
/// Call this at the end of `artifacts::generate()` with lock held.
/// Returns the number of pruned directories.
pub fn register_and_prune(
    out_dir: &Path,
    entry: RunEntry,
    emit_human_stdout: bool,
) -> Result<usize> {
    let _lock = acquire_lock()?;
    let mut index = RunIndex::load();
    index.append(entry);

    let pruned = index.prune(&RetentionPolicy::default(), out_dir);
    let pruned_count = pruned.len();

    // Delete pruned directories
    for path in &pruned {
        if path.is_dir() {
            let _ = fs::remove_dir_all(path);
        }
    }

    index.save()?;

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
    fn test_acquire_lock_rejects_live_owner_and_cleans_up_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("index.jsonl.lock");

        let guard = acquire_lock_at(&lock_file).unwrap();
        let second = acquire_lock_at(&lock_file).unwrap_err();
        assert!(second.to_string().contains("another live process"));
        drop(guard);
        assert!(!lock_file.exists());
    }

    #[test]
    fn test_acquire_lock_replaces_stale_dead_pid_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("index.jsonl.lock");
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&lock_file, "999999:1").unwrap();

        let guard = acquire_lock_at(&lock_file).unwrap();
        let content = fs::read_to_string(&lock_file).unwrap();
        assert_eq!(content.trim(), guard.token);
        drop(guard);
    }

    #[test]
    fn test_acquire_lock_claims_stale_lock_without_leaving_artifacts() {
        // Replacing a stale lock goes through an atomic rename-claim rather than
        // a blind remove. The claimed temp file must be cleaned up, leaving only
        // our own live lock behind — no leaked `.claim.` files.
        let tmp = tempfile::tempdir().unwrap();
        let lock_file = tmp.path().join("index.jsonl.lock");
        fs::write(&lock_file, "999999:1").unwrap();

        let guard = acquire_lock_at(&lock_file).unwrap();

        let leftovers: Vec<String> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".claim."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "stale-lock claim must leave no temp artifacts, found: {leftovers:?}"
        );
        assert!(lock_file.exists(), "our live lock must be held");
        drop(guard);
        assert!(!lock_file.exists());
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
        fs::create_dir_all(&dir).unwrap();
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
