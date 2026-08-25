//! Revision-bound repository file access for language backends.
//!
//! The source contract keeps bytes attached to one exact Git commit, or to a
//! tracked working-tree overlay whose inventory and bytes/states are captured
//! together and then hashed into their own dirty digest. Overlay inventory
//! admits only exact target entries and extra paths supplied by tracked Git
//! status; untracked paths remain outside both inventory and reads. It
//! deliberately performs no language or API classification.

use crate::git::{GitTreeEntryKind, GitWorktreeChange, Repository};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;

/// Maximum total regular-file content retained by one tracked overlay capture.
///
/// The budget is shared by every tracked change. A path that does not fit is
/// retained as an explicit unreadable state, so snapshotting stays bounded and
/// can never fall through to a later filesystem read.
pub const TRACKED_CAPTURE_BYTE_BUDGET: u64 = 64 * 1024 * 1024;

/// Identity of the substrate that produced an entry or read result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionProvenance {
    GitTree {
        commit_oid: String,
    },
    WorkingTreeOverlay {
        target_oid: String,
        dirty_digest: String,
    },
}

/// Repository entry type without following symlinks or gitlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionEntryKind {
    RegularFile,
    Symlink,
    Tree,
    Gitlink,
    Unsupported,
}

/// Current state of a path exposed by a revision source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionEntryState {
    Present,
    Added,
    RenamedFrom { from: String },
    Deleted,
    Renamed { to: String },
    NonRegular { kind: RevisionEntryKind },
    Unreadable { reason: String },
}

/// One deterministic, normalized repo-relative tree entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionEntry {
    pub path: String,
    /// Object identity in the exact target/baseline tree, when one exists.
    ///
    /// Overlay-only additions have no baseline object and therefore carry
    /// `None`. A rename destination retains the old path's baseline object
    /// identity while its state names the source path explicitly.
    pub baseline_object_id: Option<String>,
    pub mode: u32,
    pub kind: RevisionEntryKind,
    pub state: RevisionEntryState,
    pub provenance: RevisionProvenance,
}

/// Coarse content marker for downstream parsers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevisionContentKind {
    Utf8Text,
    BinaryOrNonUtf8,
}

/// Exact regular-file bytes and their source identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionBytes {
    pub bytes: Vec<u8>,
    pub content_kind: RevisionContentKind,
    pub provenance: RevisionProvenance,
}

/// Explicit result of reading one repo-relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionRead {
    Bytes(RevisionBytes),
    Missing {
        provenance: RevisionProvenance,
    },
    Deleted {
        provenance: RevisionProvenance,
    },
    Renamed {
        to: String,
        provenance: RevisionProvenance,
    },
    NonRegular {
        kind: RevisionEntryKind,
        provenance: RevisionProvenance,
    },
    Unreadable {
        reason: String,
        provenance: RevisionProvenance,
    },
}

/// Construction/read errors that must not collapse into absence or empty text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevisionSourceError {
    InvalidCommitOid { oid: String, reason: String },
    ObjectUnavailable { oid: String, reason: String },
    InvalidRepoRelativePath { path: String, reason: String },
    MissingDirtyDigest,
    WorktreeStatusUnavailable { reason: String },
}

impl fmt::Display for RevisionSourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommitOid { oid, reason } => {
                write!(f, "invalid exact commit OID {oid}: {reason}")
            }
            Self::ObjectUnavailable { oid, reason } => {
                write!(f, "Git object for commit {oid} is unavailable: {reason}")
            }
            Self::InvalidRepoRelativePath { path, reason } => {
                write!(f, "invalid repo-relative path {path:?}: {reason}")
            }
            Self::MissingDirtyDigest => {
                write!(f, "working-tree overlay requires a captured dirty digest")
            }
            Self::WorktreeStatusUnavailable { reason } => {
                write!(f, "working-tree status is unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for RevisionSourceError {}

/// Language-neutral file source consumed by revision analysis backends.
pub trait RevisionFileSource {
    fn provenance(&self) -> &RevisionProvenance;
    fn entries(&self) -> Vec<RevisionEntry>;
    fn read(&self, path: &str) -> Result<RevisionRead, RevisionSourceError>;
}

/// Exact commit-tree source. It never resolves or reads implicit `HEAD`.
pub struct GitTree<'repo> {
    repo: &'repo Repository,
    provenance: RevisionProvenance,
    entries: Vec<RevisionEntry>,
}

impl<'repo> GitTree<'repo> {
    pub fn new(repo: &'repo Repository, commit_oid: &str) -> Result<Self, RevisionSourceError> {
        validate_exact_oid(commit_oid)?;
        let provenance = RevisionProvenance::GitTree {
            commit_oid: commit_oid.to_owned(),
        };
        let entries = repo
            .tree_entries_at_oid(commit_oid)
            .map_err(|error| RevisionSourceError::ObjectUnavailable {
                oid: commit_oid.to_owned(),
                reason: error.to_string(),
            })?
            .into_iter()
            .map(|entry| RevisionEntry {
                path: entry.path,
                baseline_object_id: Some(entry.object_id),
                mode: entry.mode,
                kind: map_entry_kind(entry.kind),
                state: RevisionEntryState::Present,
                provenance: provenance.clone(),
            })
            .collect();
        Ok(Self {
            repo,
            provenance,
            entries,
        })
    }

    pub fn commit_oid(&self) -> &str {
        match &self.provenance {
            RevisionProvenance::GitTree { commit_oid } => commit_oid,
            RevisionProvenance::WorkingTreeOverlay { .. } => unreachable!("GitTree provenance"),
        }
    }

    fn entry(&self, path: &str) -> Option<&RevisionEntry> {
        self.entries
            .binary_search_by(|entry| entry.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.entries[index])
    }
}

impl RevisionFileSource for GitTree<'_> {
    fn provenance(&self) -> &RevisionProvenance {
        &self.provenance
    }

    fn entries(&self) -> Vec<RevisionEntry> {
        self.entries.clone()
    }

    fn read(&self, path: &str) -> Result<RevisionRead, RevisionSourceError> {
        validate_path(path)?;
        let Some(entry) = self.entry(path) else {
            return Ok(RevisionRead::Missing {
                provenance: self.provenance.clone(),
            });
        };
        if entry.kind != RevisionEntryKind::RegularFile {
            return Ok(RevisionRead::NonRegular {
                kind: entry.kind,
                provenance: self.provenance.clone(),
            });
        }
        let bytes = self
            .repo
            .regular_blob_bytes_at_oid(self.commit_oid(), path)
            .map_err(|error| RevisionSourceError::ObjectUnavailable {
                oid: self.commit_oid().to_owned(),
                reason: error.to_string(),
            })?
            .ok_or_else(|| RevisionSourceError::ObjectUnavailable {
                oid: self.commit_oid().to_owned(),
                reason: format!("tree entry disappeared while reading {path}"),
            })?;
        Ok(RevisionRead::Bytes(classify_bytes(
            bytes,
            self.provenance.clone(),
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CapturedPathState {
    Bytes(Vec<u8>),
    Deleted,
    NonRegular { kind: RevisionEntryKind },
    Unreadable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CapturedTrackedChange {
    change: GitWorktreeChange,
    path_state: Option<CapturedPathState>,
}

/// Immutable run-start inventory and owned content for every tracked change.
///
/// `dirty_digest` is derived from this exact inventory and these exact owned
/// bytes/states. It is deliberately separate from the broader pack-level
/// worktree status digest, which may also include unrelated untracked paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedOverlayCapture {
    target_oid: String,
    dirty_digest: String,
    changes: Vec<CapturedTrackedChange>,
    captured_bytes: u64,
    byte_budget: u64,
}

impl TrackedOverlayCapture {
    /// Capture all tracked target-to-worktree changes now, before checks run.
    pub fn capture_tracked(
        repo: &Repository,
        target_oid: &str,
        byte_budget: u64,
    ) -> Result<Self, RevisionSourceError> {
        validate_exact_oid(target_oid)?;
        let changes = repo
            .worktree_changes_from_oid(target_oid)
            .map_err(|error| RevisionSourceError::WorktreeStatusUnavailable {
                reason: error.to_string(),
            })?;
        let mut remaining = byte_budget;
        let mut captured_bytes = 0_u64;
        let mut captured = Vec::with_capacity(changes.len());

        for change in changes {
            let path_state = capture_change_path(repo, &change, &mut remaining)?;
            if let Some(CapturedPathState::Bytes(bytes)) = &path_state {
                captured_bytes = captured_bytes.saturating_add(bytes.len() as u64);
            }
            captured.push(CapturedTrackedChange { change, path_state });
        }

        let dirty_digest = digest_tracked_capture(target_oid, &captured);
        Ok(Self {
            target_oid: target_oid.to_owned(),
            dirty_digest,
            changes: captured,
            captured_bytes,
            byte_budget,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn dirty_digest(&self) -> &str {
        &self.dirty_digest
    }

    pub fn captured_bytes(&self) -> u64 {
        self.captured_bytes
    }

    pub fn byte_budget(&self) -> u64 {
        self.byte_budget
    }
}

/// Tracked working-tree state frozen over one exact target commit.
pub struct CapturedWorkingTreeOverlay<'repo> {
    target: GitTree<'repo>,
    provenance: RevisionProvenance,
    entries: BTreeMap<String, RevisionEntry>,
    captured_reads: BTreeMap<String, CapturedPathState>,
}

impl<'repo> CapturedWorkingTreeOverlay<'repo> {
    /// Construct a revision source from an already-owned run-start capture.
    /// No current filesystem state is consulted here or by [`Self::read`].
    pub fn from_capture(
        repo: &'repo Repository,
        capture: TrackedOverlayCapture,
    ) -> Result<Self, RevisionSourceError> {
        if capture.dirty_digest.trim().is_empty() {
            return Err(RevisionSourceError::MissingDirtyDigest);
        }
        let target = GitTree::new(repo, &capture.target_oid)?;
        let provenance = RevisionProvenance::WorkingTreeOverlay {
            target_oid: capture.target_oid.clone(),
            dirty_digest: capture.dirty_digest.clone(),
        };
        let mut entries: BTreeMap<_, _> = target
            .entries
            .iter()
            .cloned()
            .map(|mut entry| {
                entry.provenance = provenance.clone();
                (entry.path.clone(), entry)
            })
            .collect();
        let mut captured_reads = BTreeMap::new();
        for captured in capture.changes {
            let read_path = captured_change_read_path(&captured.change);
            record_tracked_change(&mut entries, &provenance, captured.change);
            if let (Some(path), Some(path_state)) = (read_path, captured.path_state) {
                if let Some(entry) = entries.get_mut(&path) {
                    match &path_state {
                        CapturedPathState::Bytes(_) => {}
                        CapturedPathState::Deleted => {
                            entry.state = RevisionEntryState::Deleted;
                        }
                        CapturedPathState::NonRegular { kind } => {
                            entry.kind = *kind;
                            entry.state = RevisionEntryState::NonRegular { kind: *kind };
                        }
                        CapturedPathState::Unreadable { reason } => {
                            entry.state = RevisionEntryState::Unreadable {
                                reason: reason.clone(),
                            };
                        }
                    }
                }
                captured_reads.insert(path, path_state);
            }
        }
        Ok(Self {
            target,
            provenance,
            entries,
            captured_reads,
        })
    }
}

impl RevisionFileSource for CapturedWorkingTreeOverlay<'_> {
    fn provenance(&self) -> &RevisionProvenance {
        &self.provenance
    }

    fn entries(&self) -> Vec<RevisionEntry> {
        self.entries.values().cloned().collect()
    }

    fn read(&self, path: &str) -> Result<RevisionRead, RevisionSourceError> {
        validate_path(path)?;
        let Some(entry) = self.entries.get(path) else {
            return Ok(RevisionRead::Missing {
                provenance: self.provenance.clone(),
            });
        };
        match &entry.state {
            RevisionEntryState::Deleted => {
                return Ok(RevisionRead::Deleted {
                    provenance: self.provenance.clone(),
                });
            }
            RevisionEntryState::Renamed { to } => {
                return Ok(RevisionRead::Renamed {
                    to: to.clone(),
                    provenance: self.provenance.clone(),
                });
            }
            RevisionEntryState::NonRegular { kind } => {
                return Ok(RevisionRead::NonRegular {
                    kind: *kind,
                    provenance: self.provenance.clone(),
                });
            }
            RevisionEntryState::Unreadable { reason } => {
                return Ok(RevisionRead::Unreadable {
                    reason: reason.clone(),
                    provenance: self.provenance.clone(),
                });
            }
            RevisionEntryState::Present
            | RevisionEntryState::Added
            | RevisionEntryState::RenamedFrom { .. } => {}
        }
        if entry.kind != RevisionEntryKind::RegularFile {
            return Ok(RevisionRead::NonRegular {
                kind: entry.kind,
                provenance: self.provenance.clone(),
            });
        }

        if let Some(captured) = self.captured_reads.get(path) {
            return Ok(captured_revision_read(captured, &self.provenance));
        }

        // Unchanged paths are not worktree inputs. Delegate to the exact target
        // blob, then relabel only the source identity of the composed overlay.
        self.target
            .read(path)
            .map(|read| with_provenance(read, &self.provenance))
    }
}

fn capture_change_path(
    repo: &Repository,
    change: &GitWorktreeChange,
    remaining: &mut u64,
) -> Result<Option<CapturedPathState>, RevisionSourceError> {
    let Some(path) = captured_change_read_path(change) else {
        return Ok(None);
    };
    validate_path(&path)?;

    let state = match change.status {
        git2::Delta::Deleted => CapturedPathState::Deleted,
        git2::Delta::Typechange => CapturedPathState::NonRegular {
            kind: map_entry_kind(change.new_mode),
        },
        git2::Delta::Unreadable | git2::Delta::Conflicted => CapturedPathState::Unreadable {
            reason: format!(
                "Git status reported {:?} during tracked capture",
                change.status
            ),
        },
        _ if change.new_mode != GitTreeEntryKind::RegularFile => CapturedPathState::NonRegular {
            kind: map_entry_kind(change.new_mode),
        },
        _ => capture_regular_file(repo, &path, remaining),
    };
    Ok(Some(state))
}

fn captured_change_read_path(change: &GitWorktreeChange) -> Option<String> {
    match change.status {
        git2::Delta::Renamed | git2::Delta::Added => change.new_path.clone(),
        _ => change.old_path.clone().or_else(|| change.new_path.clone()),
    }
}

fn capture_regular_file(repo: &Repository, path: &str, remaining: &mut u64) -> CapturedPathState {
    let disk_path = repo.path().join(path);
    let file = match std::fs::File::open(&disk_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CapturedPathState::Deleted;
        }
        Err(error) => {
            return CapturedPathState::Unreadable {
                reason: format!("tracked capture could not open {path}: {error}"),
            };
        }
    };
    let declared_len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return CapturedPathState::Unreadable {
                reason: format!("tracked capture could not stat {path}: {error}"),
            };
        }
    };
    if declared_len > *remaining {
        return CapturedPathState::Unreadable {
            reason: format!(
                "tracked capture byte budget exceeded for {path}: {declared_len} bytes exceeds {remaining} remaining"
            ),
        };
    }

    let read_limit = remaining.saturating_add(1);
    let mut bytes = Vec::with_capacity(declared_len as usize);
    match file.take(read_limit).read_to_end(&mut bytes) {
        Ok(_) if bytes.len() as u64 <= *remaining => {
            *remaining -= bytes.len() as u64;
            CapturedPathState::Bytes(bytes)
        }
        Ok(_) => CapturedPathState::Unreadable {
            reason: format!(
                "tracked capture byte budget exceeded while reading {path}: more than {remaining} bytes"
            ),
        },
        Err(error) => CapturedPathState::Unreadable {
            reason: format!("tracked capture could not read {path}: {error}"),
        },
    }
}

fn digest_tracked_capture(target_oid: &str, changes: &[CapturedTrackedChange]) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"prview-tracked-overlay-v1");
    hash_field(&mut hasher, target_oid.as_bytes());
    for captured in changes {
        hash_field(&mut hasher, delta_name(captured.change.status).as_bytes());
        hash_optional_field(&mut hasher, captured.change.old_path.as_deref());
        hash_optional_field(&mut hasher, captured.change.new_path.as_deref());
        hash_field(
            &mut hasher,
            captured.change.new_mode_raw.to_string().as_bytes(),
        );
        match &captured.path_state {
            None => hash_field(&mut hasher, b"no-read-state"),
            Some(CapturedPathState::Bytes(bytes)) => {
                hash_field(&mut hasher, b"bytes");
                hash_field(&mut hasher, bytes);
            }
            Some(CapturedPathState::Deleted) => hash_field(&mut hasher, b"deleted"),
            Some(CapturedPathState::NonRegular { kind }) => {
                hash_field(&mut hasher, format!("non-regular:{kind:?}").as_bytes());
            }
            Some(CapturedPathState::Unreadable { reason }) => {
                hash_field(&mut hasher, b"unreadable");
                hash_field(&mut hasher, reason.as_bytes());
            }
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_optional_field(hasher: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash_field(hasher, b"some");
            hash_field(hasher, value.as_bytes());
        }
        None => hash_field(hasher, b"none"),
    }
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn delta_name(delta: git2::Delta) -> &'static str {
    match delta {
        git2::Delta::Unmodified => "unmodified",
        git2::Delta::Added => "added",
        git2::Delta::Deleted => "deleted",
        git2::Delta::Modified => "modified",
        git2::Delta::Renamed => "renamed",
        git2::Delta::Copied => "copied",
        git2::Delta::Ignored => "ignored",
        git2::Delta::Untracked => "untracked",
        git2::Delta::Typechange => "typechange",
        git2::Delta::Unreadable => "unreadable",
        git2::Delta::Conflicted => "conflicted",
    }
}

fn captured_revision_read(
    captured: &CapturedPathState,
    provenance: &RevisionProvenance,
) -> RevisionRead {
    match captured {
        CapturedPathState::Bytes(bytes) => {
            RevisionRead::Bytes(classify_bytes(bytes.clone(), provenance.clone()))
        }
        CapturedPathState::Deleted => RevisionRead::Deleted {
            provenance: provenance.clone(),
        },
        CapturedPathState::NonRegular { kind } => RevisionRead::NonRegular {
            kind: *kind,
            provenance: provenance.clone(),
        },
        CapturedPathState::Unreadable { reason } => RevisionRead::Unreadable {
            reason: reason.clone(),
            provenance: provenance.clone(),
        },
    }
}

fn with_provenance(read: RevisionRead, provenance: &RevisionProvenance) -> RevisionRead {
    match read {
        RevisionRead::Bytes(bytes) => {
            RevisionRead::Bytes(classify_bytes(bytes.bytes, provenance.clone()))
        }
        RevisionRead::Missing { .. } => RevisionRead::Missing {
            provenance: provenance.clone(),
        },
        RevisionRead::Deleted { .. } => RevisionRead::Deleted {
            provenance: provenance.clone(),
        },
        RevisionRead::Renamed { to, .. } => RevisionRead::Renamed {
            to,
            provenance: provenance.clone(),
        },
        RevisionRead::NonRegular { kind, .. } => RevisionRead::NonRegular {
            kind,
            provenance: provenance.clone(),
        },
        RevisionRead::Unreadable { reason, .. } => RevisionRead::Unreadable {
            reason,
            provenance: provenance.clone(),
        },
    }
}

fn record_tracked_change(
    entries: &mut BTreeMap<String, RevisionEntry>,
    provenance: &RevisionProvenance,
    change: GitWorktreeChange,
) {
    let old_path = change.old_path.as_deref();
    let new_path = change.new_path.as_deref();
    match change.status {
        git2::Delta::Added => {
            let Some(path) = new_path else { return };
            if entries.contains_key(path) {
                return;
            }
            entries.insert(
                path.to_owned(),
                overlay_only_entry(
                    path,
                    None,
                    change.new_mode_raw,
                    change.new_mode,
                    RevisionEntryState::Added,
                    provenance,
                ),
            );
        }
        git2::Delta::Renamed => {
            let Some(from) = old_path else {
                return;
            };
            let Some(to) = new_path else {
                if let Some(old_entry) = entries.get_mut(from) {
                    old_entry.state = RevisionEntryState::Unreadable {
                        reason: "rename target path is unavailable".to_owned(),
                    };
                }
                return;
            };
            if from == to {
                return;
            }
            let Some(source) = entries.get(from).cloned() else {
                return;
            };
            if let Some(old_entry) = entries.get_mut(from) {
                old_entry.state = RevisionEntryState::Renamed { to: to.to_owned() };
            }
            entries.insert(
                to.to_owned(),
                overlay_only_entry(
                    to,
                    source.baseline_object_id,
                    change.new_mode_raw,
                    change.new_mode,
                    RevisionEntryState::RenamedFrom {
                        from: from.to_owned(),
                    },
                    provenance,
                ),
            );
        }
        status => {
            let Some(path) = old_path else { return };
            let Some(entry) = entries.get_mut(path) else {
                return;
            };
            entry.state = match status {
                git2::Delta::Deleted => RevisionEntryState::Deleted,
                git2::Delta::Typechange => RevisionEntryState::NonRegular {
                    kind: map_entry_kind(change.new_mode),
                },
                git2::Delta::Unreadable | git2::Delta::Conflicted => {
                    RevisionEntryState::Unreadable {
                        reason: format!("Git status reported {status:?}"),
                    }
                }
                _ => RevisionEntryState::Present,
            };
            if status == git2::Delta::Typechange {
                entry.mode = change.new_mode_raw;
                entry.kind = map_entry_kind(change.new_mode);
            }
        }
    }
}

fn overlay_only_entry(
    path: &str,
    baseline_object_id: Option<String>,
    mode: u32,
    kind: GitTreeEntryKind,
    state: RevisionEntryState,
    provenance: &RevisionProvenance,
) -> RevisionEntry {
    RevisionEntry {
        path: path.to_owned(),
        baseline_object_id,
        mode,
        kind: map_entry_kind(kind),
        state,
        provenance: provenance.clone(),
    }
}

fn validate_exact_oid(oid: &str) -> Result<(), RevisionSourceError> {
    if oid.len() != 40 {
        return Err(RevisionSourceError::InvalidCommitOid {
            oid: oid.to_owned(),
            reason: "expected a full 40-character object id".to_owned(),
        });
    }
    git2::Oid::from_str(oid)
        .map(|_| ())
        .map_err(|error| RevisionSourceError::InvalidCommitOid {
            oid: oid.to_owned(),
            reason: error.to_string(),
        })
}

fn validate_path(path: &str) -> Result<(), RevisionSourceError> {
    crate::paths::validate_repo_relative_str(path)
        .map(|_| ())
        .map_err(|error| RevisionSourceError::InvalidRepoRelativePath {
            path: path.to_owned(),
            reason: error.to_string(),
        })
}

fn map_entry_kind(kind: GitTreeEntryKind) -> RevisionEntryKind {
    match kind {
        GitTreeEntryKind::RegularFile => RevisionEntryKind::RegularFile,
        GitTreeEntryKind::Symlink => RevisionEntryKind::Symlink,
        GitTreeEntryKind::Tree => RevisionEntryKind::Tree,
        GitTreeEntryKind::Gitlink => RevisionEntryKind::Gitlink,
        GitTreeEntryKind::Unsupported => RevisionEntryKind::Unsupported,
    }
}

fn classify_bytes(bytes: Vec<u8>, provenance: RevisionProvenance) -> RevisionBytes {
    let content_kind = if !bytes.contains(&0) && std::str::from_utf8(&bytes).is_ok() {
        RevisionContentKind::Utf8Text
    } else {
        RevisionContentKind::BinaryOrNonUtf8
    };
    RevisionBytes {
        bytes,
        content_kind,
        provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::signal::api_surface::{RustApiUnknownKind, snapshot_rust_api};
    use crate::git::git_cmd;
    use std::fs;
    use std::path::Path;

    fn run_git(repo: &Path, args: &[&str]) -> Vec<u8> {
        let output = git_cmd()
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn commit(repo: &Path, message: &str) -> String {
        run_git(repo, &["add", "-A"]);
        run_git(
            repo,
            &[
                "-c",
                "user.name=prview test",
                "-c",
                "user.email=prview@example.test",
                "commit",
                "-m",
                message,
            ],
        );
        String::from_utf8(run_git(repo, &["rev-parse", "HEAD"]))
            .expect("ASCII oid")
            .trim()
            .to_owned()
    }

    fn fixture_repo() -> (tempfile::TempDir, String, String) {
        let temp = tempfile::tempdir().expect("tempdir");
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::create_dir_all(temp.path().join("src")).expect("src dir");
        fs::write(
            temp.path().join("src/lib.rs"),
            b"pub fn value() -> u8 { 1 }\n",
        )
        .expect("text v1");
        fs::write(temp.path().join("binary.dat"), [0xff, 0x00, 0x80, b'X']).expect("binary v1");
        #[cfg(unix)]
        std::os::unix::fs::symlink("src/lib.rs", temp.path().join("link.rs")).expect("symlink");
        let first = commit(temp.path(), "first");

        fs::write(
            temp.path().join("src/lib.rs"),
            b"pub fn value() -> u8 { 2 }\n",
        )
        .expect("text v2");
        fs::write(temp.path().join("binary.dat"), [0xfe, 0x00, 0x81, b'Y']).expect("binary v2");
        let second = commit(temp.path(), "second");
        (temp, first, second)
    }

    fn bytes(read: RevisionRead) -> RevisionBytes {
        match read {
            RevisionRead::Bytes(bytes) => bytes,
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[test]
    fn revision_source_exact_tree_matches_git_show_and_exposes_modes() {
        let (temp, first, second) = fixture_repo();
        let repo = Repository::open(temp.path()).expect("repo");
        let source = GitTree::new(&repo, &first).expect("source");

        assert_eq!(
            source.provenance(),
            &RevisionProvenance::GitTree {
                commit_oid: first.clone()
            }
        );
        let paths: Vec<_> = source
            .entries()
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(paths, ["binary.dat", "link.rs", "src", "src/lib.rs"]);

        let text = bytes(source.read("src/lib.rs").expect("text"));
        assert_eq!(
            text.bytes,
            run_git(temp.path(), &["show", &format!("{first}:src/lib.rs")])
        );
        assert_eq!(text.content_kind, RevisionContentKind::Utf8Text);

        let binary = bytes(source.read("binary.dat").expect("binary"));
        assert_eq!(
            binary.bytes,
            run_git(temp.path(), &["show", &format!("{first}:binary.dat")])
        );
        assert_eq!(binary.content_kind, RevisionContentKind::BinaryOrNonUtf8);

        assert!(matches!(
            source.read("link.rs").expect("link state"),
            RevisionRead::NonRegular {
                kind: RevisionEntryKind::Symlink,
                ..
            }
        ));
        assert_ne!(first, second);
    }

    #[test]
    fn revision_source_missing_invalid_and_wrong_object_are_explicit() {
        let (temp, first, _) = fixture_repo();
        let repo = Repository::open(temp.path()).expect("repo");
        let source = GitTree::new(&repo, &first).expect("source");

        assert!(matches!(
            source.read("missing.rs").expect("missing state"),
            RevisionRead::Missing { .. }
        ));
        assert!(matches!(
            source.read("../outside.rs"),
            Err(RevisionSourceError::InvalidRepoRelativePath { .. })
        ));
        assert!(matches!(
            GitTree::new(&repo, "HEAD"),
            Err(RevisionSourceError::InvalidCommitOid { .. })
        ));

        let tree_oid = String::from_utf8(run_git(temp.path(), &["rev-parse", "HEAD^{tree}"]))
            .expect("tree oid")
            .trim()
            .to_owned();
        assert!(matches!(
            GitTree::new(&repo, &tree_oid),
            Err(RevisionSourceError::ObjectUnavailable { .. })
        ));
    }

    #[test]
    fn revision_source_multi_base_keeps_bytes_and_provenance_independent() {
        let (temp, first, second) = fixture_repo();
        let repo = Repository::open(temp.path()).expect("repo");
        let first_source = GitTree::new(&repo, &first).expect("first source");
        let second_source = GitTree::new(&repo, &second).expect("second source");

        let first_bytes = bytes(first_source.read("src/lib.rs").expect("first read"));
        let second_bytes = bytes(second_source.read("src/lib.rs").expect("second read"));
        assert_ne!(first_bytes.bytes, second_bytes.bytes);
        assert_eq!(
            first_bytes.provenance,
            RevisionProvenance::GitTree {
                commit_oid: first.clone()
            }
        );
        assert_eq!(
            second_bytes.provenance,
            RevisionProvenance::GitTree {
                commit_oid: second.clone()
            }
        );
        assert_eq!(
            first_bytes.bytes,
            run_git(temp.path(), &["show", &format!("{first}:src/lib.rs")])
        );
        assert_eq!(
            second_bytes.bytes,
            run_git(temp.path(), &["show", &format!("{second}:src/lib.rs")])
        );
        assert_eq!(
            bytes(first_source.read("binary.dat").expect("first binary")).bytes,
            run_git(temp.path(), &["show", &format!("{first}:binary.dat")])
        );
        assert_eq!(
            bytes(second_source.read("binary.dat").expect("second binary")).bytes,
            run_git(temp.path(), &["show", &format!("{second}:binary.dat")])
        );
    }

    #[test]
    fn revision_source_gitlink_is_explicit_and_never_followed() {
        let (temp, first, _) = fixture_repo();
        run_git(
            temp.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{first},nested-repo"),
            ],
        );
        run_git(
            temp.path(),
            &[
                "-c",
                "user.name=prview test",
                "-c",
                "user.email=prview@example.test",
                "commit",
                "-m",
                "gitlink",
            ],
        );
        let target = String::from_utf8(run_git(temp.path(), &["rev-parse", "HEAD"]))
            .expect("target oid")
            .trim()
            .to_owned();
        let repo = Repository::open(temp.path()).expect("repo");
        let source = GitTree::new(&repo, &target).expect("source");

        assert!(source.entries().iter().any(|entry| {
            entry.path == "nested-repo" && entry.kind == RevisionEntryKind::Gitlink
        }));
        assert!(matches!(
            source.read("nested-repo").expect("gitlink read"),
            RevisionRead::NonRegular {
                kind: RevisionEntryKind::Gitlink,
                ..
            }
        ));
    }

    #[test]
    fn revision_source_overlay_is_target_and_digest_bound_and_tracked_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::write(temp.path().join("changed.rs"), b"pub fn old() {}\n").expect("changed");
        fs::write(temp.path().join("deleted.rs"), b"pub fn gone() {}\n").expect("deleted");
        fs::write(temp.path().join("rename_me.rs"), b"pub fn moved() {}\n").expect("rename");
        let target = commit(temp.path(), "target");

        fs::write(temp.path().join("changed.rs"), b"pub fn current() {}\n").expect("edit");
        fs::remove_file(temp.path().join("deleted.rs")).expect("delete tracked fixture");
        run_git(temp.path(), &["mv", "rename_me.rs", "renamed.rs"]);
        fs::write(
            temp.path().join("staged_added.rs"),
            b"pub fn staged_addition() {}\n",
        )
        .expect("staged addition");
        run_git(temp.path(), &["add", "staged_added.rs"]);
        fs::write(temp.path().join("untracked.rs"), b"pub fn unrelated() {}\n").expect("untracked");

        let repo = Repository::open(temp.path()).expect("repo");
        let capture =
            TrackedOverlayCapture::capture_tracked(&repo, &target, TRACKED_CAPTURE_BYTE_BUDGET)
                .expect("capture");
        assert!(!capture.is_empty());
        assert!(capture.captured_bytes() > 0);
        assert_eq!(capture.byte_budget(), TRACKED_CAPTURE_BYTE_BUDGET);
        let digest = capture.dirty_digest().to_owned();
        let overlay = CapturedWorkingTreeOverlay::from_capture(&repo, capture).expect("overlay");
        let expected_provenance = RevisionProvenance::WorkingTreeOverlay {
            target_oid: target.clone(),
            dirty_digest: digest,
        };
        assert_eq!(overlay.provenance(), &expected_provenance);
        let entries = overlay.entries();
        let paths: Vec<_> = entries.iter().map(|entry| entry.path.as_str()).collect();
        let mut sorted_paths = paths.clone();
        sorted_paths.sort_unstable();
        assert_eq!(paths, sorted_paths, "overlay inventory must be path-sorted");

        let changed = entries
            .iter()
            .find(|entry| entry.path == "changed.rs")
            .expect("changed target entry");
        assert!(changed.baseline_object_id.is_some());
        assert_eq!(changed.provenance, expected_provenance);
        assert_eq!(
            bytes(overlay.read("changed.rs").expect("changed read")),
            RevisionBytes {
                bytes: b"pub fn current() {}\n".to_vec(),
                content_kind: RevisionContentKind::Utf8Text,
                provenance: expected_provenance.clone(),
            }
        );

        let deleted = entries
            .iter()
            .find(|entry| entry.path == "deleted.rs")
            .expect("deleted target entry");
        assert!(matches!(deleted.state, RevisionEntryState::Deleted));
        assert!(deleted.baseline_object_id.is_some());
        assert_eq!(
            overlay.read("deleted.rs").expect("deleted read"),
            RevisionRead::Deleted {
                provenance: expected_provenance.clone()
            }
        );

        let rename_source = entries
            .iter()
            .find(|entry| entry.path == "rename_me.rs")
            .expect("rename source entry");
        assert!(matches!(
            &rename_source.state,
            RevisionEntryState::Renamed { to } if to == "renamed.rs"
        ));
        assert_eq!(
            overlay.read("rename_me.rs").expect("rename read"),
            RevisionRead::Renamed {
                to: "renamed.rs".to_owned(),
                provenance: expected_provenance.clone(),
            }
        );

        let additions: Vec<_> = entries
            .iter()
            .filter(|entry| entry.path == "staged_added.rs")
            .collect();
        assert_eq!(additions.len(), 1, "staged addition must appear once");
        let addition = additions[0];
        assert!(matches!(addition.state, RevisionEntryState::Added));
        assert_eq!(addition.baseline_object_id, None);
        assert_eq!(addition.mode, 0o100644);
        assert_eq!(addition.kind, RevisionEntryKind::RegularFile);
        assert_eq!(addition.provenance, expected_provenance);
        assert_eq!(
            bytes(
                overlay
                    .read("staged_added.rs")
                    .expect("staged addition read")
            ),
            RevisionBytes {
                bytes: b"pub fn staged_addition() {}\n".to_vec(),
                content_kind: RevisionContentKind::Utf8Text,
                provenance: expected_provenance.clone(),
            }
        );

        let destinations: Vec<_> = entries
            .iter()
            .filter(|entry| entry.path == "renamed.rs")
            .collect();
        assert_eq!(destinations.len(), 1, "rename destination must appear once");
        let destination = destinations[0];
        assert!(matches!(
            &destination.state,
            RevisionEntryState::RenamedFrom { from } if from == "rename_me.rs"
        ));
        assert_eq!(
            destination.baseline_object_id,
            rename_source.baseline_object_id
        );
        assert_eq!(destination.provenance, expected_provenance);
        assert_eq!(
            bytes(overlay.read("renamed.rs").expect("rename destination read")),
            RevisionBytes {
                bytes: b"pub fn moved() {}\n".to_vec(),
                content_kind: RevisionContentKind::Utf8Text,
                provenance: expected_provenance.clone(),
            }
        );

        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.path == "untracked.rs")
                .count(),
            0
        );
        assert_eq!(
            overlay.read("untracked.rs").expect("untracked read"),
            RevisionRead::Missing {
                provenance: expected_provenance
            }
        );
    }

    #[test]
    fn revision_source_overlay_does_not_infer_unstaged_delete_untracked_rename() {
        let temp = tempfile::tempdir().expect("tempdir");
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::write(temp.path().join("old.rs"), b"pub fn moved() {}\n").expect("old path");
        let target = commit(temp.path(), "target");

        fs::rename(temp.path().join("old.rs"), temp.path().join("new.rs"))
            .expect("filesystem-only rename");

        let repo = Repository::open(temp.path()).expect("repo");
        let capture =
            TrackedOverlayCapture::capture_tracked(&repo, &target, TRACKED_CAPTURE_BYTE_BUDGET)
                .expect("capture");
        let overlay = CapturedWorkingTreeOverlay::from_capture(&repo, capture).expect("overlay");

        assert!(matches!(
            overlay.read("old.rs").expect("old read"),
            RevisionRead::Deleted { .. }
        ));
        assert!(matches!(
            overlay.read("new.rs").expect("new read"),
            RevisionRead::Missing { .. }
        ));
        assert!(!overlay.entries().iter().any(|entry| entry.path == "new.rs"));
    }

    #[test]
    fn revision_source_capture_owns_changed_bytes_and_delegates_unchanged_to_git() {
        let temp = tempfile::tempdir().expect("tempdir");
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::write(
            temp.path().join("changed.rs"),
            b"pub fn target_changed() {}\n",
        )
        .expect("changed target");
        fs::write(
            temp.path().join("unchanged.rs"),
            b"pub fn target_unchanged() {}\n",
        )
        .expect("unchanged target");
        let target = commit(temp.path(), "target");

        fs::write(temp.path().join("changed.rs"), b"pub fn captured() {}\n")
            .expect("captured change");
        let repo = Repository::open(temp.path()).expect("repo");
        let capture =
            TrackedOverlayCapture::capture_tracked(&repo, &target, TRACKED_CAPTURE_BYTE_BUDGET)
                .expect("capture");

        // Both paths move after capture. The tracked changed path must retain
        // its owned bytes; the previously unchanged path must read the exact
        // target Git blob rather than the later filesystem bytes.
        fs::write(
            temp.path().join("changed.rs"),
            b"pub fn later_changed() {}\n",
        )
        .expect("later changed");
        fs::write(
            temp.path().join("unchanged.rs"),
            b"pub fn later_unchanged() {}\n",
        )
        .expect("later unchanged");
        let overlay = CapturedWorkingTreeOverlay::from_capture(&repo, capture).expect("overlay");

        assert_eq!(
            bytes(overlay.read("changed.rs").expect("changed read")).bytes,
            b"pub fn captured() {}\n"
        );
        assert_eq!(
            bytes(overlay.read("unchanged.rs").expect("unchanged read")).bytes,
            b"pub fn target_unchanged() {}\n"
        );
    }

    #[test]
    fn revision_source_tracked_digest_ignores_unrelated_untracked_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::write(temp.path().join("tracked.rs"), b"pub fn target() {}\n").expect("target");
        let target = commit(temp.path(), "target");
        fs::write(temp.path().join("tracked.rs"), b"pub fn captured() {}\n")
            .expect("tracked change");
        let repo = Repository::open(temp.path()).expect("repo");

        let first =
            TrackedOverlayCapture::capture_tracked(&repo, &target, TRACKED_CAPTURE_BYTE_BUDGET)
                .expect("first capture");
        fs::write(
            temp.path().join("unrelated-untracked.rs"),
            b"pub fn unrelated() {}\n",
        )
        .expect("untracked");
        let second =
            TrackedOverlayCapture::capture_tracked(&repo, &target, TRACKED_CAPTURE_BYTE_BUDGET)
                .expect("second capture");

        assert_eq!(first.dirty_digest(), second.dirty_digest());
        assert_eq!(first.captured_bytes(), second.captured_bytes());
    }

    #[test]
    fn revision_source_capture_budget_exhaustion_is_typed_unknown_evidence() {
        let temp = tempfile::tempdir().expect("tempdir");
        run_git(temp.path(), &["init", "-q", "-b", "main"]);
        fs::create_dir_all(temp.path().join("src")).expect("src");
        fs::write(
            temp.path().join("Cargo.toml"),
            b"[package]\nname='fixture'\nversion='0.0.0'\n[lib]\npath='src/lib.rs'\n",
        )
        .expect("manifest");
        fs::write(temp.path().join("src/lib.rs"), b"pub fn target() {}\n").expect("target");
        let target = commit(temp.path(), "target");
        fs::write(
            temp.path().join("src/lib.rs"),
            b"pub fn captured_but_over_budget() {}\n",
        )
        .expect("oversized change");
        let repo = Repository::open(temp.path()).expect("repo");
        let capture =
            TrackedOverlayCapture::capture_tracked(&repo, &target, 4).expect("bounded capture");
        let overlay = CapturedWorkingTreeOverlay::from_capture(&repo, capture).expect("overlay");

        assert!(matches!(
            overlay.read("src/lib.rs").expect("typed read"),
            RevisionRead::Unreadable { ref reason, .. }
                if reason.contains("tracked capture byte budget exceeded")
        ));
        let snapshot = snapshot_rust_api(&overlay);
        assert!(
            snapshot.unknowns.iter().any(|unknown| {
                matches!(
                    unknown.kind,
                    RustApiUnknownKind::SourceRead | RustApiUnknownKind::MissingLibRoot
                ) && unknown.source_path == "src/lib.rs"
                    && unknown
                        .evidence
                        .contains("tracked capture byte budget exceeded")
            }),
            "typed snapshot unknowns: {:#?}",
            snapshot.unknowns
        );
    }
}
