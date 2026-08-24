//! Revision-bound repository file access for language backends.
//!
//! The source contract keeps bytes attached to one exact Git commit, or to a
//! tracked working-tree overlay whose target commit and pre-captured dirty
//! digest are supplied together. Overlay inventory admits only exact target
//! entries and extra paths supplied by tracked Git status; untracked paths
//! remain outside both inventory and reads. It deliberately performs no
//! language or API classification.

use crate::git::{GitTreeEntryKind, GitWorktreeChange, Repository};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;

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

/// Tracked working-tree state over one exact target commit.
pub struct WorkingTreeOverlay<'repo> {
    target: GitTree<'repo>,
    provenance: RevisionProvenance,
    entries: BTreeMap<String, RevisionEntry>,
}

impl<'repo> WorkingTreeOverlay<'repo> {
    pub fn new(
        repo: &'repo Repository,
        target_oid: &str,
        dirty_digest: impl Into<String>,
    ) -> Result<Self, RevisionSourceError> {
        let dirty_digest = dirty_digest.into();
        if dirty_digest.trim().is_empty() {
            return Err(RevisionSourceError::MissingDirtyDigest);
        }
        let target = GitTree::new(repo, target_oid)?;
        let provenance = RevisionProvenance::WorkingTreeOverlay {
            target_oid: target_oid.to_owned(),
            dirty_digest,
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
        let changes = repo
            .worktree_changes_from_oid(target_oid)
            .map_err(|error| RevisionSourceError::WorktreeStatusUnavailable {
                reason: error.to_string(),
            })?;
        for change in changes {
            record_tracked_change(&mut entries, &provenance, change);
        }
        Ok(Self {
            target,
            provenance,
            entries,
        })
    }
}

impl RevisionFileSource for WorkingTreeOverlay<'_> {
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

        let disk_path = self.target.repo.path().join(path);
        let metadata = match fs::symlink_metadata(&disk_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RevisionRead::Deleted {
                    provenance: self.provenance.clone(),
                });
            }
            Err(error) => {
                return Ok(RevisionRead::Unreadable {
                    reason: error.to_string(),
                    provenance: self.provenance.clone(),
                });
            }
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Ok(RevisionRead::NonRegular {
                kind: RevisionEntryKind::Symlink,
                provenance: self.provenance.clone(),
            });
        }
        if !file_type.is_file() {
            return Ok(RevisionRead::NonRegular {
                kind: RevisionEntryKind::Tree,
                provenance: self.provenance.clone(),
            });
        }
        match fs::read(disk_path) {
            Ok(bytes) => Ok(RevisionRead::Bytes(classify_bytes(
                bytes,
                self.provenance.clone(),
            ))),
            Err(error) => Ok(RevisionRead::Unreadable {
                reason: error.to_string(),
                provenance: self.provenance.clone(),
            }),
        }
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
    use crate::git::git_cmd;
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
        let digest = "sha256:captured-before-analysis";
        let overlay = WorkingTreeOverlay::new(&repo, &target, digest).expect("overlay");
        let expected_provenance = RevisionProvenance::WorkingTreeOverlay {
            target_oid: target.clone(),
            dirty_digest: digest.to_owned(),
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
        assert!(matches!(
            WorkingTreeOverlay::new(&repo, &target, ""),
            Err(RevisionSourceError::MissingDirtyDigest)
        ));
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
        let overlay =
            WorkingTreeOverlay::new(&repo, &target, "sha256:unstaged-pair").expect("overlay");

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
}
