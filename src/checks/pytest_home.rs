//! A fresh HOME/XDG view for tests running in a reviewed snapshot.
//!
//! Bootstrap pytest without project configuration, then run the original test
//! arguments in the new environment. A project can preload plugins in addopts,
//! before any plugin added to the regular command line.
//! Changing HOME on the uv/pytest launcher would also change tool discovery,
//! dependency setup, and user-installed pytest's own import path.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

const PLUGIN: &str = r#"import json
import os
import site
import pytest
from pathlib import Path

# Keep Python's existing user package location available to subprocesses,
# including xdist bootstraps, after changing HOME for application data.
_user_base = site.getuserbase()
if _user_base:
    os.environ.setdefault("PYTHONUSERBASE", _user_base)
with Path(__file__).with_suffix(".json").open(encoding="utf-8") as _stream:
    _settings = json.load(_stream)
os.environ.update(_settings["home"])

def pytest_addoption(parser):
    parser.addoption("--prview-snapshot-home", action="store_true",
                     help="internal PrView snapshot environment bootstrap")

@pytest.hookimpl(tryfirst=True)
def pytest_cmdline_main(config):
    # invocation_params holds the original argv, before ini/environment addopts.
    # The inner run has no bootstrap flag, so it uses pytest's normal main hook.
    args = list(config.invocation_params.args)
    if "--prview-snapshot-home" not in args:
        return None
    if "--" not in args:
        raise pytest.UsageError("PrView snapshot bootstrap requires test arguments after --")
    for key, value in _settings["pytest_env"].items():
        if value is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = value
    return pytest.main(args[args.index("--") + 1:], plugins=[__name__])

def pytest_report_header():
    return "prview: snapshot-local HOME/XDG"
"#;

pub(super) struct SnapshotPytestHome {
    // Retain both the environment and its plugin through the check execution.
    _dir: tempfile::TempDir,
    module_name: String,
    python_path: String,
}

impl SnapshotPytestHome {
    pub(super) fn for_run(
        repo_root: &Path,
        scan_dir: &Path,
        mut inherited: impl FnMut(&str) -> Option<OsString>,
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
        let mut pytest_env = BTreeMap::new();
        for key in [
            "PYTEST_ADDOPTS",
            "PYTEST_PLUGINS",
            "PYTEST_DISABLE_PLUGIN_AUTOLOAD",
        ] {
            let value = inherited(key)
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| anyhow::anyhow!("snapshot pytest {key} is not valid UTF-8"))
                })
                .transpose()?;
            pytest_env.insert(key, value);
        }
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
            serde_json::to_vec(&serde_json::json!({
                "home": environment,
                "pytest_env": pytest_env,
            }))?,
        )?;
        let mut python_paths = vec![plugin_dir];
        if let Some(inherited) = inherited("PYTHONPATH") {
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
        env.retain(|(key, _)| {
            !matches!(
                key.as_str(),
                "PYTHONPATH"
                    | "PYTEST_ADDOPTS"
                    | "PYTEST_PLUGINS"
                    | "PYTEST_DISABLE_PLUGIN_AUTOLOAD"
            )
        });
        env.push(("PYTHONPATH".to_string(), self.python_path.clone()));
        env.extend([
            ("PYTEST_ADDOPTS".to_string(), String::new()),
            ("PYTEST_PLUGINS".to_string(), String::new()),
            (
                "PYTEST_DISABLE_PLUGIN_AUTOLOAD".to_string(),
                "1".to_string(),
            ),
        ]);
        env
    }

    pub(super) fn wrap_args(
        &self,
        root: &Path,
        null_config: &Path,
        test_args: Vec<String>,
    ) -> Vec<String> {
        let mut args = vec![
            "-p".to_string(),
            self.module_name.clone(),
            "-c".to_string(),
            null_config.display().to_string(),
            "--rootdir".to_string(),
            root.display().to_string(),
            "--noconftest".to_string(),
            "--prview-snapshot-home".to_string(),
            "--".to_string(),
        ];
        args.extend(test_args);
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(inherited: Option<OsString>) -> SnapshotPytestHome {
        SnapshotPytestHome::for_run(Path::new("checkout"), Path::new("snapshot"), |key| {
            if key == "PYTHONPATH" {
                inherited.clone()
            } else {
                None
            }
        })
        .expect("home plan")
        .expect("off-checkout isolation")
    }

    #[test]
    fn local_checkout_keeps_its_environment() {
        assert!(
            SnapshotPytestHome::for_run(Path::new("repo"), Path::new("repo"), |_| None)
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
        let payload: serde_json::Value =
            serde_json::from_slice(&std::fs::read(payload).unwrap()).unwrap();
        let environment: BTreeMap<String, String> =
            serde_json::from_value(payload["home"].clone()).unwrap();
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
        assert_eq!(env.len(), 5);
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
            SnapshotPytestHome::for_run(Path::new("checkout"), Path::new("snapshot"), |key| {
                if key == "PYTHONPATH" {
                    Some(inherited.clone())
                } else {
                    None
                }
            },)
            .is_err()
        );
        assert!(
            SnapshotPytestHome::for_run(Path::new("repo"), Path::new("repo"), |_| {
                Some(inherited.clone())
            })
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn bootstrap_defers_original_addopts_and_plugin_settings_to_the_inner_run() {
        let home =
            SnapshotPytestHome::for_run(Path::new("checkout"), Path::new("snapshot"), |key| {
                match key {
                    "PYTEST_ADDOPTS" => Some(OsString::from("-p early_plugin -o addopts=-n0")),
                    "PYTEST_PLUGINS" => Some(OsString::from("project_plugin")),
                    _ => None,
                }
            })
            .unwrap()
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                home._dir
                    .path()
                    .join("plugin")
                    .join(format!("{}.json", home.module_name)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            payload["pytest_env"]["PYTEST_ADDOPTS"],
            "-p early_plugin -o addopts=-n0"
        );
        assert_eq!(payload["pytest_env"]["PYTEST_PLUGINS"], "project_plugin");
        assert!(payload["pytest_env"]["PYTEST_DISABLE_PLUGIN_AUTOLOAD"].is_null());
        let env: BTreeMap<_, _> = home.child_env(&[]).into_iter().collect();
        assert_eq!(env["PYTEST_ADDOPTS"], "");
        assert_eq!(env["PYTEST_PLUGINS"], "");
        assert_eq!(env["PYTEST_DISABLE_PLUGIN_AUTOLOAD"], "1");
        let original = vec!["-v".to_string(), "-n".to_string(), "1".to_string()];
        let args = home.wrap_args(
            Path::new("snapshot"),
            Path::new("null-config"),
            original.clone(),
        );
        let separator = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(args[separator + 1..], original);
        assert!(args[..separator].contains(&"--noconftest".to_string()));
        assert!(args[..separator].contains(&"null-config".to_string()));
    }

    #[tokio::test]
    async fn real_pytest_loads_all_project_plugin_routes_after_home_isolation() {
        let Ok(pytest) = which::which("pytest") else {
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let scan = root.path().join("snapshot");
        std::fs::create_dir(&scan).unwrap();
        let home =
            SnapshotPytestHome::for_run(&root.path().join("checkout"), &scan, |key| match key {
                "PYTHONPATH" => Some(scan.as_os_str().to_os_string()),
                "PYTEST_ADDOPTS" => Some(OsString::from("-p addopts_home_probe")),
                "PYTEST_PLUGINS" => Some(OsString::from("environment_home_probe")),
                "PYTEST_DISABLE_PLUGIN_AUTOLOAD" => Some(OsString::from("1")),
                _ => None,
            })
            .unwrap()
            .unwrap();
        let expected_home = serde_json::to_string(&home._dir.path().join("home")).unwrap();
        for name in [
            "ini_home_probe",
            "addopts_home_probe",
            "environment_home_probe",
        ] {
            std::fs::write(
                scan.join(format!("{name}.py")),
                format!(
                    "from pathlib import Path\n\
                     assert Path.home().resolve() == Path({expected_home}).resolve(), \
                     'project plugin read operator HOME'\n\
                     Path(__file__).with_suffix('.loaded').write_text('loaded')\n"
                ),
            )
            .unwrap();
        }
        std::fs::write(
            scan.join("pytest.ini"),
            "[pytest]\naddopts = -p ini_home_probe\n",
        )
        .unwrap();
        std::fs::write(
            scan.join("test_home.py"),
            "def test_home():\n    assert True\n",
        )
        .unwrap();
        let null_config = root.path().join("empty.ini");
        std::fs::write(&null_config, "").unwrap();
        let args = home.wrap_args(&scan, &null_config, vec!["-v".to_string()]);
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        let output = crate::checks::run_command_with_timeout_and_env(
            pytest.to_str().unwrap(),
            &args,
            &scan,
            60,
            &home.child_env(&[]),
        )
        .await
        .expect("pytest bootstrap");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{stdout}\n{stderr}");
        assert!(stdout.contains("prview: snapshot-local HOME/XDG"));
        for name in [
            "ini_home_probe",
            "addopts_home_probe",
            "environment_home_probe",
        ] {
            assert!(scan.join(format!("{name}.loaded")).is_file(), "{name}");
        }
    }
}
