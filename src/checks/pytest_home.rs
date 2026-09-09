//! A fresh HOME/XDG view for tests running in a reviewed snapshot.
//!
//! Load the environment inside pytest, before project plugins and conftests.
//! Changing HOME on the uv/pytest launcher would also change tool discovery,
//! dependency setup, and user-installed pytest's own import path.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

const PLUGIN: &str = r#"import json
import os
import site
from pathlib import Path

# Keep Python's existing user package location available to subprocesses,
# including xdist bootstraps, after changing HOME for application data.
_user_base = site.getuserbase()
if _user_base:
    os.environ.setdefault("PYTHONUSERBASE", _user_base)
with Path(__file__).with_suffix(".json").open(encoding="utf-8") as _stream:
    os.environ.update(json.load(_stream))

def pytest_report_header():
    return "prview: snapshot-local HOME/XDG"
"#;

pub(super) struct SnapshotPytestHome {
    // Retain both the environment and its plugin until pytest's children exit.
    _dir: tempfile::TempDir,
    module_name: String,
    python_path: String,
}

impl SnapshotPytestHome {
    pub(super) fn for_run(
        repo_root: &Path,
        scan_dir: &Path,
        inherited_python_path: Option<OsString>,
    ) -> Result<Option<Self>> {
        if scan_dir == repo_root {
            return Ok(None);
        }

        let dir = tempfile::Builder::new()
            .prefix("prview-pytest-home-")
            .tempdir()
            .context("cannot create snapshot pytest home")?;
        let home = dir.path().join("home");
        let plugin_dir = dir.path().join("plugin");
        std::fs::create_dir_all(&plugin_dir)?;
        let mut environment = BTreeMap::new();
        for (key, suffix) in [
            ("HOME", ""),
            ("USERPROFILE", ""),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("APPDATA", "config"),
            ("LOCALAPPDATA", "cache"),
        ] {
            let path = home.join(suffix);
            std::fs::create_dir_all(&path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
            }
            environment.insert(
                key,
                path.to_str()
                    .context("snapshot pytest home is not valid UTF-8")?
                    .to_string(),
            );
        }
        let unique_name = dir
            .path()
            .file_name()
            .context("snapshot pytest home has no directory name")?
            .to_string_lossy()
            .replace('-', "_");
        let module_name = format!("_{unique_name}");
        std::fs::write(plugin_dir.join(format!("{module_name}.py")), PLUGIN)?;
        std::fs::write(
            plugin_dir.join(format!("{module_name}.json")),
            serde_json::to_vec(&environment)?,
        )?;
        let mut python_paths = vec![plugin_dir];
        if let Some(inherited) = inherited_python_path {
            python_paths.extend(std::env::split_paths(&inherited));
        }
        let python_path = std::env::join_paths(python_paths)
            .context("cannot prepend the snapshot pytest plugin to PYTHONPATH")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("snapshot pytest PYTHONPATH is not valid UTF-8"))?;
        Ok(Some(Self {
            _dir: dir,
            module_name,
            python_path,
        }))
    }

    pub(super) fn module_name(&self) -> &str {
        &self.module_name
    }

    pub(super) fn child_env(&self, base: &[(String, String)]) -> Vec<(String, String)> {
        let mut env = base.to_vec();
        env.retain(|(key, _)| key != "PYTHONPATH");
        env.push(("PYTHONPATH".to_string(), self.python_path.clone()));
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(inherited: Option<OsString>) -> SnapshotPytestHome {
        SnapshotPytestHome::for_run(Path::new("checkout"), Path::new("snapshot"), inherited)
            .expect("home plan")
            .expect("off-checkout isolation")
    }

    #[test]
    fn local_checkout_keeps_its_environment() {
        assert!(
            SnapshotPytestHome::for_run(Path::new("repo"), Path::new("repo"), None)
                .expect("local plan")
                .is_none()
        );
    }

    #[test]
    fn every_home_selector_points_to_a_fresh_owned_directory() {
        let home = home(None);
        let payload = home
            ._dir
            .path()
            .join("plugin")
            .join(format!("{}.json", home.module_name));
        let environment: BTreeMap<String, String> =
            serde_json::from_slice(&std::fs::read(payload).unwrap()).unwrap();
        assert_eq!(environment.len(), 9);
        for path in environment.values() {
            let path = Path::new(path);
            assert!(path.starts_with(home._dir.path().join("home")));
            assert!(path.is_dir());
            assert!(!path.join(".codex").exists());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
            }
        }
    }

    #[test]
    fn python_path_retains_operator_imports_and_worker_caps() {
        let inherited = std::env::join_paths([Path::new("first"), Path::new("second")]).unwrap();
        let home = home(Some(inherited));
        let env = home.child_env(&[
            ("CARGO_BUILD_JOBS".to_string(), "2".to_string()),
            ("PYTHONPATH".to_string(), "superseded".to_string()),
        ]);
        assert_eq!(env.len(), 2);
        assert_eq!(env[0], ("CARGO_BUILD_JOBS".to_string(), "2".to_string()));
        let paths: Vec<_> = std::env::split_paths(&env[1].1).collect();
        assert_eq!(paths[0], home._dir.path().join("plugin"));
        assert_eq!(paths[1], Path::new("first"));
        assert_eq!(paths[2], Path::new("second"));
        assert!(env.iter().all(|(key, _)| key != "HOME"));
    }

    #[test]
    fn concurrent_runs_do_not_share_home_or_plugin_identity() {
        let first = home(None);
        let second = home(None);
        assert_ne!(first._dir.path(), second._dir.path());
        assert_ne!(first.module_name, second.module_name);
    }

    #[test]
    fn dropping_the_plan_cleans_up_home_and_plugin_together() {
        let home = home(None);
        let path = home._dir.path().to_path_buf();
        assert!(path.exists());
        drop(home);
        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_python_path_is_rejected_only_for_snapshots() {
        use std::os::unix::ffi::OsStringExt;
        let inherited = OsString::from_vec(vec![0xff]);
        assert!(
            SnapshotPytestHome::for_run(
                Path::new("checkout"),
                Path::new("snapshot"),
                Some(inherited.clone()),
            )
            .is_err()
        );
        assert!(
            SnapshotPytestHome::for_run(Path::new("repo"), Path::new("repo"), Some(inherited))
                .unwrap()
                .is_none()
        );
    }
}
