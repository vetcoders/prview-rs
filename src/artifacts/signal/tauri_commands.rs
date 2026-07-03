use crate::git::{Diff, FileStatus, Repository};
use crate::paths::read_to_string_within;
use anyhow::Result;
use regex::Regex;
use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

pub fn generate_tauri_commands(
    out_dir: &Path,
    diffs: &[Diff],
    repo: &Repository,
    tauri_dir: &Path,
) -> Result<()> {
    let mut head_commands = HashSet::new();
    let cmd_regex = Regex::new(r#"#\[tauri::command\][\s\S]*?(?:pub\s+)?(?:async\s+)?fn\s+(\w+)"#)?;

    // 1. Scan the *current* tree for commands. These are ADDED or UNCHANGED.
    for entry in WalkDir::new(tauri_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "rs").unwrap_or(false))
    {
        if let Ok(rel_path) = entry.path().strip_prefix(tauri_dir)
            && let Ok(content) = read_to_string_within(tauri_dir, rel_path)
        {
            for cap in cmd_regex.captures_iter(&content) {
                if let Some(name) = cap.get(1) {
                    head_commands.insert(format!("{}:{}", rel_path.display(), name.as_str()));
                }
            }
        }
    }

    // 1.5. Clean zombie commands from HEAD if git diff says they are Deleted!
    // This fixes PV-11 where local worktrees have stale files left from other branches
    for diff in diffs {
        for file in &diff.files {
            if file.status == FileStatus::Deleted && file.path.ends_with(".rs") {
                let abs_file_path = repo.path().join(&file.path);
                if let Ok(rel_path) = abs_file_path.strip_prefix(tauri_dir) {
                    let prefix = format!("{}:", rel_path.display());
                    head_commands.retain(|cmd| !cmd.starts_with(&prefix));
                }
            }
        }
    }

    // 2. Adjust base properties using Diffs
    let mut base_commands = head_commands.clone();

    for diff in diffs {
        for file in &diff.files {
            if !file.path.ends_with(".rs") {
                continue;
            }

            let abs_file_path = repo.path().join(&file.path);
            if let Ok(rel_path) = abs_file_path.strip_prefix(tauri_dir) {
                let prefix = format!("{}:", rel_path.display());
                base_commands.retain(|cmd| !cmd.starts_with(&prefix));

                if file.status != FileStatus::Added
                    && let Ok(base_content) = repo.file_at_commit(&diff.base_commit_id, &file.path)
                {
                    for cap in cmd_regex.captures_iter(&base_content) {
                        if let Some(name) = cap.get(1) {
                            base_commands.insert(format!(
                                "{}:{}",
                                rel_path.display(),
                                name.as_str()
                            ));
                        }
                    }
                }
            }
        }
    }

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = Vec::new();

    for cmd in &head_commands {
        if base_commands.contains(cmd) {
            unchanged.push(cmd.clone());
        } else {
            added.push(cmd.clone());
        }
    }

    for cmd in &base_commands {
        if !head_commands.contains(cmd) {
            removed.push(cmd.clone());
        }
    }

    added.sort();
    removed.sort();
    unchanged.sort();

    let mut content = String::new();
    let _ = writeln!(content, "# Tauri Commands\n");
    let target_sha = diffs
        .first()
        .map(|d| crate::git::short_sha(&d.target_commit_id))
        .unwrap_or_else(|| "HEAD");
    let _ = writeln!(content, "Scanned from PR branch @ `{}`\n", target_sha);

    let total = added.len() + removed.len() + unchanged.len();
    let _ = writeln!(
        content,
        "Found {} commands total ({} added, {} removed, {} unchanged):\n",
        total,
        added.len(),
        removed.len(),
        unchanged.len()
    );

    for cmd in &added {
        let _ = writeln!(content, "- [ADDED] {}", cmd);
    }
    for cmd in &removed {
        let _ = writeln!(content, "- [REMOVED] {}", cmd);
    }
    for cmd in &unchanged {
        let _ = writeln!(content, "- [UNCHANGED] {}", cmd);
    }

    fs::write(out_dir.join("tauri-commands.txt"), content)?;
    Ok(())
}
