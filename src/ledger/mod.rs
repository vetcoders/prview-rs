//! Run-wide task ledger: one record per unit of work the run considered.
//!
//! A run does the same work twice for structural reasons, not accidental ones:
//! a gate and a context artifact can be the same tool reading the same tree
//! under two different names ("TypeScript" the check, a `tsc` trace the
//! artifact), and nothing in the pipeline holds a record that could pair them.
//! The ledger is that record — every task states WHAT tool ran and WHICH
//! substrate it read, so a second request for the same pair is answerable
//! without re-running the tool.
//!
//! Equivalence is the whole point, so the key is deliberately semantic rather
//! than literal. [`TaskKey::new`] normalises the tool name through
//! [`crate::check_id::check_id_from_name`] — the repository's existing canon for
//! "which gate is this" — instead of introducing a second alias table that would
//! be free to drift from the first one.
//!
//! This module is the data model only: it records outcomes and answers lookups.
//! It never runs, skips or caches anything itself.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::checks::{ScanSubstrate, TreeState};
use crate::git::WorktreeSnapshot;

/// Semantic equivalence key: "the same work, on the same substrate".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskKey {
    /// Canonical tool id, normalised the same way check ids are, so a check and
    /// a context artifact backed by the same tool land on one key.
    pub tool: String,
    pub substrate: SubstrateKey,
}

impl TaskKey {
    /// Build a key from a tool's display name (`"TypeScript"`, `"Cargo check"`)
    /// and the substrate it read.
    #[must_use]
    pub fn new(tool_name: &str, substrate: SubstrateKey) -> Self {
        Self {
            tool: crate::check_id::check_id_from_name(tool_name),
            substrate,
        }
    }
}

/// The tree a task read, reduced to the two facts that decide equivalence: the
/// commit whose bytes were scanned, and whether that tree was a snapshot, the
/// live working tree, or something else entirely.
///
/// Both fields are optional for the same reason [`ScanSubstrate`] makes them
/// optional: a substrate that could not be resolved stays visibly unknown
/// instead of being certified as anything. Two unknown substrates are equal as
/// keys — which is correct, because an unknown substrate carries no evidence
/// that two tasks read different trees, and a caller that needs certainty must
/// check the fields rather than the key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SubstrateKey {
    pub target_sha: Option<String>,
    pub tree_state: Option<TreeState>,
}

impl From<ScanSubstrate> for SubstrateKey {
    fn from(substrate: ScanSubstrate) -> Self {
        Self {
            target_sha: substrate.target_sha,
            tree_state: substrate.tree_state,
        }
    }
}

/// How a task was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    /// The tool executed.
    Run { duration: Duration },
    /// A stored result was replayed instead of executing the tool. `origin` is
    /// the substrate the ORIGINAL execution read, which is what makes a replay
    /// auditable: it may differ from the substrate of the run replaying it.
    Cached {
        cache_age_secs: Option<u64>,
        origin: SubstrateKey,
    },
    /// The task was applicable but ruled out for this run (disabled gate,
    /// missing tool, unreachable root).
    Skipped { reason: String },
    /// The task does not apply to this repository at all.
    NotApplicable { reason: String },
}

/// Which surface asked for the work. Two kinds can share one [`TaskKey`] — that
/// is precisely the duplication the ledger exists to make visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Check,
    ContextArtifact,
}

/// One recorded task.
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub key: TaskKey,
    pub kind: TaskKind,
    pub state: TaskState,
    /// When the task entered the execution queue.
    pub queued_at: Option<Instant>,
    /// When the task actually started running.
    pub started_at: Option<Instant>,
}

/// The run's ledger.
///
/// Shared across the run's concurrent tasks by reference (`&TaskLedger`), so
/// every field sits behind its own mutex and no lock is ever held across an
/// `await`. Poisoning is recovered from rather than propagated: a panicking
/// task must not turn the whole ledger into a run-ending error.
#[derive(Default)]
pub struct TaskLedger {
    entries: Mutex<Vec<TaskEntry>>,
    /// The one target snapshot the run shares. Materialising it is still the
    /// caller's job today; the ledger only holds it so a later stage can borrow
    /// the same scan directory instead of building a second worktree.
    shared_snapshot: Mutex<Option<WorktreeSnapshot>>,
    resolved_substrate: Mutex<Option<SubstrateKey>>,
}

impl TaskLedger {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a resolved task.
    pub fn record(&self, entry: TaskEntry) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(entry);
    }

    /// The most recent entry recorded under `key`, or `None`.
    ///
    /// Returns an owned snapshot rather than a reference: handing out a
    /// borrow would mean handing out the lock guard with it, and a caller
    /// holding that across an `await` would deadlock the run.
    #[must_use]
    pub fn lookup(&self, key: &TaskKey) -> Option<TaskEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|entry| &entry.key == key)
            .cloned()
    }

    /// Every recorded entry, in the order it was recorded.
    #[must_use]
    pub fn entries(&self) -> Vec<TaskEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Install the substrate this run resolved once, for tasks that have no
    /// provenance of their own to report.
    pub fn set_substrate(&self, substrate: SubstrateKey) {
        *self
            .resolved_substrate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(substrate);
    }

    #[must_use]
    pub fn resolved_substrate(&self) -> Option<SubstrateKey> {
        self.resolved_substrate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Hand the run's shared target snapshot to the ledger, which keeps it
    /// alive for as long as the ledger lives.
    pub fn set_shared_snapshot(&self, snapshot: Option<WorktreeSnapshot>) {
        *self
            .shared_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = snapshot;
    }

    /// The directory the shared snapshot was materialised into, if there is one.
    #[must_use]
    pub fn scan_dir(&self) -> Option<PathBuf> {
        self.shared_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|snapshot| snapshot.worktree_path.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::collections::{HashMap, HashSet};
    use std::hash::{Hash, Hasher};

    fn substrate(sha: &str) -> SubstrateKey {
        SubstrateKey {
            target_sha: Some(sha.to_string()),
            tree_state: Some(TreeState::Snapshot),
        }
    }

    fn hash_of(key: &TaskKey) -> u64 {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    fn entry(key: TaskKey, state: TaskState) -> TaskEntry {
        TaskEntry {
            key,
            kind: TaskKind::Check,
            state,
            queued_at: None,
            started_at: None,
        }
    }

    /// The point of the ledger: a check and a context artifact naming the same
    /// tool differently must land on ONE key, without a second alias table.
    #[test]
    fn the_same_tool_on_the_same_substrate_is_one_key() {
        let a = TaskKey::new("TypeScript", substrate("abc"));
        let b = TaskKey::new("typescript", substrate("abc"));
        let c = TaskKey::new("tsc", substrate("abc"));

        assert_eq!(a, b, "display-name casing must not partition a tool");
        assert_eq!(a, c, "the alias and its id must be the same task");
        assert_eq!(hash_of(&a), hash_of(&c), "equal keys must hash equally");

        let mut map: HashMap<TaskKey, &str> = HashMap::new();
        map.insert(a, "first");
        map.insert(c, "second");
        assert_eq!(map.len(), 1, "one tool, one substrate, one map slot");
    }

    #[test]
    fn a_different_substrate_is_a_different_key() {
        let base = TaskKey::new("Clippy", substrate("abc"));
        let other_sha = TaskKey::new("Clippy", substrate("def"));
        let other_tree = TaskKey::new(
            "Clippy",
            SubstrateKey {
                target_sha: Some("abc".to_string()),
                tree_state: Some(TreeState::LocalDirty),
            },
        );
        let unknown = TaskKey::new("Clippy", SubstrateKey::default());

        assert_ne!(base, other_sha, "a different commit is different work");
        assert_ne!(
            base, other_tree,
            "the same commit read from a dirty local tree is different work"
        );
        assert_ne!(base, unknown, "an unresolved substrate is not a match");

        let keys: HashSet<TaskKey> = [base, other_sha, other_tree, unknown].into_iter().collect();
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn a_different_tool_on_one_substrate_is_a_different_key() {
        assert_ne!(
            TaskKey::new("Clippy", substrate("abc")),
            TaskKey::new("Rustfmt", substrate("abc")),
        );
    }

    #[test]
    fn recorded_tasks_are_looked_up_by_key() {
        let ledger = TaskLedger::new();
        let key = TaskKey::new("Cargo check", substrate("abc"));

        assert!(
            ledger.lookup(&key).is_none(),
            "an empty ledger knows nothing"
        );

        ledger.record(entry(
            key.clone(),
            TaskState::Run {
                duration: Duration::from_secs(3),
            },
        ));

        let found = ledger.lookup(&key).expect("recorded task must be found");
        assert_eq!(
            found.state,
            TaskState::Run {
                duration: Duration::from_secs(3)
            }
        );
        assert_eq!(found.kind, TaskKind::Check);
        assert!(
            ledger
                .lookup(&TaskKey::new("Cargo check", substrate("def")))
                .is_none(),
            "the same tool on another commit is not this task"
        );
        assert_eq!(ledger.entries().len(), 1);
    }

    #[test]
    fn every_state_survives_a_round_trip() {
        let ledger = TaskLedger::new();
        let states = [
            TaskState::Run {
                duration: Duration::from_millis(120),
            },
            TaskState::Cached {
                cache_age_secs: Some(42),
                origin: substrate("abc"),
            },
            TaskState::Skipped {
                reason: "lint disabled".to_string(),
            },
            TaskState::NotApplicable {
                reason: "profile rust".to_string(),
            },
        ];

        for (i, state) in states.iter().enumerate() {
            let key = TaskKey::new(&format!("tool {i}"), substrate("abc"));
            ledger.record(entry(key.clone(), state.clone()));
            assert_eq!(&ledger.lookup(&key).expect("recorded").state, state);
        }
        assert_eq!(ledger.entries().len(), states.len());
    }

    /// A re-record under the same key answers with the LATEST outcome — the
    /// ledger reports what a task resolved to now, not what it once was.
    #[test]
    fn the_latest_record_wins_a_lookup() {
        let ledger = TaskLedger::new();
        let key = TaskKey::new("Ruff", substrate("abc"));

        ledger.record(entry(
            key.clone(),
            TaskState::Skipped {
                reason: "tool missing".to_string(),
            },
        ));
        ledger.record(entry(
            key.clone(),
            TaskState::Run {
                duration: Duration::from_secs(1),
            },
        ));

        assert_eq!(
            ledger.lookup(&key).expect("recorded").state,
            TaskState::Run {
                duration: Duration::from_secs(1)
            }
        );
        assert_eq!(
            ledger.entries().len(),
            2,
            "history is kept, not overwritten"
        );
    }

    #[test]
    fn a_scan_substrate_converts_without_loss() {
        let resolved = ScanSubstrate {
            target_sha: Some("abc".to_string()),
            tree_state: Some(TreeState::SnapshotBorrowedDeps),
        };
        assert_eq!(
            SubstrateKey::from(resolved),
            SubstrateKey {
                target_sha: Some("abc".to_string()),
                tree_state: Some(TreeState::SnapshotBorrowedDeps),
            }
        );
        assert_eq!(
            SubstrateKey::from(ScanSubstrate::default()),
            SubstrateKey::default(),
            "an unresolved substrate stays unknown rather than becoming a guess"
        );
    }

    #[test]
    fn substrate_and_snapshot_start_unset() {
        let ledger = TaskLedger::new();
        assert!(ledger.resolved_substrate().is_none());
        assert!(
            ledger.scan_dir().is_none(),
            "no shared snapshot means no scan dir to borrow"
        );

        ledger.set_substrate(substrate("abc"));
        assert_eq!(ledger.resolved_substrate(), Some(substrate("abc")));

        ledger.set_shared_snapshot(None);
        assert!(ledger.scan_dir().is_none());
    }
}
