//! Policy layer for merge gating and check severity mapping.
//!
//! Loads `.prview-policy.yml` and provides normalized runtime policy.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub mod engine;
pub use engine::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PolicyMode {
    Shadow,
    #[default]
    Warn,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PolicySeverity {
    Block,
    #[default]
    Warn,
    Ignore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyConfig {
    pub version: u32,
    pub mode: PolicyMode,
    pub default_severity: PolicySeverity,
    #[serde(default)]
    pub checks: HashMap<String, PolicySeverity>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            version: 1,
            mode: PolicyMode::Warn,
            default_severity: PolicySeverity::Warn,
            checks: HashMap::new(),
        }
    }
}

impl PolicyConfig {
    pub fn severity_for(&self, check_id: &str) -> PolicySeverity {
        self.checks
            .get(check_id)
            .copied()
            .unwrap_or(self.default_severity)
    }

    pub fn is_blocking(&self, severity: PolicySeverity, class: GateClass) -> bool {
        match self.mode {
            PolicyMode::Shadow => false,
            PolicyMode::Warn => {
                matches!(class, GateClass::Fail) && matches!(severity, PolicySeverity::Block)
            }
            PolicyMode::Block => {
                matches!(class, GateClass::Fail)
                    && matches!(severity, PolicySeverity::Block | PolicySeverity::Warn)
            }
        }
    }

    pub fn mode_str(&self) -> &'static str {
        match self.mode {
            PolicyMode::Shadow => "shadow",
            PolicyMode::Warn => "warn",
            PolicyMode::Block => "block",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateClass {
    Pass,
    Skip,
    Fail,
    Info,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PartialPolicyConfig {
    version: Option<u32>,
    mode: Option<PolicyMode>,
    default_severity: Option<PolicySeverity>,
    checks: Option<HashMap<String, PolicySeverity>>,
}

pub fn resolve_policy_path(repo_root: &Path, requested: Option<&Path>) -> Result<PathBuf> {
    let requested_path = requested.unwrap_or_else(|| Path::new(".prview-policy.yml"));
    ensure_policy_path_shape(requested_path)?;
    crate::paths::resolve_path_within(repo_root, requested_path)
}

fn ensure_policy_path_shape(path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| matches!(ext, "yml" | "yaml"))
        .unwrap_or(false);

    if !ext {
        bail!(
            "Policy file must use .yml or .yaml extension: {}",
            path.display()
        );
    }

    Ok(())
}

pub fn load_policy(path: &Path, mode_override: Option<PolicyMode>) -> Result<PolicyConfig> {
    let mut policy = if path.exists() {
        let parent = path.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "Policy path must include a parent directory: {}",
                path.display()
            )
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            anyhow::anyhow!("Policy path must include a file name: {}", path.display())
        })?;
        let raw = crate::paths::read_to_string_within(parent, Path::new(file_name))?;
        let parsed: PartialPolicyConfig = serde_yaml::from_str(&raw)
            .with_context(|| format!("Failed parsing YAML policy: {}", path.display()))?;

        let mut checks = parsed.checks.unwrap_or_default();
        checks
            .entry("cargo_audit".to_string())
            .or_insert(PolicySeverity::Block);

        PolicyConfig {
            version: parsed.version.unwrap_or(1),
            mode: parsed.mode.unwrap_or_default(),
            default_severity: parsed.default_severity.unwrap_or_default(),
            checks,
        }
    } else {
        PolicyConfig::default()
    };

    if let Some(mode) = mode_override {
        policy.mode = mode;
    }

    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_policy_values() {
        let p = PolicyConfig::default();
        assert_eq!(p.version, 1);
        assert_eq!(p.mode, PolicyMode::Warn);
        assert_eq!(p.default_severity, PolicySeverity::Warn);
    }

    #[test]
    fn severity_resolution_prefers_check_override() {
        let mut p = PolicyConfig::default();
        p.checks
            .insert("cargo_audit".to_string(), PolicySeverity::Block);

        assert_eq!(p.severity_for("cargo_audit"), PolicySeverity::Block);
        assert_eq!(p.severity_for("unknown"), PolicySeverity::Warn);
    }

    #[test]
    fn blocking_rules_shadow_never_blocks() {
        let p = PolicyConfig {
            mode: PolicyMode::Shadow,
            ..Default::default()
        };
        assert!(!p.is_blocking(PolicySeverity::Block, GateClass::Fail));
    }

    #[test]
    fn blocking_rules_warn_blocks_only_block_severity() {
        let p = PolicyConfig {
            mode: PolicyMode::Warn,
            ..Default::default()
        };
        assert!(p.is_blocking(PolicySeverity::Block, GateClass::Fail));
        assert!(!p.is_blocking(PolicySeverity::Warn, GateClass::Fail));
    }

    #[test]
    fn blocking_rules_block_blocks_warn_and_block() {
        let p = PolicyConfig {
            mode: PolicyMode::Block,
            ..Default::default()
        };
        assert!(p.is_blocking(PolicySeverity::Block, GateClass::Fail));
        assert!(p.is_blocking(PolicySeverity::Warn, GateClass::Fail));
        assert!(!p.is_blocking(PolicySeverity::Ignore, GateClass::Fail));
    }

    #[test]
    fn blocking_requires_fail_class() {
        let p = PolicyConfig {
            mode: PolicyMode::Warn,
            ..Default::default()
        };
        assert!(!p.is_blocking(PolicySeverity::Block, GateClass::Info));

        let p = PolicyConfig {
            mode: PolicyMode::Block,
            ..Default::default()
        };
        assert!(!p.is_blocking(PolicySeverity::Warn, GateClass::Info));
    }

    #[test]
    fn resolve_policy_path_defaults_to_repo_policy_file() {
        let repo = tempdir().unwrap();
        let repo_canon = repo.path().canonicalize().unwrap();

        let resolved = resolve_policy_path(repo.path(), None).unwrap();

        assert_eq!(resolved, repo_canon.join(".prview-policy.yml"));
    }

    #[test]
    fn resolve_policy_path_accepts_repo_local_relative_file() {
        let repo = tempdir().unwrap();
        let policies = repo.path().join("policies");
        std::fs::create_dir_all(&policies).unwrap();
        let policy = policies.join("custom.yml");
        std::fs::write(&policy, "version: 1\n").unwrap();

        let resolved =
            resolve_policy_path(repo.path(), Some(Path::new("policies/custom.yml"))).unwrap();

        assert_eq!(resolved, policy.canonicalize().unwrap());
    }

    #[test]
    fn resolve_policy_path_rejects_parent_traversal_outside_repo() {
        let repo = tempdir().unwrap();

        let err = resolve_policy_path(repo.path(), Some(Path::new("../outside.yml"))).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains("escapes root"));
    }

    #[test]
    fn resolve_policy_path_rejects_non_yaml_extensions() {
        let repo = tempdir().unwrap();

        let err = resolve_policy_path(repo.path(), Some(Path::new("policy.json"))).unwrap_err();
        let msg = err.to_string();

        assert!(msg.contains(".yml or .yaml"));
    }
}
