//! Fetch-only snapshot of the invocation repository's transport authority.
//! Values are never written into the owned repository or included in argv.
use anyhow::{Context, Result, ensure};
use std::path::Path;
use std::process::{Command, ExitStatus};

const DENY_STORE_HELPER: &str = "!f() { printf '%s\\n' \"$PRVIEW_DENIED_STORE_TAG\" >&2; printf x >> \"$PRVIEW_DENIED_STORE_MARKER\"; return 1; }; f";

pub(crate) struct Transport {
    entries: Vec<(String, String)>,
    caller: std::path::PathBuf,
    origin: String,
    empty_config: tempfile::NamedTempFile,
}

pub(super) struct TransportRun<'a> {
    transport: &'a Transport,
    marker: tempfile::NamedTempFile,
    marker_for_shell: std::ffi::OsString,
    marker_tag: String,
}

fn remove_tracing(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("GIT_TRACE") || upper == "GIT_CURL_VERBOSE" || upper == "CURL_VERBOSE"
        {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_TRACE2", "0")
        .env("GIT_TRACE2_EVENT", "0")
        .env("GIT_TRACE2_PERF", "0");
}

#[cfg(not(windows))]
fn marker_path_for_shell(path: &Path) -> Result<std::ffi::OsString> {
    Ok(path.as_os_str().to_owned())
}

#[cfg(windows)]
fn marker_path_for_shell(path: &Path) -> Result<std::ffi::OsString> {
    let path = path
        .to_str()
        .context("credential-store marker path is not Unicode")?;
    let path = if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        format!("//{unc}")
    } else if let Some(drive) = path.strip_prefix(r"\\?\") {
        drive.to_owned()
    } else {
        path.to_owned()
    };
    Ok(path.replace('\\', "/").into())
}

fn admitted(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.starts_with("http.")
        || lower.starts_with("credential.")
        || lower.starts_with("protocol.")
        || matches!(
            lower.as_str(),
            "core.sshcommand"
                | "core.gitproxy"
                | "core.askpass"
                | "ssh.variant"
                | "remote.origin.proxy"
                | "remote.origin.proxyauthmethod"
                | "remote.origin.uploadpack"
                | "remote.origin.vcs"
        )
}

fn validate_store_paths(caller: &Path, entries: &[(String, String)]) -> Result<()> {
    let repo = git2::Repository::open(caller)?;
    let roots = [
        caller.to_path_buf(),
        repo.path().to_path_buf(),
        repo.commondir().to_path_buf(),
    ];
    for (key, helper) in entries {
        if !key.to_ascii_lowercase().starts_with("credential.")
            || !key.to_ascii_lowercase().ends_with(".helper")
        {
            continue;
        }
        let trimmed = helper.trim();
        if trimmed.starts_with('!') {
            continue;
        }
        let Some(parts) = shlex::split(trimmed) else {
            ensure!(
                !trimmed.contains("store"),
                "cannot interpret credential-store options"
            );
            continue;
        };
        let Some(program) = parts.first() else {
            continue;
        };
        let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
        if !matches!(
            name.to_ascii_lowercase().as_str(),
            "store" | "git-credential-store" | "git-credential-store.exe"
        ) {
            continue;
        }
        let mut paths = Vec::new();
        match parts.as_slice() {
            [_] => {
                let home =
                    std::env::var_os("HOME").context("cannot resolve credential-store home")?;
                let home = std::path::PathBuf::from(home);
                paths.push(home.join(".git-credentials"));
                let xdg = std::env::var_os("XDG_CONFIG_HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| home.join(".config"));
                paths.push(xdg.join("git/credentials"));
            }
            [_, flag] if flag.starts_with("--file=") => paths.push(flag["--file=".len()..].into()),
            [_, flag, path] if flag == "--file" => paths.push(path.into()),
            _ => anyhow::bail!("cannot safely interpret credential-store options"),
        }
        for path in paths {
            let raw = path.to_string_lossy();
            ensure!(
                !raw.is_empty()
                    && !raw.contains(['$', '`', '*', '?', '~', ';', '|', '&', '<', '>']),
                "cannot safely interpret credential-store path"
            );
            let path = if path.is_absolute() {
                path
            } else {
                caller.join(path)
            };
            let path = crate::checks::caller_integrity::planned_path(&path)?;
            for root in &roots {
                let root = crate::checks::caller_integrity::planned_path(root)?;
                ensure!(
                    !path.starts_with(&root),
                    "credential-store path would write into the invocation checkout"
                );
            }
        }
    }
    Ok(())
}

impl Transport {
    pub(crate) fn verify_origin(
        caller: &Path,
        expected_url: &str,
        expected_specs: &[Option<String>],
    ) -> Result<()> {
        let mut url = super::git_cmd();
        remove_tracing(&mut url);
        url.env_remove("GIT_CONFIG")
            .current_dir(caller)
            .args(["remote", "get-url", "origin"]);
        let actual = crate::proc::output_governed(url, "inspect invocation origin")?;
        ensure!(actual.status.success(), "cannot inspect invocation origin");
        let actual_url = std::str::from_utf8(&actual.stdout)
            .context("origin URL is not UTF-8")?
            .trim_end_matches(['\r', '\n']);
        ensure!(
            actual_url == expected_url,
            "origin URL has inconsistent Git configuration authority; retry the review"
        );
        let mut specs = super::git_cmd();
        remove_tracing(&mut specs);
        specs.env_remove("GIT_CONFIG").current_dir(caller).args([
            "config",
            "--null",
            "--get-all",
            "remote.origin.fetch",
        ]);
        let actual =
            crate::proc::output_governed(specs, "inspect invocation origin fetch mappings")?;
        ensure!(
            matches!(actual.status.code(), Some(0 | 1)),
            "cannot inspect invocation origin fetch mappings"
        );
        let values = actual
            .stdout
            .split(|b| *b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| std::str::from_utf8(v).map(|s| Some(s.to_owned())))
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("origin fetch mapping is not UTF-8")?;
        ensure!(
            values == expected_specs,
            "origin fetch mapping has inconsistent Git configuration authority; retry the review"
        );
        Ok(())
    }

    pub(crate) fn capture(caller: &Path, origin: &str) -> Result<Self> {
        let mut command = super::git_cmd();
        remove_tracing(&mut command);
        command.env_remove("GIT_CONFIG");
        command
            .current_dir(caller)
            .args(["config", "--null", "--list", "--includes"]);
        let output =
            crate::proc::output_governed(command, "read invocation transport configuration")?;
        ensure!(
            output.status.success(),
            "cannot read invocation transport configuration"
        );
        let mut entries = Vec::new();
        for entry in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let (key, value) = match entry.iter().position(|byte| *byte == b'\n') {
                Some(pos) => (&entry[..pos], &entry[pos + 1..]),
                None => (entry, &b"true"[..]),
            };
            let key =
                std::str::from_utf8(key).context("transport configuration key is not UTF-8")?;
            if !admitted(key) {
                continue;
            }
            let value =
                std::str::from_utf8(value).context("transport configuration value is not UTF-8")?;
            entries.push((key.to_owned(), value.to_owned()));
        }
        for entry in &mut entries {
            let lower = entry.0.to_ascii_lowercase();
            if lower.starts_with("credential.")
                && lower.ends_with(".helper")
                && validate_store_paths(caller, std::slice::from_ref(entry)).is_err()
            {
                entry.1 = DENY_STORE_HELPER.to_owned();
            }
        }
        let transport = Self {
            entries,
            caller: caller.to_path_buf(),
            origin: origin.to_owned(),
            empty_config: tempfile::NamedTempFile::new()?,
        };
        transport.validate_cookie_policy(origin)?;
        Ok(transport)
    }

    fn validate_cookie_policy(&self, origin: &str) -> Result<()> {
        if !self.entries.iter().any(|(key, _)| {
            key.to_ascii_lowercase().starts_with("http.")
                && key.to_ascii_lowercase().ends_with(".savecookies")
        }) || !(origin.starts_with("https://") || origin.starts_with("http://"))
        {
            return Ok(());
        }
        let (scheme, rest) = origin.split_once("://").expect("known scheme");
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        ensure!(
            !authority.contains('@') && !origin.contains(['?', '#']),
            "cannot safely inspect cookie policy for a credential-bearing origin URL"
        );
        let url = format!("{scheme}://{authority}/{path}");
        let temporary = tempfile::tempdir()?;
        let probe = temporary.path().join("probe.git");
        git2::Repository::init_bare(&probe)?;
        let mut check = super::git_cmd();
        self.configure_environment(&mut check);
        check
            .current_dir(temporary.path())
            .arg("--git-dir")
            .arg(&probe)
            .args([
                "config",
                "--bool",
                "--get-urlmatch",
                "http.saveCookies",
                &url,
            ]);
        let result = crate::proc::output_governed(check, "check invocation cookie policy")?;
        match result.status.code() {
            Some(0) => ensure!(
                result.stdout == b"false\n",
                "remote review cannot use http.saveCookies; it can write into the invocation checkout"
            ),
            Some(1) => {}
            _ => anyhow::bail!("cannot determine invocation cookie policy"),
        }
        Ok(())
    }

    fn configure_environment(&self, command: &mut Command) {
        for (key, _) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_ascii_uppercase();
            if upper == "GIT_CONFIG"
                || upper == "GIT_CONFIG_COUNT"
                || upper == "GIT_CONFIG_PARAMETERS"
                || upper.starts_with("GIT_CONFIG_KEY_")
                || upper.starts_with("GIT_CONFIG_VALUE_")
            {
                command.env_remove(key);
            }
        }
        remove_tracing(command);
        command
            .env("GIT_CONFIG_GLOBAL", self.empty_config.path())
            .env("GIT_CONFIG_SYSTEM", self.empty_config.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", self.entries.len().to_string());
        for (index, (key, value)) in self.entries.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
    }

    pub(super) fn begin(&self) -> Result<TransportRun<'_>> {
        let marker = tempfile::NamedTempFile::new()?;
        ensure!(
            marker.path().is_absolute(),
            "credential-store marker path is not absolute"
        );
        let marker_for_shell = marker_path_for_shell(marker.path())?;
        let name = marker
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .context("credential-store marker name is not Unicode")?;
        let marker_tag = format!("prview-denied-store-{name}");
        Ok(TransportRun {
            transport: self,
            marker,
            marker_for_shell,
            marker_tag,
        })
    }

    fn redact(&self, message: &str) -> String {
        let mut message = message.to_owned();
        let mut secrets = Vec::new();
        fn url_credentials(url: &str) -> Vec<&str> {
            let Some((_, rest)) = url.split_once("://") else {
                return Vec::new();
            };
            let authority = rest.split('/').next().unwrap_or("");
            let Some((userinfo, _)) = authority.rsplit_once('@') else {
                return Vec::new();
            };
            userinfo
                .split(':')
                .filter(|part| !part.is_empty())
                .collect()
        }
        secrets.extend(url_credentials(&self.origin));
        for (key, value) in &self.entries {
            if value == DENY_STORE_HELPER {
                continue;
            }
            let lower = key.to_ascii_lowercase();
            if lower.starts_with("http.") {
                secrets.extend(url_credentials(key));
            }
            if lower.ends_with(".extraheader")
                || lower.contains("proxy")
                || lower.starts_with("credential.")
                || matches!(
                    lower.as_str(),
                    "core.sshcommand"
                        | "core.askpass"
                        | "remote.origin.uploadpack"
                        | "remote.origin.vcs"
                )
            {
                if !value.is_empty() {
                    secrets.push(value.as_str());
                }
                secrets.extend(
                    value
                        .split(|c: char| c.is_whitespace() || matches!(c, ':' | '@' | '/' | '='))
                        .filter(|part| !part.is_empty()),
                );
            }
        }
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        for secret in secrets {
            message = message.replace(secret, "[redacted]");
        }
        message
    }
}

impl TransportRun<'_> {
    pub(super) fn configure(&self, command: &mut Command, repo: &Path) {
        self.transport.configure_environment(command);
        command
            .env("PRVIEW_DENIED_STORE_MARKER", &self.marker_for_shell)
            .env("PRVIEW_DENIED_STORE_TAG", &self.marker_tag)
            .current_dir(&self.transport.caller)
            .arg("--git-dir")
            .arg(repo.join(".git"))
            .arg("--work-tree")
            .arg(repo);
    }

    pub(super) fn run(&self, command: Command, label: &str) -> Result<ExitStatus> {
        let output = crate::proc::output_governed(command, label)?;
        self.evaluate(output)
    }

    fn evaluate(&self, output: std::process::Output) -> Result<ExitStatus> {
        let was_selected = output
            .stderr
            .windows(self.marker_tag.len())
            .any(|window| window == self.marker_tag.as_bytes());
        ensure!(
            self.marker.as_file().metadata()?.len() == 0 && !was_selected,
            "credential-store was selected but its destination could write into the invocation checkout"
        );
        if !output.status.success() {
            let message = self
                .transport
                .redact(&String::from_utf8_lossy(&output.stderr));
            if !message.trim().is_empty() {
                eprintln!("{}", message.trim());
            }
        }
        Ok(output.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::git_cmd;

    #[test]
    fn captures_included_multivars_and_never_imports_unrelated_config() {
        let caller = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        git2::Repository::init(caller.path()).unwrap();
        git2::Repository::init(private.path()).unwrap();
        let included = caller.path().join("transport.inc");
        std::fs::write(&included, "[http \"https://example.invalid/\"]\n extraHeader = Alpha: fake-token-one\n extraHeader =\n extraHeader = Beta: fake-token-two\n[credential]\n helper = example-helper\n[core]\n hooksPath = /forbidden\n").unwrap();
        let mut config = git2::Repository::open(caller.path())
            .unwrap()
            .config()
            .unwrap();
        config
            .set_str("include.path", included.to_str().unwrap())
            .unwrap();
        let transport = Transport::capture(caller.path(), "https://example.invalid/repo").unwrap();
        let selected: Vec<_> = transport
            .entries
            .iter()
            .filter(|(key, _)| key.ends_with("extraheader"))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            selected,
            ["Alpha: fake-token-one", "", "Beta: fake-token-two"]
        );
        assert!(
            transport
                .entries
                .iter()
                .any(|(k, v)| k == "credential.helper" && v == "example-helper")
        );
        assert!(!transport.entries.iter().any(|(k, _)| k == "core.hookspath"));
        let mut command = git_cmd();
        let run = transport.begin().unwrap();
        run.configure(&mut command, private.path());
        assert!(
            !command
                .get_args()
                .any(|a| a.to_string_lossy().contains("fake-token"))
        );
        assert!(
            !transport
                .redact("server echoed fake-token-two")
                .contains("fake-token-two")
        );
        let config = std::fs::read(private.path().join(".git/config")).unwrap();
        assert!(!String::from_utf8_lossy(&config).contains("fake-token"));
    }

    #[test]
    fn discovery_respects_command_auth_but_not_git_config_or_traces() {
        if let Some(root) = std::env::var_os("PRVIEW_TRANSPORT_CHILD") {
            let root = std::path::PathBuf::from(root);
            let caller = root.join("caller");
            let private = root.join("private");
            let transport = Transport::capture(&caller, "https://example.invalid/repo").unwrap();
            assert!(
                Transport::verify_origin(
                    &caller,
                    "https://example.invalid/repo",
                    &[Some("+refs/heads/*:refs/remotes/origin/*".to_owned())]
                )
                .is_err()
            );
            assert!(
                transport
                    .entries
                    .iter()
                    .any(|(key, value)| key == "credential.helper" && value == "global-helper")
            );
            assert!(
                !transport
                    .entries
                    .iter()
                    .any(|(_, value)| value == "wrong-helper")
            );
            assert!(
                transport
                    .entries
                    .iter()
                    .any(|(key, value)| key == "http.extraheader"
                        && value == "X-Auth: fake-sentinel")
            );
            assert!(
                !transport
                    .entries
                    .iter()
                    .any(|(key, _)| key == "core.hookspath")
            );
            let mut command = git_cmd();
            let run = transport.begin().unwrap();
            run.configure(&mut command, &private);
            assert!(
                !command
                    .get_args()
                    .any(|v| v.to_string_lossy().contains("fake-sentinel"))
            );
            command.args(["config", "--get", "core.bare"]);
            assert!(command.output().unwrap().status.success());
            assert!(!root.join("trace").exists());
            assert!(!root.join("trace2").exists());
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let caller_repo = git2::Repository::init(root.path().join("caller")).unwrap();
        caller_repo
            .remote("origin", "https://example.invalid/repo")
            .unwrap();
        git2::Repository::init(root.path().join("private")).unwrap();
        let global = root.path().join("global");
        let misleading = root.path().join("misleading");
        std::fs::write(&global, format!(
            "[credential]\n helper = global-helper\n[trace2]\n normalTarget = {}\n eventTarget = {}\n perfTarget = {}\n",
            marker_path_for_shell(&root.path().join("trace-normal")).unwrap().to_string_lossy(),
            marker_path_for_shell(&root.path().join("trace-event")).unwrap().to_string_lossy(),
            marker_path_for_shell(&root.path().join("trace-perf")).unwrap().to_string_lossy()
        )).unwrap();
        std::fs::write(&misleading, "[credential]\n helper = wrong-helper\n").unwrap();
        let control = root.path().join("control-global");
        std::fs::write(
            &control,
            format!(
                "[trace2]\n normalTarget = {}\n eventTarget = {}\n perfTarget = {}\n",
                root.path().join("control-normal").display(),
                root.path().join("control-event").display(),
                root.path().join("control-perf").display()
            ),
        )
        .unwrap();
        let control_status = git_cmd()
            .current_dir(root.path().join("caller"))
            .env("GIT_CONFIG_GLOBAL", control)
            .env_remove("GIT_TRACE2")
            .env_remove("GIT_TRACE2_EVENT")
            .env_remove("GIT_TRACE2_PERF")
            .args(["config", "--list"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(control_status.success());
        for name in ["control-normal", "control-event", "control-perf"] {
            assert!(
                root.path().join(name).exists(),
                "trace control did not create {name}"
            );
        }
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "git::transport::tests::discovery_respects_command_auth_but_not_git_config_or_traces"])
            .env("PRVIEW_TRANSPORT_CHILD", root.path())
            .env("GIT_CONFIG_GLOBAL", global)
            .env("GIT_CONFIG", misleading)
            .env("GIT_TRACE", root.path().join("trace"))
            .env("GIT_TRACE2_EVENT", root.path().join("trace2"))
            .env("GIT_CURL_VERBOSE", "1")
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", "X-Auth: fake-sentinel")
            .env("GIT_CONFIG_KEY_1", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_1", "/forbidden")
            .env("GIT_CONFIG_KEY_2", "url.https://other.invalid/.insteadOf")
            .env("GIT_CONFIG_VALUE_2", "https://example.invalid/")
            .output().unwrap();
        assert!(
            result.status.success(),
            "isolated discovery subprocess failed"
        );
        assert!(!String::from_utf8_lossy(&result.stderr).contains("fake-sentinel"));
        assert!(!root.path().join("trace").exists());
        assert!(!root.path().join("trace2").exists());
        assert!(!root.path().join("trace-normal").exists());
        assert!(!root.path().join("trace-event").exists());
        assert!(!root.path().join("trace-perf").exists());
    }

    fn credential_output(
        run: &TransportRun<'_>,
        repo: &Path,
        operation: &str,
    ) -> Result<std::process::Output> {
        let payload: &[u8] = if operation == "fill" {
            b"url=https://example.invalid/repo\n\n"
        } else {
            b"url=https://example.invalid/repo\nusername=fixture\npassword=fixture\n\n"
        };
        let mut command = git_cmd();
        run.configure(&mut command, repo);
        command.args(["credential", operation]);
        crate::proc::output_governed_with_input(command, "fixture credential operation", payload)
    }

    fn credential_command(
        run: &TransportRun<'_>,
        repo: &Path,
        operation: &str,
    ) -> Result<ExitStatus> {
        run.evaluate(credential_output(run, repo, operation)?)
    }

    #[test]
    fn selected_caller_store_is_blocked_on_get_and_erase() {
        let caller = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(caller.path()).unwrap();
        let private = tempfile::tempdir().unwrap();
        git2::Repository::init(private.path()).unwrap();
        let credentials = caller.path().join(".git/credentials");
        std::fs::write(&credentials, "original-fixture\n").unwrap();
        let before = std::fs::read(&credentials).unwrap();
        for helper in [
            "store --file=.git/credentials",
            "\"store\" --file=.git/credentials",
            "st\\ore --file=.git/credentials",
            "/usr/bin/git-credential-store --file=.git/credentials",
            "C:/Git/bin/git-credential-store.exe --file=.git/credentials",
        ] {
            repo.config()
                .unwrap()
                .set_str("credential.helper", helper)
                .unwrap();
            let mut transport =
                Transport::capture(caller.path(), "https://example.invalid/repo").unwrap();
            assert!(
                transport
                    .entries
                    .iter()
                    .any(|(key, value)| key == "credential.helper" && value == DENY_STORE_HELPER)
            );
            transport
                .entries
                .retain(|(key, _)| !key.starts_with("credential."));
            transport
                .entries
                .push(("credential.helper".into(), DENY_STORE_HELPER.into()));
            transport.entries.push((
                "credential.helper".into(),
                "!f() { test \"$1\" = get && printf 'username=fixture\\npassword=fixture\\n'; }; f"
                    .into(),
            ));
            for operation in ["fill", "reject"] {
                let run = transport.begin().unwrap();
                assert!(run.marker.path().is_absolute());
                assert!(!run.marker.path().starts_with(caller.path()));
                assert!(credential_command(&run, private.path(), operation).is_err());
                assert!(run.marker.as_file().metadata().unwrap().len() > 0);
            }
            assert_eq!(std::fs::read(&credentials).unwrap(), before);
        }
        assert!(!caller.path().join(".git/FETCH_HEAD").exists());
    }

    #[cfg(windows)]
    #[test]
    fn marker_paths_are_native_git_shell_absolute_paths() {
        for (input, expected) in [
            (r"C:\Temp\marker", "C:/Temp/marker"),
            (r"\\?\C:\Temp\marker", "C:/Temp/marker"),
            (r"\\server\share\marker", "//server/share/marker"),
            (r"\\?\UNC\server\share\marker", "//server/share/marker"),
        ] {
            assert_eq!(marker_path_for_shell(Path::new(input)).unwrap(), expected);
        }
    }

    #[test]
    fn store_outside_caller_remains_unchanged() {
        let caller = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(caller.path()).unwrap();
        let file = marker_path_for_shell(&external.path().join("credentials")).unwrap();
        let helper = format!("store --file='{}'", file.to_string_lossy());
        repo.config()
            .unwrap()
            .set_str("credential.helper", &helper)
            .unwrap();
        let transport = Transport::capture(caller.path(), "https://example.invalid/repo").unwrap();
        assert!(
            transport
                .entries
                .iter()
                .any(|(key, value)| key == "credential.helper" && value == &helper)
        );
    }

    #[test]
    fn denied_store_is_detected_when_marker_write_fails_and_git_succeeds() {
        let caller = tempfile::tempdir().unwrap();
        git2::Repository::init(caller.path()).unwrap();
        let private = tempfile::tempdir().unwrap();
        git2::Repository::init(private.path()).unwrap();
        let transport = Transport {
            entries: vec![
                ("credential.helper".into(), DENY_STORE_HELPER.into()),
                ("credential.helper".into(), "!f() { test \"$1\" = get && printf 'username=fixture\\npassword=fixture\\n'; }; f".into()),
            ],
            caller: caller.path().to_path_buf(),
            origin: "https://example.invalid/repo".into(),
            empty_config: tempfile::NamedTempFile::new().unwrap(),
        };
        let mut run = transport.begin().unwrap();
        run.marker_for_shell = marker_path_for_shell(caller.path()).unwrap();
        let output = credential_output(&run, private.path(), "fill").unwrap();
        assert!(
            output.status.success(),
            "the secondary fixture helper must make Git succeed"
        );
        assert_eq!(run.marker.as_file().metadata().unwrap().len(), 0);
        assert!(
            run.evaluate(output).is_err(),
            "stderr tag must signal denial after marker write fails"
        );
        assert!(!caller.path().join(".git/credentials").exists());
    }

    #[test]
    fn reset_and_unmatched_url_do_not_activate_denied_store() {
        let caller = tempfile::tempdir().unwrap();
        git2::Repository::init(caller.path()).unwrap();
        let private = tempfile::tempdir().unwrap();
        git2::Repository::init(private.path()).unwrap();
        let fake_helper =
            "!f() { test \"$1\" = get && printf 'username=fixture\\npassword=fixture\\n'; }; f";
        for entries in [
            vec![
                ("credential.helper", DENY_STORE_HELPER),
                ("credential.helper", ""),
                ("credential.helper", fake_helper),
            ],
            vec![
                ("credential.https://other.invalid.helper", DENY_STORE_HELPER),
                ("credential.helper", fake_helper),
            ],
        ] {
            let transport = Transport {
                entries: entries
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
                caller: caller.path().to_path_buf(),
                origin: "https://example.invalid/repo".into(),
                empty_config: tempfile::NamedTempFile::new().unwrap(),
            };
            let run = transport.begin().unwrap();
            assert!(
                credential_command(&run, private.path(), "fill")
                    .unwrap()
                    .success()
            );
            assert_eq!(run.marker.as_file().metadata().unwrap().len(), 0);
        }
    }

    #[test]
    fn denied_marker_does_not_garble_fetch_diagnostics() {
        let transport = Transport {
            entries: vec![("credential.helper".into(), DENY_STORE_HELPER.into())],
            caller: std::path::PathBuf::new(),
            origin: "https://example.invalid/repo".into(),
            empty_config: tempfile::NamedTempFile::new().unwrap(),
        };
        assert_eq!(
            transport.redact("fatal: connection refused"),
            "fatal: connection refused"
        );
    }

    #[test]
    fn failure_diagnostics_redact_url_and_ssh_credentials() {
        let transport = Transport {
            entries: vec![
                (
                    "http.https://fakeuser:fakekey@example.invalid/.extraheader".into(),
                    "X-Token: fakeheader".into(),
                ),
                (
                    "core.sshcommand".into(),
                    "ssh --password=fakesshpass".into(),
                ),
            ],
            caller: std::path::PathBuf::new(),
            origin: "https://originuser:originpass@example.invalid/repo".into(),
            empty_config: tempfile::NamedTempFile::new().unwrap(),
        };
        let error = transport
            .redact("fakeuser fakekey fakeheader fakesshpass originuser originpass network failed");
        for secret in [
            "fakeuser",
            "fakekey",
            "fakeheader",
            "fakesshpass",
            "originuser",
            "originpass",
        ] {
            assert!(!error.contains(secret));
        }
        assert!(error.contains("network failed"));
    }

    #[test]
    fn inline_origin_credentials_never_enter_cookie_probe_arguments() {
        let caller = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(caller.path()).unwrap();
        let private = tempfile::tempdir().unwrap();
        git2::Repository::init(private.path()).unwrap();
        let url = "https://fake-inline-credential@example.invalid/repo";
        let transport = Transport::capture(caller.path(), url).unwrap();
        let mut command = git_cmd();
        let run = transport.begin().unwrap();
        run.configure(&mut command, private.path());
        assert!(
            !command
                .get_args()
                .any(|arg| arg.to_string_lossy().contains("fake-inline-credential"))
        );
        repo.config()
            .unwrap()
            .set_bool("http.saveCookies", false)
            .unwrap();
        let error = match Transport::capture(caller.path(), url) {
            Ok(_) => panic!("credential-bearing URL with cookie config must be refused"),
            Err(error) => error,
        };
        assert!(!format!("{error:#}").contains("fake-inline-credential"));
        for url in [
            "https://example.invalid?token=fake-query",
            "https://example.invalid#fake-fragment",
        ] {
            let error = match Transport::capture(caller.path(), url) {
                Ok(_) => panic!("query or fragment URL with cookie config must be refused"),
                Err(error) => error,
            };
            assert!(!format!("{error:#}").contains("fake-query"));
            assert!(!format!("{error:#}").contains("fake-fragment"));
        }
    }

    #[test]
    fn cookie_policy_matches_origin_not_unrelated_url() {
        let caller = tempfile::tempdir().unwrap();
        git2::Repository::init(caller.path()).unwrap();
        let repo = git2::Repository::open(caller.path()).unwrap();
        let mut config = repo.config().unwrap();
        config
            .set_bool("http.https://other.invalid/.saveCookies", true)
            .unwrap();
        assert!(Transport::capture(caller.path(), "https://example.invalid/repo").is_ok());
        config
            .set_bool("http.https://example.invalid/.saveCookies", true)
            .unwrap();
        assert!(Transport::capture(caller.path(), "https://example.invalid/repo").is_err());
    }

    #[test]
    fn http_fetch_matches_caller_includeif_multivar_and_reset() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let caller = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let caller_repo = git2::Repository::init(caller.path()).unwrap();
        let private_repo = git2::Repository::init(private.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/repo", listener.local_addr().unwrap());
        caller_repo.remote("origin", &url).unwrap();
        private_repo.remote("origin", &url).unwrap();
        let included = caller.path().join("headers.inc");
        std::fs::write(&included, "[http]\n extraHeader = X-Old: rejected\n extraHeader =\n extraHeader = X-Fixture: fake-token-value\n").unwrap();
        let mut config = caller_repo.config().unwrap();
        let gitdir = marker_path_for_shell(caller_repo.path()).unwrap();
        config
            .set_str(
                &format!(
                    "includeIf.gitdir:{}.path",
                    gitdir.to_string_lossy().trim_end_matches('/')
                ),
                included.to_str().unwrap(),
            )
            .unwrap();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut requests = Vec::new();
            while requests.len() < 2 && std::time::Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                            .unwrap();
                        let mut request = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = stream.read(&mut buf).unwrap();
                            if n == 0 {
                                break;
                            }
                            request.extend_from_slice(&buf[..n]);
                        }
                        stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                        requests.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(error) => panic!("server accept: {error}"),
                }
            }
            requests
        });
        let caller_status = git_cmd()
            .args(["fetch", "--quiet", "origin"])
            .current_dir(caller.path())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!caller_status.success());
        let transport = Transport::capture(caller.path(), &url).unwrap();
        let mut command = git_cmd();
        let run = transport.begin().unwrap();
        run.configure(&mut command, private.path());
        command.args(["fetch", "--quiet", "origin"]);
        assert!(!run.run(command, "fixture HTTP fetch").unwrap().success());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests {
            assert!(
                request.contains("x-fixture: fake-token-value"),
                "fixture header absent"
            );
            assert!(
                !request.contains("x-old: rejected"),
                "reset header survived"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn ssh_fetch_does_not_invoke_http_credential_store() {
        use std::os::unix::fs::PermissionsExt;
        let caller = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let upstream = tempfile::tempdir().unwrap();
        let source = git2::Repository::init(caller.path()).unwrap();
        let destination = git2::Repository::init(private.path()).unwrap();
        git2::Repository::init_bare(upstream.path()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let tree_id = source.treebuilder(None).unwrap().write().unwrap();
        let tree = source.find_tree(tree_id).unwrap();
        source
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        assert!(
            git_cmd()
                .args(["-c", "core.hooksPath=", "push", "--quiet"])
                .arg(upstream.path())
                .arg("HEAD:refs/heads/main")
                .current_dir(caller.path())
                .status()
                .unwrap()
                .success()
        );
        let ssh = caller.path().join("fixture-ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nexec git-upload-pack \"{}\"\n",
                upstream.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let url = "ssh://fixture.invalid/repo";
        source.remote("origin", url).unwrap();
        destination.remote("origin", url).unwrap();
        let mut config = source.config().unwrap();
        config.set_str("core.sshCommand", "./fixture-ssh").unwrap();
        config.set_str("ssh.variant", "simple").unwrap();
        config
            .set_str("credential.helper", "store --file=.git/credentials")
            .unwrap();
        let transport = Transport::capture(caller.path(), url).unwrap();
        let run = transport.begin().unwrap();
        let mut command = git_cmd();
        run.configure(&mut command, private.path());
        command
            .env_remove("GIT_SSH_COMMAND")
            .args(["fetch", "--quiet", "origin"]);
        assert!(run.run(command, "fixture SSH fetch").unwrap().success());
        assert_eq!(run.marker.as_file().metadata().unwrap().len(), 0);
        assert!(!caller.path().join(".git/credentials").exists());
        assert!(
            destination
                .find_reference("refs/remotes/origin/main")
                .is_ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_uploadpack_uses_caller_cwd_but_updates_only_owned_repo() {
        use std::os::unix::fs::PermissionsExt;
        let caller = tempfile::tempdir().unwrap();
        let private = tempfile::tempdir().unwrap();
        let upstream = tempfile::tempdir().unwrap();
        let caller_repo = git2::Repository::init(caller.path()).unwrap();
        git2::Repository::init(private.path()).unwrap();
        git2::Repository::init_bare(upstream.path()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let tree_id = caller_repo.treebuilder(None).unwrap().write().unwrap();
        let tree = caller_repo.find_tree(tree_id).unwrap();
        let oid = caller_repo
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        let status = git_cmd()
            .args(["-c", "core.hooksPath=", "push", "--quiet"])
            .arg(upstream.path())
            .arg("HEAD:refs/heads/main")
            .current_dir(caller.path())
            .status()
            .unwrap();
        assert!(status.success());
        let script = caller.path().join("upload-wrapper");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$PWD\" = \"{}\" ] || exit 71\nexec git-upload-pack \"{}\"\n",
                std::fs::canonicalize(caller.path()).unwrap().display(),
                upstream.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let virtual_path = caller.path().join("not-a-repository");
        caller_repo
            .remote("origin", virtual_path.to_str().unwrap())
            .unwrap();
        caller_repo
            .config()
            .unwrap()
            .set_str("remote.origin.uploadpack", "./upload-wrapper")
            .unwrap();
        caller_repo
            .config()
            .unwrap()
            .set_str("credential.helper", "store --file=.git/credentials")
            .unwrap();
        let status = git_cmd()
            .args(["fetch", "--quiet", "origin"])
            .current_dir(caller.path())
            .status()
            .unwrap();
        assert!(status.success(), "native caller fetch must work");
        let caller_ref = caller_repo
            .find_reference("refs/remotes/origin/main")
            .unwrap()
            .target();
        let caller_fetch_head = std::fs::read(caller.path().join(".git/FETCH_HEAD")).unwrap();
        let private_repo = git2::Repository::open(private.path()).unwrap();
        private_repo
            .remote("origin", virtual_path.to_str().unwrap())
            .unwrap();
        let status = git_cmd()
            .args(["fetch", "--quiet", "origin"])
            .current_dir(private.path())
            .status()
            .unwrap();
        assert!(
            !status.success(),
            "private fetch must reproduce the original failure"
        );
        let transport = Transport::capture(caller.path(), virtual_path.to_str().unwrap()).unwrap();
        let mut command = git_cmd();
        let run = transport.begin().unwrap();
        run.configure(&mut command, private.path());
        command.args(["fetch", "--quiet", "origin"]);
        assert!(run.run(command, "fixture fetch").unwrap().success());
        assert_eq!(
            private_repo
                .find_reference("refs/remotes/origin/main")
                .unwrap()
                .target(),
            Some(oid)
        );
        assert_eq!(
            caller_repo
                .find_reference("refs/remotes/origin/main")
                .unwrap()
                .target(),
            caller_ref
        );
        assert_eq!(
            std::fs::read(caller.path().join(".git/FETCH_HEAD")).unwrap(),
            caller_fetch_head
        );
        assert!(!caller.path().join(".git/credentials").exists());
    }
}
