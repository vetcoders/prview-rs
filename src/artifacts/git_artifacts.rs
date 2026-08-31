//! Git-derived pack files: file status, commit list, per-commit diffs, full patch.

use super::*;

#[cfg(unix)]
use anyhow::Context;
#[cfg(unix)]
use serde::{Deserialize, Serialize};

/// Durable intent for the only cross-file part of run publication.
///
/// `index.jsonl` and the per-branch `latest` symlink cannot be replaced in one
/// filesystem operation. This record lets the next publisher reconcile an
/// interrupted process to the persisted index while holding the same global
/// publication lock.
#[cfg(unix)]
#[derive(Debug, Serialize, Deserialize)]
struct LatestPublicationRecord {
    schema: u8,
    out_dir: Vec<u8>,
    previous_target: Option<Vec<u8>>,
}

#[cfg(unix)]
pub(crate) struct LatestPublication {
    record: LatestPublicationRecord,
}

#[cfg(not(unix))]
pub(crate) struct LatestPublication;

pub(super) fn generate_file_status(dir: &Path, diffs: &[Diff]) -> Result<()> {
    let mut content = String::new();

    for diff in diffs {
        for file in &diff.files {
            let status_char = match file.status {
                crate::git::FileStatus::Added => 'A',
                crate::git::FileStatus::Modified => 'M',
                crate::git::FileStatus::Deleted => 'D',
                crate::git::FileStatus::Renamed => 'R',
                crate::git::FileStatus::Copied => 'C',
            };
            content.push_str(&format!("{}\t{}\n", status_char, file.path));
        }
    }

    fs::write(dir.join("file-status.txt"), content)?;
    Ok(())
}

pub(super) fn generate_commit_list(dir: &Path, diffs: &[Diff]) -> Result<()> {
    let mut content = String::new();

    if let Some(diff) = diffs.first() {
        for commit in &diff.commits {
            content.push_str(&format!(
                "{} {} {} {}\n",
                commit.short_id, commit.date, commit.author, commit.message
            ));
        }
    }

    if content.is_empty() {
        content = "(no commits)\n".to_string();
    }

    fs::write(dir.join("commit-list.txt"), content)?;
    Ok(())
}

/// Create `latest` and return the target it replaced in the same critical section.
#[cfg(all(test, unix))]
fn create_latest_symlink(out_dir: &Path) -> Result<Option<std::ffi::OsString>> {
    if let (Some(parent), Some(basename)) = (out_dir.parent(), out_dir.file_name()) {
        return with_latest_lock(parent, || {
            let previous = match fs::read_link(parent.join("latest")) {
                Ok(target) => Some(target.into_os_string()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            replace_latest_target(parent, basename)?;
            Ok(previous)
        });
    }
    Ok(None)
}

/// Put `latest` back to `previous`, or remove it when this run created the alias.
#[cfg(all(test, unix))]
fn restore_latest_symlink(out_dir: &Path, previous: Option<&std::ffi::OsStr>) -> Result<()> {
    let (Some(parent), Some(our_target)) = (out_dir.parent(), out_dir.file_name()) else {
        return Ok(());
    };
    with_latest_lock(parent, || {
        restore_latest_target_unlocked(parent, our_target, previous)
    })
}

#[cfg(unix)]
fn restore_latest_target_unlocked(
    parent: &Path,
    our_target: &std::ffi::OsStr,
    previous: Option<&std::ffi::OsStr>,
) -> Result<()> {
    let latest_link = parent.join("latest");
    let current = match fs::read_link(&latest_link) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if current.as_os_str() != our_target {
        return Ok(());
    }
    if let Some(name) = previous {
        replace_latest_target(parent, name)
    } else {
        fs::remove_file(latest_link)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("Failed to sync latest directory {}", parent.display()))?;
        Ok(())
    }
}

#[cfg(unix)]
fn latest_publication_record_path() -> PathBuf {
    crate::config::prview_home().join("publication-transaction.json")
}

#[cfg(unix)]
fn capture_native_path(path: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes().to_vec()
}

#[cfg(unix)]
fn restore_native_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(bytes))
}

#[cfg(unix)]
fn validate_latest_target_name(target: &std::ffi::OsStr) -> Result<()> {
    let mut components = Path::new(target).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(name)), None) if name != "latest" => Ok(()),
        _ => anyhow::bail!(
            "latest target must be one local pack directory name, got {:?}",
            target
        ),
    }
}

#[cfg(unix)]
fn validate_recovery_pack_identity(out_dir: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(out_dir).with_context(|| {
        format!(
            "Publication transaction points to missing pack {}",
            out_dir.display()
        )
    })?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "Publication transaction pack is not an owned directory: {}",
            out_dir.display()
        );
    }
    let run_path = out_dir.join("00_summary").join("RUN.json");
    let run: serde_json::Value =
        serde_json::from_slice(&fs::read(&run_path).with_context(|| {
            format!(
                "Publication transaction pack has no readable identity {}",
                run_path.display()
            )
        })?)
        .with_context(|| format!("Invalid publication pack identity {}", run_path.display()))?;
    let Some(recorded_root) = run
        .get("artifacts_root")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
    else {
        anyhow::bail!(
            "Publication pack identity has no artifacts_root: {}",
            run_path.display()
        );
    };
    let same_path = recorded_root == out_dir
        || (fs::canonicalize(&recorded_root).ok() == fs::canonicalize(out_dir).ok()
            && fs::canonicalize(out_dir).is_ok());
    if !same_path {
        anyhow::bail!(
            "Publication transaction path {} does not match pack identity {}",
            out_dir.display(),
            recorded_root.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn write_latest_publication_record(record: &LatestPublicationRecord) -> Result<()> {
    let path = latest_publication_record_path();
    write_latest_publication_record_at(&path, record)
}

#[cfg(unix)]
fn write_latest_publication_record_at(path: &Path, record: &LatestPublicationRecord) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("publication transaction has no parent"))?;
    fs::create_dir_all(parent)?;
    let (temp, mut file) = crate::storage::create_owned_temp_file(parent, "publication-journal")?;
    let write_result = (|| -> Result<()> {
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = crate::storage::atomic_replace_file(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn clear_latest_publication_record() -> Result<()> {
    let path = latest_publication_record_path();
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                let _ = File::open(parent).and_then(|directory| directory.sync_all());
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Preserve an invalid crash journal as evidence without letting it permanently
/// deny every later publication. The destination is an owned create-new file,
/// so the rename cannot overwrite an operator file even if names collide.
#[cfg(unix)]
fn quarantine_invalid_latest_publication_record(
    path: &Path,
    reason: &anyhow::Error,
) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("publication transaction has no parent"))?;
    let (quarantine, placeholder) =
        crate::storage::create_owned_temp_file(parent, "publication-transaction.invalid")?;
    drop(placeholder);
    if let Err(error) = fs::rename(path, &quarantine) {
        let _ = fs::remove_file(&quarantine);
        return Err(error).with_context(|| {
            format!(
                "Failed to quarantine invalid publication transaction {}",
                path.display()
            )
        });
    }
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    eprintln!(
        "prview: preserved invalid publication journal as {} and will continue: {reason:#}",
        quarantine.display()
    );
    Ok(quarantine)
}

/// Reconcile a hard-crashed publication to the durable index.
///
/// The index is the canonical ordered publication ledger. If it contains a
/// later run for the same branch directory, that run wins; otherwise the saved
/// predecessor is restored only while the interrupted run still owns `latest`.
#[cfg(unix)]
pub(crate) fn recover_latest_publication(
    _publication: &crate::storage::RunPublicationLock,
) -> Result<()> {
    let path = latest_publication_record_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let record: LatestPublicationRecord = match serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid publication transaction {}", path.display()))
    {
        Ok(record) => record,
        Err(error) => {
            quarantine_invalid_latest_publication_record(&path, &error)?;
            return Ok(());
        }
    };

    // Index I/O or parse failure is not evidence that the publication journal
    // is invalid. Preserve the journal and stop so a later healthy process can
    // reconcile against the complete durable ledger.
    let index = crate::storage::RunIndex::load_strict()?;
    let recovery = reconcile_latest_publication_record_with_index(&record, true, &index);
    match recovery {
        Ok(()) => clear_latest_publication_record(),
        Err(error) => {
            quarantine_invalid_latest_publication_record(&path, &error)?;
            Ok(())
        }
    }
}

#[cfg(unix)]
fn reconcile_latest_publication_record(
    record: &LatestPublicationRecord,
    validate_persisted_identity: bool,
) -> Result<()> {
    let index = crate::storage::RunIndex::load_strict()?;
    reconcile_latest_publication_record_with_index(record, validate_persisted_identity, &index)
}

#[cfg(unix)]
fn reconcile_latest_publication_record_with_index(
    record: &LatestPublicationRecord,
    validate_persisted_identity: bool,
    index: &crate::storage::RunIndex,
) -> Result<()> {
    if record.schema != 1 {
        anyhow::bail!(
            "Unsupported latest publication transaction schema {}",
            record.schema
        );
    }
    let out_dir = restore_native_path(record.out_dir.clone());
    if validate_persisted_identity {
        validate_recovery_pack_identity(&out_dir)?;
    }
    let Some(parent) = out_dir.parent() else {
        return Ok(());
    };
    let indexed_target = index
        .entries()
        .iter()
        .rev()
        .filter(|entry| entry.path.parent() == Some(parent))
        .find_map(|entry| {
            // A syntactically valid index row is not current filesystem truth:
            // retention, an operator, or a crash can leave a stale path, and a
            // same-parent directory can be substituted independently. Recovery
            // may advertise only a live owned pack whose RUN identity agrees.
            if validate_recovery_pack_identity(&entry.path).is_ok() {
                entry.path.file_name().map(std::ffi::OsStr::to_owned)
            } else {
                None
            }
        });

    if let Some(target) = indexed_target {
        replace_latest_target(parent, &target)
    } else {
        let previous = record
            .previous_target
            .clone()
            .map(restore_native_path)
            .map(PathBuf::into_os_string);
        if let Some(previous) = previous.as_deref() {
            validate_latest_target_name(previous)?;
        }
        let Some(our_target) = out_dir.file_name() else {
            return Ok(());
        };
        restore_latest_target_unlocked(parent, our_target, previous.as_deref())
    }
}

/// Publish this run's alias after first durably recording how to recover it.
#[cfg(unix)]
pub(crate) fn begin_latest_publication(
    publication: &crate::storage::RunPublicationLock,
    out_dir: &Path,
) -> Result<LatestPublication> {
    recover_latest_publication(publication)?;
    let (Some(parent), Some(target)) = (out_dir.parent(), out_dir.file_name()) else {
        return Ok(LatestPublication {
            record: LatestPublicationRecord {
                schema: 1,
                out_dir: capture_native_path(out_dir.as_os_str()),
                previous_target: None,
            },
        });
    };
    let previous_target = match fs::read_link(parent.join("latest")) {
        Ok(previous) => {
            validate_latest_target_name(previous.as_os_str())?;
            Some(capture_native_path(previous.as_os_str()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let record = LatestPublicationRecord {
        schema: 1,
        out_dir: capture_native_path(out_dir.as_os_str()),
        previous_target,
    };
    write_latest_publication_record(&record)?;
    // Keep the journal on every alias-publication error. The rename may already
    // have succeeded and only its parent fsync failed; clearing intent in that
    // state would make a power-loss rollback unrecoverable. A later publisher
    // reconciles both pre-rename and post-rename failures idempotently.
    replace_latest_target(parent, target)?;
    Ok(LatestPublication { record })
}

#[cfg(not(unix))]
pub(crate) fn begin_latest_publication(
    _publication: &crate::storage::RunPublicationLock,
    _out_dir: &Path,
) -> Result<LatestPublication> {
    Ok(LatestPublication)
}

/// Complete a successful publication. Failure to remove the journal is safe:
/// the next publisher will idempotently reconcile it to the committed index.
#[cfg(unix)]
pub(crate) fn finish_latest_publication(_transaction: &LatestPublication) -> Result<()> {
    clear_latest_publication_record()
}

#[cfg(not(unix))]
pub(crate) fn finish_latest_publication(_transaction: &LatestPublication) -> Result<()> {
    Ok(())
}

/// Resolve an aborted or failed publication from the index while the shared
/// lock is still held, then clear its durable intent.
#[cfg(unix)]
pub(crate) fn rollback_latest_publication(transaction: &LatestPublication) -> Result<()> {
    // This record was built in this process from the live output path. Pack
    // identity validation is for a journal read back after a crash; applying it
    // here can prevent the cancellation rollback it is meant to protect.
    reconcile_latest_publication_record(&transaction.record, false)?;
    clear_latest_publication_record()
}

#[cfg(not(unix))]
pub(crate) fn rollback_latest_publication(_transaction: &LatestPublication) -> Result<()> {
    Ok(())
}

/// Serialize standalone alias helpers. Production publication holds the
/// stronger global RunPublicationLock and performs cancellation rollback as a
/// short uninterruptible consistency operation.
#[cfg(all(test, unix))]
fn with_latest_lock<T>(parent: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = parent.join(".latest.lock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match crate::storage::acquire_lock_at(&lock_path) {
            Ok(_guard) => return operation(),
            Err(error)
                if error
                    .to_string()
                    .starts_with("Index lock held by another live process")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Replace `latest` with one atomic rename, so readers never observe the gap
/// produced by remove-then-symlink and concurrent publishers have a total order.
#[cfg(unix)]
fn replace_latest_target(parent: &Path, target: &std::ffi::OsStr) -> Result<()> {
    validate_latest_target_name(target)?;
    let latest_link = parent.join("latest");
    match fs::symlink_metadata(&latest_link) {
        Ok(metadata) if metadata.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!(
            "Refusing to replace non-symlink latest entry {}",
            latest_link.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staged = parent.join(format!(".latest.{}.{nonce}", std::process::id()));
    std::os::unix::fs::symlink(target, &staged)?;
    if let Err(error) = fs::rename(&staged, &latest_link) {
        let _ = fs::remove_file(&staged);
        return Err(error.into());
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("Failed to sync latest directory {}", parent.display()))?;
    Ok(())
}

/// Generate changed-tests.txt listing test files touched by this diff
pub(super) fn generate_changed_tests(diffs: &[Diff], dir: &Path) -> Result<()> {
    let mut test_files: Vec<String> = Vec::new();

    let ignored_extensions = ["json", "md", "txt", "yaml", "yml", "snap"];

    for diff in diffs {
        for file in &diff.files {
            let safe_path = crate::paths::validate_repo_relative_str(&file.path)?;
            let ext = safe_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ignored_extensions.contains(&ext) {
                continue;
            }
            if is_test_file(&file.path) {
                test_files.push(file.path.clone());
            }
        }
    }

    test_files.sort();
    test_files.dedup();

    let content = if test_files.is_empty() {
        "(no test files changed)\n".to_string()
    } else {
        format!(
            "# Changed test files: {}\n\n{}\n",
            test_files.len(),
            test_files.join("\n")
        )
    };

    fs::write(dir.join("changed-tests.txt"), content)?;
    Ok(())
}

/// Parse a patch string and return per-file diff stats
pub(super) fn compute_diff_stat(patch: &str) -> Vec<(String, usize, usize)> {
    let mut stats: Vec<(String, usize, usize)> = Vec::new();
    let mut current_file: Option<String> = None;
    let mut adds = 0usize;
    let mut dels = 0usize;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            // Flush previous file
            if let Some(file) = current_file.take() {
                stats.push((file, adds, dels));
            }
            // Parse "diff --git a/FILE b/FILE" — take the b/ part
            if let Some(b_part) = rest.split(" b/").nth(1) {
                current_file = Some(b_part.to_string());
            }
            adds = 0;
            dels = 0;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            adds += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            dels += 1;
        }
    }
    if let Some(file) = current_file {
        stats.push((file, adds, dels));
    }
    stats
}

/// Format a diff-stat block as header comment lines
pub(super) fn format_diff_stat_header(stats: &[(String, usize, usize)]) -> String {
    if stats.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&format!("# {} files changed\n", stats.len()));
    let max_path = stats.iter().map(|(f, _, _)| f.len()).max().unwrap_or(0);
    for (file, adds, dels) in stats {
        out.push_str(&format!(
            "# {:<width$}  +{:<4} -{}\n",
            file,
            adds,
            dels,
            width = max_path
        ));
    }
    out
}

pub(super) fn sanitize_commit_msg(msg: &str) -> String {
    msg.chars()
        .take(40)
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(super) fn generate_per_commit_diffs(
    repo: &Repository,
    dir: &Path,
    diffs: &[Diff],
    emit_human_stdout: bool,
) -> Result<()> {
    use colored::Colorize;

    let diff = match diffs.first() {
        Some(d) => d,
        None => return Ok(()),
    };

    let commit_count = diff.commits.len();

    if commit_count > MAX_COMMITS_FOR_PER_COMMIT_DIFFS {
        if emit_human_stdout {
            println!(
                "  {} Skipping per-commit diffs (>{} commits), generating top-10 summary",
                "i".blue(),
                MAX_COMMITS_FOR_PER_COMMIT_DIFFS
            );
        }

        // Generate top-10 commits by churn even when full diffs are skipped
        let mut commit_churns: Vec<(&crate::git::CommitInfo, usize)> = diff
            .commits
            .iter()
            .filter(|c| !c.message.starts_with("Merge "))
            .filter_map(|c| {
                let patch = repo.commit_patch(&c.id).ok()?;
                let stats = compute_diff_stat(&patch);
                let churn: usize = stats.iter().map(|s| s.1 + s.2).sum();
                Some((c, churn))
            })
            .collect();
        commit_churns.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        commit_churns.truncate(10);

        let mut summary = format!(
            "# Per-commit diffs skipped: too many commits ({} > {})\n\n\
             ## Top 10 commits by churn\n\n\
             | # | Churn | Commit | Message |\n\
             |---|-------|--------|---------|{}\n",
            commit_count,
            MAX_COMMITS_FOR_PER_COMMIT_DIFFS,
            commit_churns
                .iter()
                .enumerate()
                .map(|(i, (c, churn))| {
                    let msg: String = c.message.chars().take(60).collect();
                    format!("\n| {} | {} | `{}` | {} |", i + 1, churn, c.short_id, msg)
                })
                .collect::<String>()
        );
        summary.push('\n');

        fs::write(dir.join("00-SUMMARY.md"), summary)?;
        return Ok(());
    }

    if emit_human_stdout {
        println!(
            "  {} Generating per-commit diffs ({} commits)",
            "i".blue(),
            commit_count
        );
    }

    let use_batching = commit_count > COMMIT_BATCH_THRESHOLD;

    if use_batching {
        generate_batched_commits(repo, dir, diff, commit_count)?;
    } else {
        generate_individual_commits(repo, dir, diff)?;
    }

    Ok(())
}

/// Generate individual patch files (one per commit, for <= COMMIT_BATCH_THRESHOLD commits)
pub(super) fn generate_individual_commits(
    repo: &Repository,
    dir: &Path,
    diff: &Diff,
) -> Result<()> {
    let commit_count = diff.commits.len();

    for (idx, commit) in diff.commits.iter().enumerate() {
        let patch = repo.commit_patch(&commit.id)?;
        let stats = compute_diff_stat(&patch);
        let safe_msg = sanitize_commit_msg(&commit.message);
        let filename = format!("{:02}-{}-{}.patch", idx + 1, commit.short_id, safe_msg);

        let mut content = String::new();
        content.push_str(&format!("# Commit: {}\n", commit.id));
        content.push_str(&format!("# Author: {} <{}>\n", commit.author, commit.email));
        content.push_str(&format!("# Date:   {}\n", commit.date));
        content.push_str(&format!("# Message: {}\n", commit.message));
        content.push_str("#\n# --- Diff stat ---\n");
        content.push_str(&format_diff_stat_header(&stats));
        content.push_str("# ---\n");
        content.push_str(&patch);
        content.push('\n');

        fs::write(dir.join(&filename), content)?;
    }

    // Summary
    let mut summary = String::new();
    summary.push_str("# Per-Commit Diffs Summary\n");
    summary.push_str(&format!(
        "# Generated: {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    summary.push_str(&format!("# Total commits: {}\n\n", commit_count));
    summary.push_str("## Commits (oldest first):\n\n");
    for (idx, commit) in diff.commits.iter().enumerate() {
        let safe_msg = sanitize_commit_msg(&commit.message);
        summary.push_str(&format!(
            "- `{:02}-{}-{}.patch` | {} | {} | {}\n",
            idx + 1,
            commit.short_id,
            safe_msg,
            commit.date,
            commit.author,
            commit.message
        ));
    }

    fs::write(dir.join("00-SUMMARY.md"), summary)?;
    Ok(())
}

/// Generate batched patch files (groups of COMMIT_BATCH_SIZE, for > COMMIT_BATCH_THRESHOLD commits)
pub(super) fn generate_batched_commits(
    repo: &Repository,
    dir: &Path,
    diff: &Diff,
    commit_count: usize,
) -> Result<()> {
    let batches: Vec<_> = diff.commits.chunks(COMMIT_BATCH_SIZE).collect();
    let mut summary = String::new();
    summary.push_str("# Per-Commit Diffs Summary (Batched)\n");
    summary.push_str(&format!(
        "# Generated: {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    summary.push_str(&format!(
        "# Total commits: {} in {} batches\n\n",
        commit_count,
        batches.len()
    ));

    let mut global_idx = 0usize;

    for (batch_idx, batch) in batches.iter().enumerate() {
        let batch_start = global_idx + 1;
        let batch_end = global_idx + batch.len();
        let batch_filename = format!("batch-{:02}.patch", batch_idx + 1);
        let batch_theme = infer_batch_theme(batch);

        let mut batch_content = String::new();
        let mut all_stats: Vec<(String, usize, usize)> = Vec::new();
        let mut commit_patches: Vec<(String, String)> = Vec::new();

        for commit in *batch {
            let patch = repo.commit_patch(&commit.id)?;
            let stats = compute_diff_stat(&patch);
            for (file, a, d) in &stats {
                if let Some(existing) = all_stats.iter_mut().find(|(f, _, _)| f == file) {
                    existing.1 += a;
                    existing.2 += d;
                } else {
                    all_stats.push((file.clone(), *a, *d));
                }
            }
            commit_patches.push((commit.id.clone(), patch));
        }

        let total_adds: usize = all_stats.iter().map(|(_, a, _)| *a).sum();
        let total_dels: usize = all_stats.iter().map(|(_, _, d)| *d).sum();

        batch_content.push_str(&format!(
            "# Batch {:02}: {} — commits {}-{} of {}\n",
            batch_idx + 1,
            batch_theme,
            batch_start,
            batch_end,
            commit_count
        ));
        batch_content.push_str("# --- Batch diff stat ---\n");
        batch_content.push_str(&format!(
            "# {} files changed, +{} -{}\n",
            all_stats.len(),
            total_adds,
            total_dels
        ));
        batch_content.push_str(&format_diff_stat_header(&all_stats));
        batch_content.push_str("# ---\n\n");

        for (commit, (_, patch)) in batch.iter().zip(commit_patches.iter()) {
            batch_content.push_str(&format!(
                "## Commit: {} -- {}\n",
                commit.short_id, commit.message
            ));
            batch_content.push_str(patch);
            batch_content.push_str("\n\n");
        }

        fs::write(dir.join(&batch_filename), batch_content)?;

        // Add to summary
        summary.push_str(&format!(
            "### Batch {:02} (`{}`): {} — commits {}-{}\n\n",
            batch_idx + 1,
            batch_filename,
            batch_theme,
            batch_start,
            batch_end
        ));
        for commit in *batch {
            summary.push_str(&format!(
                "- {} | {} | {} | {}\n",
                commit.short_id, commit.date, commit.author, commit.message
            ));
        }
        summary.push('\n');

        global_idx = batch_end;
    }

    fs::write(dir.join("00-SUMMARY.md"), summary)?;
    Ok(())
}

pub(super) fn infer_batch_theme(batch: &[crate::git::CommitInfo]) -> String {
    const THEMES: &[(&str, &[&str])] = &[
        (
            "search infrastructure",
            &[
                "search", "query", "bm25", "hybrid", "retriev", "index", "ranking", "rank",
                "vector",
            ],
        ),
        (
            "storage and persistence",
            &[
                "storage", "db", "database", "sqlite", "cache", "persist", "store", "lance",
            ],
        ),
        (
            "api and runtime",
            &[
                "api", "server", "http", "grpc", "rpc", "runtime", "service", "resolver",
            ],
        ),
        (
            "artifacts and review flow",
            &[
                "artifact",
                "report",
                "review",
                "sarif",
                "dashboard",
                "merge",
                "gate",
                "signal",
            ],
        ),
        (
            "tests and validation",
            &[
                "test",
                "e2e",
                "integration",
                "fixture",
                "contract",
                "validate",
                "qa",
            ],
        ),
        (
            "dependencies and security",
            &[
                "dependency",
                "dependencies",
                "dep ",
                "upgrade",
                "bump",
                "rustsec",
                "audit",
                "security",
            ],
        ),
        (
            "docs and tooling",
            &[
                "docs", "readme", "ci", "clippy", "fmt", "tool", "hook", "build",
            ],
        ),
        (
            "ui and tui polish",
            &["ui", "tui", "panel", "layout", "view"],
        ),
    ];

    let messages: Vec<String> = batch
        .iter()
        .map(|commit| commit.message.to_ascii_lowercase())
        .collect();

    let mut best_label = "mixed changes";
    let mut best_score = 0usize;
    for (label, keywords) in THEMES {
        let score = messages
            .iter()
            .map(|message| {
                keywords
                    .iter()
                    .filter(|keyword| message.contains(**keyword))
                    .count()
            })
            .sum::<usize>();
        if score > best_score {
            best_score = score;
            best_label = label;
        }
    }

    if best_score == 0
        && batch
            .iter()
            .all(|commit| commit.message.to_ascii_lowercase().contains("test"))
    {
        return "tests and validation".to_string();
    }

    best_label.to_string()
}

/// Generate full.patch and return the raw patch texts per diff (for reuse by breaking_changes).
pub(super) fn generate_full_patch(
    dir: &Path,
    repo: &Repository,
    diffs: &[Diff],
) -> Result<Vec<String>> {
    let mut content = String::new();
    let mut patch_texts = Vec::with_capacity(diffs.len());

    for diff in diffs {
        content.push_str(&format!(
            "# Diff: {} vs {}\n# Files: {} | +{} -{}\n\n",
            diff.base,
            diff.target,
            diff.stats.files_changed,
            diff.stats.additions,
            diff.stats.deletions
        ));

        match repo.full_diff(&diff.base_commit_id, &diff.target_commit_id) {
            Ok(patch) => {
                content.push_str(&patch);
                content.push('\n');
                patch_texts.push(patch);
            }
            Err(e) => {
                content.push_str(&format!("# Error generating diff: {}\n\n", e));
                patch_texts.push(String::new());
            }
        }
    }

    fs::write(dir.join("full.patch"), content)?;
    Ok(patch_texts)
}

#[cfg(all(test, unix))]
mod latest_tests {
    use super::*;

    fn run_entry(path: &Path, id: &str) -> crate::storage::RunEntry {
        crate::storage::RunEntry {
            id: id.to_owned(),
            repo: "repo".to_owned(),
            branch: "main".to_owned(),
            commit: id.to_owned(),
            path: path.to_path_buf(),
            created_at: format!("2026-08-31T00:00:0{id}Z"),
            quality_pass: true,
            merge_status: "ALLOW".to_owned(),
            policy_mode: "warn".to_owned(),
            checks_passed: 1,
            checks_failed: 0,
            files_changed: 1,
            size_bytes: 1,
            has_dashboard: false,
        }
    }

    fn write_pack_identity(path: &Path) {
        fs::create_dir_all(path.join("00_summary")).unwrap();
        fs::write(
            path.join("00_summary/RUN.json"),
            serde_json::json!({"artifacts_root": path}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn latest_publication_refuses_to_overwrite_a_regular_file() {
        let root = tempfile::tempdir().unwrap();
        let latest = root.path().join("latest");
        fs::write(&latest, "operator-owned").unwrap();

        let error = replace_latest_target(root.path(), std::ffi::OsStr::new("run"))
            .expect_err("a regular latest file is outside the symlink protocol");

        assert!(error.to_string().contains("non-symlink"));
        assert_eq!(fs::read_to_string(latest).unwrap(), "operator-owned");
    }

    #[test]
    fn recovery_refuses_a_journal_whose_pack_identity_disagrees() {
        let root = tempfile::tempdir().unwrap();
        let out_dir = root.path().join("run");
        fs::create_dir_all(out_dir.join("00_summary")).unwrap();
        fs::write(
            out_dir.join("00_summary/RUN.json"),
            serde_json::json!({"artifacts_root": root.path().join("different")}).to_string(),
        )
        .unwrap();
        fs::write(root.path().join("latest"), "operator-owned").unwrap();
        let record = LatestPublicationRecord {
            schema: 1,
            out_dir: capture_native_path(out_dir.as_os_str()),
            previous_target: None,
        };

        let error = reconcile_latest_publication_record(&record, true)
            .expect_err("tampered journal identity must fail closed");

        assert!(error.to_string().contains("does not match pack identity"));
        assert_eq!(
            fs::read_to_string(root.path().join("latest")).unwrap(),
            "operator-owned"
        );
    }

    #[test]
    fn recovery_quarantines_a_missing_pack_journal_and_allows_the_next_publication() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let branch = home.path().join("runs/repo/main");
        let first = branch.join("first");
        write_pack_identity(&first);
        symlink("first", branch.join("latest")).unwrap();

        let missing = branch.join("missing");
        let stale = LatestPublicationRecord {
            schema: 1,
            out_dir: capture_native_path(missing.as_os_str()),
            previous_target: Some(capture_native_path(std::ffi::OsStr::new("first"))),
        };
        write_latest_publication_record(&stale).unwrap();

        let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
        recover_latest_publication(&publication).unwrap();
        assert_eq!(
            fs::read_link(branch.join("latest")).unwrap(),
            PathBuf::from("first")
        );
        assert!(!latest_publication_record_path().exists());
        assert!(fs::read_dir(home.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("publication-transaction.invalid")
        }));

        let next = branch.join("next");
        write_pack_identity(&next);
        let transaction = begin_latest_publication(&publication, &next)
            .expect("quarantined stale state must not deny a new publication");
        assert_eq!(
            fs::read_link(branch.join("latest")).unwrap(),
            PathBuf::from("next")
        );
        finish_latest_publication(&transaction).unwrap();
    }

    #[test]
    fn recovery_preserves_its_journal_when_the_index_is_corrupt() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let branch = home.path().join("runs/repo/main");
        let predecessor = branch.join("predecessor");
        let interrupted = branch.join("interrupted");
        write_pack_identity(&predecessor);
        write_pack_identity(&interrupted);
        symlink("predecessor", branch.join("latest")).unwrap();

        let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
        let _transaction = begin_latest_publication(&publication, &interrupted).unwrap();
        fs::write(home.path().join("index.jsonl"), b"not-json\n").unwrap();

        let error = recover_latest_publication(&publication)
            .expect_err("recovery must stop when the durable ledger is unreadable");

        assert!(error.to_string().contains("Invalid run index JSON"));
        assert!(latest_publication_record_path().is_file());
        assert_eq!(
            fs::read_link(branch.join("latest")).unwrap(),
            PathBuf::from("interrupted"),
            "recovery must not guess an older target from an unreadable ledger"
        );
        assert!(
            !fs::read_dir(home.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("publication-transaction.invalid")
            }),
            "a valid journal must not be quarantined because its index is corrupt"
        );
    }

    #[test]
    fn unconfirmed_index_rollback_keeps_journal_for_next_owner_recovery() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let branch = home.path().join("runs/repo/main");
        let first = branch.join("first");
        let second = branch.join("second");
        write_pack_identity(&first);
        write_pack_identity(&second);

        let publication = crate::storage::acquire_publication_lock(|| false).unwrap();
        let first_transaction = begin_latest_publication(&publication, &first).unwrap();
        crate::storage::register_and_prune_locked(
            &publication,
            &first,
            run_entry(&first, "1"),
            false,
            || false,
        )
        .unwrap();
        finish_latest_publication(&first_transaction).unwrap();

        let _second_transaction = begin_latest_publication(&publication, &second).unwrap();
        let error = crate::storage::register_and_prune_locked(
            &publication,
            &second,
            run_entry(&second, "2"),
            false,
            || {
                let new_index_is_visible = crate::storage::RunIndex::load()
                    .latest("repo", "main")
                    .is_some_and(|entry| entry.id == "2");
                if new_index_is_visible {
                    crate::storage::arm_test_index_save_failure();
                    true
                } else {
                    false
                }
            },
        )
        .expect_err("the previous index restore is injected to fail");
        assert!(crate::storage::is_unconfirmed_publication_rollback(&error));
        assert!(latest_publication_record_path().is_file());
        drop(publication);

        let next_owner = crate::storage::acquire_publication_lock(|| false).unwrap();
        recover_latest_publication(&next_owner).unwrap();
        assert_eq!(
            fs::read_link(branch.join("latest")).unwrap(),
            PathBuf::from("second")
        );
        assert_eq!(
            crate::storage::RunIndex::load()
                .latest("repo", "main")
                .unwrap()
                .id,
            "2"
        );
        assert!(!latest_publication_record_path().exists());
    }

    #[test]
    fn publication_journal_ignores_a_predictable_temp_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let protected = root.path().join("protected.txt");
        fs::write(&protected, "do-not-touch").unwrap();
        symlink(
            &protected,
            root.path().join("publication-transaction.json.tmp"),
        )
        .unwrap();
        let destination = root.path().join("publication-transaction.json");
        let record = LatestPublicationRecord {
            schema: 1,
            out_dir: capture_native_path(root.path().join("run").as_os_str()),
            previous_target: None,
        };

        write_latest_publication_record_at(&destination, &record).unwrap();

        assert_eq!(fs::read_to_string(&protected).unwrap(), "do-not-touch");
        let written: LatestPublicationRecord =
            serde_json::from_slice(&fs::read(destination).unwrap()).unwrap();
        assert_eq!(written.schema, 1);
    }

    #[test]
    fn rollback_does_not_overwrite_a_newer_latest_publication() {
        let root = tempfile::tempdir().unwrap();
        let predecessor = root.path().join("predecessor");
        let cancelled = root.path().join("cancelled");
        let newer = root.path().join("newer");
        for path in [&predecessor, &cancelled, &newer] {
            fs::create_dir(path).unwrap();
        }

        create_latest_symlink(&predecessor).unwrap();
        let previous = create_latest_symlink(&cancelled).unwrap().unwrap();
        create_latest_symlink(&newer).unwrap();

        restore_latest_symlink(&cancelled, Some(previous.as_os_str())).unwrap();

        assert_eq!(
            fs::read_link(root.path().join("latest")).unwrap(),
            PathBuf::from("newer"),
            "a cancelled older publisher must not roll back a newer run"
        );
    }

    #[test]
    fn rollback_restores_predecessor_only_while_latest_is_owned() {
        let root = tempfile::tempdir().unwrap();
        let predecessor = root.path().join("predecessor");
        let cancelled = root.path().join("cancelled");
        fs::create_dir(&predecessor).unwrap();
        fs::create_dir(&cancelled).unwrap();

        create_latest_symlink(&predecessor).unwrap();
        let previous = create_latest_symlink(&cancelled).unwrap().unwrap();
        restore_latest_symlink(&cancelled, Some(previous.as_os_str())).unwrap();

        assert_eq!(
            fs::read_link(root.path().join("latest")).unwrap(),
            PathBuf::from("predecessor")
        );
    }

    #[test]
    fn rollback_uses_the_immediate_serialized_predecessor() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let completed = root.path().join("completed");
        let cancelled = root.path().join("cancelled");
        for path in [&original, &completed, &cancelled] {
            fs::create_dir(path).unwrap();
        }

        create_latest_symlink(&original).unwrap();
        create_latest_symlink(&completed).unwrap();
        let previous = create_latest_symlink(&cancelled).unwrap().unwrap();
        restore_latest_symlink(&cancelled, Some(previous.as_os_str())).unwrap();

        assert_eq!(
            fs::read_link(root.path().join("latest")).unwrap(),
            PathBuf::from("completed"),
            "a cancelled publisher restores the run immediately before it, not an older peek"
        );
    }

    #[tokio::test]
    async fn cancelled_rollback_is_immediate_and_restores_the_predecessor() {
        let root = tempfile::tempdir().unwrap();
        let predecessor = root.path().join("predecessor");
        let cancelled = root.path().join("cancelled");
        fs::create_dir(&predecessor).unwrap();
        fs::create_dir(&cancelled).unwrap();
        create_latest_symlink(&predecessor).unwrap();
        let previous = create_latest_symlink(&cancelled).unwrap().unwrap();
        let governor = std::sync::Arc::new(crate::governor::ResourceGovernor::new());
        governor.cancel();

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::governor::with_run_scope(std::sync::Arc::clone(&governor), async {
                restore_latest_symlink(&cancelled, Some(previous.as_os_str()))
            }),
        )
        .await
        .expect("consistency rollback must not wait on the cancelled work queue")
        .expect("consistency rollback ignores the already-cancelled governor");
        assert_eq!(
            fs::read_link(root.path().join("latest")).unwrap(),
            PathBuf::from("predecessor")
        );
    }
}
