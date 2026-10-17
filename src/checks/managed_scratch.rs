//! Effective scratch for snapshot-only Cargo/uv commands. Config getters stay
//! configured paths; no old cache is migrated or pruned by relocation.
use crate::Config;
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

pub(super) fn snapshot_root(config: &Config, configured: PathBuf, kind: &str) -> Result<PathBuf> {
    let caller = std::fs::canonicalize(&config.repo_root)?;
    let resolved = super::caller_integrity::planned_path(&configured)?;
    let mut visible = super::caller_integrity::namespace_source_visible(&caller, &configured)?;
    // A descendant link can redirect writes even when the namespace root is
    // external. Inspect links without traversing them or reading file contents.
    if resolved.exists() {
        for entry in walkdir::WalkDir::new(&resolved).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                let target = std::fs::canonicalize(entry.path()).with_context(|| {
                    format!("unresolved managed scratch link {}", entry.path().display())
                })?;
                if target.starts_with(&caller) {
                    visible = true;
                    break;
                }
            }
        }
    }
    if !visible {
        return Ok(configured);
    }
    let namespace = configured
        .file_name()
        .context("managed scratch has no repository namespace")?;
    let effective = std::env::temp_dir()
        .join("prview-managed-scratch")
        .join(kind)
        .join(namespace);
    let effective_resolved = super::caller_integrity::planned_path(&effective)?;
    ensure!(
        !effective_resolved.starts_with(&caller),
        "cannot place {kind} snapshot scratch outside caller: temporary namespace {} is inside {}",
        effective.display(),
        caller.display()
    );
    // Do not reuse a redirected descendant in the chosen effective namespace.
    if effective_resolved.exists() {
        for entry in walkdir::WalkDir::new(&effective_resolved).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                let target = std::fs::canonicalize(entry.path())?;
                ensure!(
                    !target.starts_with(&caller),
                    "effective {kind} scratch redirects into caller at {}",
                    entry.path().display()
                );
            }
        }
    }
    eprintln!(
        "prview: {kind} snapshot scratch relocated from {} to {} because the configured namespace is caller-visible; old scratch retained",
        configured.display(),
        effective.display()
    );
    Ok(effective)
}
