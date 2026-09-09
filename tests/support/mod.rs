//! Environment owned by one binary contract test or live MCP session.

use std::process::Command;
use tempfile::TempDir;

pub struct ContractEnvironment {
    root: TempDir,
}

impl ContractEnvironment {
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("contract environment");
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).expect("contract bin directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let scanner = bin.join("semgrep");
            std::fs::write(
                &scanner,
                "#!/bin/sh\nif [ \"$1\" = --version ]; then\n  printf '%s\\n' '1.0.0-contract-fixture'\nelse\n  printf '%s\\n' '{\"results\":[],\"errors\":[]}'\nfi\n",
            )
            .expect("write contract scanner");
            std::fs::set_permissions(&scanner, std::fs::Permissions::from_mode(0o755))
                .expect("executable contract scanner");
        }
        #[cfg(windows)]
        std::fs::write(
            bin.join("semgrep.cmd"),
            "@echo off\r\nif \"%~1\"==\"--version\" (\r\n  echo 1.0.0-contract-fixture\r\n) else (\r\n  echo {\"results\":[],\"errors\":[]}\r\n)\r\n",
        )
        .expect("write contract scanner");
        Self { root }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_prview"));
        self.configure(&mut command);
        command
    }

    fn configure(&self, command: &mut Command) {
        let mut paths = vec![self.root.path().join("bin")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command
            .env("PATH", std::env::join_paths(paths).expect("contract PATH"))
            .env("PRVIEW_HOME", self.root.path().join("home"));
    }
}

#[cfg(unix)]
#[test]
fn contract_scanner_is_executable_and_environment_is_owned() {
    let environment = ContractEnvironment::new();
    let root = environment.root.path().to_owned();
    let mut scanner = Command::new("semgrep");
    environment.configure(&mut scanner);
    let output = scanner.output().expect("run contract scanner");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value, serde_json::json!({"results": [], "errors": []}));
    let version = scanner.arg("--version").output().expect("scanner version");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "1.0.0-contract-fixture"
    );
    drop(environment);
    assert!(
        !root.exists(),
        "fixture directory must be removed with its owner"
    );
}
