//! Python checks (ruff, mypy, pytest)

use super::{
    Check, CheckProvenance, CheckResult, CheckStatus, TEST_TIMEOUT_SECS, find_hard_fail_signatures,
    off_head_target_commit, plan_check_run, run_command_with_env, run_command_with_timeout_and_env,
    tool_spawn_failure_in_output,
};
use crate::Config;
use crate::cache;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct RuffCheck;
pub struct MypyCheck;
pub struct PytestCheck;

/// Skip reason when the REVIEWED commit is not a Python project.
///
/// `config.profile` describes the local checkout. When a target removes the last
/// Python project and source files, the checkout still says "Python" and the
/// checks were still scheduled — into a snapshot that has no Python in it.
/// Pytest is where that hurts: it exits 5 for "no tests collected", a blocking
/// failure attributed to a target the check no longer applies to. Ruff and Mypy
/// pass vacuously, which is a green signal for something never examined; both
/// are answers about a question that should not have been asked.
///
/// The same shape as `missing_reviewed_cargo_manifest`, and answered from git —
/// the snapshot carries exactly this tree, so no worktree is materialised to ask.
///
/// Fail open at every step: a question git cannot answer must not become a skip,
/// so an unreadable repo or a failed walk leaves the check running.
fn missing_reviewed_python_project(config: &Config) -> Option<String> {
    let commit = off_head_target_commit(config)?;
    let repo = crate::git::Repository::open(&config.repo_root).ok()?;

    // A pyproject.toml is an explicit project declaration and settles it alone,
    // exactly as `runs_python_checks` treats it locally.
    if repo
        .regular_file_at_commit(&commit, "pyproject.toml")
        .unwrap_or(true)
    {
        return None;
    }
    if repo
        .any_file_at_commit(&commit, crate::config::is_runtime_python_path)
        .unwrap_or(true)
    {
        return None;
    }

    let short = &commit[..commit.len().min(8)];
    Some(format!(
        "commit {short} has no pyproject.toml and no Python source — not a Python project",
    ))
}

/// Where a python check must execute, plus the environment it needs there.
pub(super) struct PythonRun {
    /// Directory to run the tool in — the reviewed snapshot in `--pr`/`--remote`
    /// mode, the local checkout otherwise.
    pub(super) cwd: PathBuf,
    /// Bounded child pools plus `UV_PROJECT_ENVIRONMENT` for an off-HEAD run.
    pub(super) env: Vec<(String, String)>,
    /// Ephemeral snapshot, kept alive until the check finishes.
    _snapshot: Option<crate::git::WorktreeSnapshot>,
}

/// Resolve where a python check runs, and isolate uv from the operator's
/// environment when that place is a target snapshot.
///
/// `create_worktree_snapshot` symlinks the checkout's `.venv` into the snapshot
/// so a review does not reinstall every dependency. `uv run` synchronises the
/// project environment before executing, so a reviewed commit that adds, drops
/// or pins dependencies differently would mutate the developer's ACTIVE
/// environment through that symlink — a review is a read of someone's branch,
/// never a write to their machine. `UV_PROJECT_ENVIRONMENT` moves the sync into
/// a prview-owned directory ([`Config::uv_env_dir_for`]), so the reviewed
/// dependency set is still installed and judged, just not on top of the
/// operator's.
///
/// That environment is per REVIEWED COMMIT, not per repository. `uv run` syncs
/// before executing and releases the environment lock while the child command
/// runs, so two prview processes reviewing different commits of one repo would
/// take turns installing incompatible dependency sets into the same directory —
/// each one resynchronising (and removing packages) under the other's running
/// pytest. A commit-scoped path makes those two reviews independent while
/// keeping the environment warm across runs of the SAME commit, which is the
/// case that pays for itself (re-review, `--watch`).
///
/// A local review (target == `HEAD`) keeps its checkout environment directory;
/// only the bounded descendant pools are added.
pub(super) fn plan_python_run(config: &Config) -> Result<PythonRun> {
    plan_python_run_with_env(config, |key| std::env::var_os(key))
}

pub(super) fn plan_python_run_with_env(
    config: &Config,
    inherited: impl FnMut(&str) -> Option<OsString>,
) -> Result<PythonRun> {
    plan_python_tool_run_with_env(config, true, inherited)
}

fn plan_python_tool_run(config: &Config, use_uv: bool) -> Result<PythonRun> {
    plan_python_tool_run_with_env(config, use_uv, |key| std::env::var_os(key))
}

fn plan_python_tool_run_with_env(
    config: &Config,
    use_uv: bool,
    mut inherited: impl FnMut(&str) -> Option<OsString>,
) -> Result<PythonRun> {
    let plan = plan_check_run(config)?;
    // A directly installed Ruff, Mypy, or Pytest never invokes uv. In that
    // path uv-only environment selectors and repository configuration are not
    // execution authority and must not turn an otherwise runnable check into
    // an error. Generic Python metadata remains contained below because the
    // direct tools still consume pyproject/pytest configuration themselves.
    let (no_discovered_uv_config, explicit_config) = if use_uv {
        (
            uv_no_config_enabled(&mut inherited)?,
            checked_uv_environment_with(&plan.scan_dir, &mut inherited)?,
        )
    } else {
        (true, None)
    };
    metadata_stays_in_project(
        &plan.scan_dir,
        explicit_config.is_some() || no_discovered_uv_config,
        use_uv,
    )?;
    let configured_limits = if use_uv {
        project_uv_concurrency_limits(
            &plan.scan_dir,
            explicit_config.as_deref(),
            no_discovered_uv_config,
        )?
    } else {
        super::UvConcurrencyLimits::default()
    };
    // Every `uv run`, not only the eager pre-sync, may synchronize or build the
    // environment. Keep all of uv's own pools and Cargo-backed PEP 517 builds
    // inside the same descendant envelope the run advertises. An operator's
    // stricter inherited or project-owned cap still wins.
    let mut env = if use_uv {
        super::uv_concurrency_env_with(
            config.resource_plan.worker_limit,
            configured_limits,
            &mut inherited,
        )?
    } else {
        // Direct Python tools do not consume UV_CONCURRENT_* either. Preserve
        // only the generic Cargo backend cap for plugins that build extensions.
        let inherited_cargo_jobs =
            inherited("CARGO_BUILD_JOBS").and_then(|value| value.into_string().ok());
        vec![(
            "CARGO_BUILD_JOBS".to_owned(),
            super::bounded_descendant_limit(
                config.resource_plan.worker_limit,
                inherited_cargo_jobs.as_deref(),
            )
            .to_string(),
        )]
    };
    if use_uv && plan.scan_dir != config.repo_root {
        let env_dir = config.uv_env_dir_for(&reviewed_env_token(config, &plan.scan_dir));
        mark_and_prune_uv_envs(&config.uv_env_root(), &env_dir);
        env.push((
            "UV_PROJECT_ENVIRONMENT".to_string(),
            env_dir.display().to_string(),
        ));
    }
    Ok(PythonRun {
        cwd: plan.scan_dir,
        env,
        _snapshot: plan._snapshot,
    })
}

/// The files that decide what a Python run actually reads.
///
/// `pyproject.toml` is the project itself — ruff, mypy and pytest take their
/// configuration from it, and uv discovers the project through it. `uv.toml`
/// overrides `[tool.uv]` in that project file. `uv.lock` pins the dependency set
/// that gets installed and imported. Dedicated pytest configs are included
/// because the selected one controls collection and worker count just as
/// directly; none may escape the reviewed tree through a symlink.
const PYTHON_PROJECT_METADATA: &[&str] = &[
    "pyproject.toml",
    "uv.toml",
    "uv.lock",
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
    "tox.ini",
    "setup.cfg",
];
/// Pytest 6.0 through 7.1 did not include `.pytest.ini` in `config_names`.
const PYTEST_PRE_HIDDEN_CONFIG_FILES: &[&str] =
    &["pytest.ini", "pyproject.toml", "tox.ini", "setup.cfg"];
/// Pytest 7.2-8's discovery order. These versions do not recognize the native
/// TOML configuration introduced in pytest 9.
const PYTEST_LEGACY_CONFIG_FILES: &[&str] = &[
    "pytest.ini",
    ".pytest.ini",
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
];
/// Pytest 9's discovery order. Dedicated TOML and INI files match even when
/// empty; generic project files require a pytest section/table.
const PYTEST_NINE_CONFIG_FILES: &[&str] = &[
    "pytest.toml",
    ".pytest.toml",
    "pytest.ini",
    ".pytest.ini",
    "pyproject.toml",
    "tox.ini",
    "setup.cfg",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PytestConfigDialect {
    /// Pytest 6.0-7.1: no hidden INI name and no pyproject fallback.
    LegacyPreHidden,
    /// Pytest 7.2-8.0: hidden INI name, but no pyproject fallback.
    LegacyHidden,
    /// Pytest 8.1-8.x: hidden INI name and sectionless pyproject fallback.
    Legacy,
    Nine,
}

#[derive(Debug, PartialEq, Eq)]
enum PytestConfigMatch {
    NotMatched,
    Matched(Option<Vec<String>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum XdistWorkerRequest {
    Dynamic,
    Count(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum XdistWorkerDisposition {
    Absent,
    Disabled,
    Requested(XdistWorkerRequest),
}

type PytestInvocation = (Vec<String>, Vec<(String, String)>);

fn xdist_value_disposition(value: &str) -> Option<XdistWorkerDisposition> {
    if matches!(value, "auto" | "logical") {
        Some(XdistWorkerDisposition::Requested(
            XdistWorkerRequest::Dynamic,
        ))
    } else if let Some((negative, digits)) = normalized_python_int(value) {
        let normalized = digits.trim_start_matches('0');
        if negative {
            return Some(XdistWorkerDisposition::Disabled);
        }
        if normalized.is_empty() {
            Some(XdistWorkerDisposition::Disabled)
        } else {
            Some(XdistWorkerDisposition::Requested(
                XdistWorkerRequest::Count(normalized.to_owned()),
            ))
        }
    } else {
        None
    }
}

/// Safe `int()` spellings accepted by pytest-xdist's `parse_numprocesses`:
/// outer ASCII whitespace, an optional sign, and ASCII digits with underscores
/// only between digits. Unicode digits stay fail-closed deliberately.
fn normalized_python_int(value: &str) -> Option<(bool, String)> {
    let value = value.trim_matches(|ch: char| ch.is_ascii_whitespace());
    let (negative, unsigned) = match value.as_bytes().first() {
        Some(b'+') => (false, &value[1..]),
        Some(b'-') => (true, &value[1..]),
        _ => (false, value),
    };
    if unsigned.is_empty() {
        return None;
    }
    let bytes = unsigned.as_bytes();
    if !bytes.first()?.is_ascii_digit() || !bytes.last()?.is_ascii_digit() {
        return None;
    }
    let mut digits = String::with_capacity(unsigned.len());
    for (index, byte) in bytes.iter().copied().enumerate() {
        if byte.is_ascii_digit() {
            digits.push(char::from(byte));
        } else if byte == b'_'
            && bytes[index - 1].is_ascii_digit()
            && bytes[index + 1].is_ascii_digit()
        {
            continue;
        } else {
            return None;
        }
    }
    Some((negative, digits))
}

fn xdist_worker_disposition(tokens: &[String]) -> Result<XdistWorkerDisposition> {
    let mut disposition = XdistWorkerDisposition::Absent;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        let parsed = if matches!(token.as_str(), "-n" | "--numprocesses") {
            Some((tokens.get(index + 1).map(String::as_str), 2))
        } else if let Some(value) = token.strip_prefix("--numprocesses=") {
            Some((Some(value), 1))
        } else if let Some(value) = token.strip_prefix("-n") {
            Some((Some(value.strip_prefix('=').unwrap_or(value)), 1))
        } else {
            None
        };
        let Some((value, consumed)) = parsed else {
            index += 1;
            continue;
        };
        let value = value.ok_or_else(|| anyhow::anyhow!("{token} is missing its worker count"))?;
        disposition = xdist_value_disposition(value).ok_or_else(|| {
            anyhow::anyhow!(
                "{token} uses unsupported worker count {value:?}; refusing an unbounded pytest run"
            )
        })?;
        index += consumed;
    }
    Ok(disposition)
}

fn xdist_request_exceeds_limit(request: &XdistWorkerRequest, worker_limit: u32) -> bool {
    match request {
        XdistWorkerRequest::Dynamic => true,
        XdistWorkerRequest::Count(count) => {
            let limit = worker_limit.to_string();
            count.len() > limit.len()
                || (count.len() == limit.len() && count.as_str() > limit.as_str())
        }
    }
}

/// Rust's `shlex` crate follows shell comment rules, while Python explicitly
/// calls `shlex.split(..., comments=False)` for pytest addopts. Escape only
/// unquoted, unescaped `#` bytes before splitting so they remain ordinary token
/// content without changing quote or backslash semantics.
fn escape_unquoted_hashes(value: &str) -> String {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut escaped = false;
    let mut quote = Quote::None;
    let mut prepared = String::with_capacity(value.len());
    for ch in value.chars() {
        if escaped {
            prepared.push(ch);
            escaped = false;
            continue;
        }
        match (quote, ch) {
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::None | Quote::Double, '\\') => escaped = true,
            (Quote::None, '#') => prepared.push('\\'),
            _ => {}
        }
        prepared.push(ch);
    }
    prepared
}

fn split_pytest_addopts(value: &str) -> Result<Vec<String>> {
    shlex::split(&escape_unquoted_hashes(value)).ok_or_else(|| {
        anyhow::anyhow!("pytest addopts contains an unterminated quote or trailing escape")
    })
}

fn checked_pytest_addopts(
    value: std::result::Result<String, std::env::VarError>,
) -> Result<Option<String>> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("PYTEST_ADDOPTS is not valid Unicode and cannot be bounded safely")
        }
    }
}

fn validate_pytest_addopts_safety(tokens: &[String], source: &str) -> Result<()> {
    for token in tokens {
        if token == "--" {
            anyhow::bail!(
                "{source} contains `--`, which would turn prview's pytest safety arguments into positional values"
            );
        }
        if matches!(token.as_str(), "--tx" | "--px")
            || token.starts_with("--tx=")
            || token.starts_with("--px=")
        {
            anyhow::bail!(
                "{source} contains unsupported xdist gateway option {token:?}; prview cannot bound custom or proxy gateways"
            );
        }
    }
    Ok(())
}

fn normalize_xdist_auto_workers(value: &str) -> Result<String> {
    let Some((negative, digits)) = normalized_python_int(value) else {
        anyhow::bail!(
            "PYTEST_XDIST_AUTO_NUM_WORKERS must be a positive ASCII integer, got {value:?}"
        );
    };
    let normalized = digits.trim_start_matches('0');
    if negative || normalized.is_empty() {
        anyhow::bail!("PYTEST_XDIST_AUTO_NUM_WORKERS must be greater than zero");
    }
    Ok(normalized.to_owned())
}

fn checked_xdist_auto_workers(
    value: std::result::Result<String, std::env::VarError>,
) -> Result<Option<String>> {
    match value {
        Ok(value) => normalize_xdist_auto_workers(&value).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!(
            "PYTEST_XDIST_AUTO_NUM_WORKERS is not valid Unicode and cannot be bounded safely"
        ),
    }
}

fn toml_addopts(value: &toml::Value, source: &str) -> Result<Vec<String>> {
    match value {
        toml::Value::String(value) => {
            split_pytest_addopts(value).with_context(|| format!("invalid addopts in {source}"))
        }
        toml::Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("{source} addopts[{index}] is not a string"))
            })
            .collect(),
        _ => anyhow::bail!("{source} addopts must be a string or an array of strings"),
    }
}

type IniDocument = BTreeMap<String, BTreeMap<String, String>>;

/// Parse enough of pytest's INI grammar to make discovery and addopts
/// fail-closed. Invalid files must not be silently skipped: pytest itself would
/// stop before collection rather than continue to a lower-priority config.
fn parse_pytest_ini(name: &str, contents: &str) -> Result<IniDocument> {
    let mut document = IniDocument::new();
    let mut current_section: Option<String> = None;
    let mut current_key: Option<String> = None;

    for (index, line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        if line.starts_with('[') {
            let section_line = line
                .split(['#', ';'])
                .next()
                .expect("split always yields one item")
                .trim_end();
            if !section_line.ends_with(']') {
                anyhow::bail!("malformed section header in {name}:{line_number}");
            }
            let section = &section_line[1..section_line.len() - 1];
            if section.is_empty() {
                anyhow::bail!("empty section name in {name}:{line_number}");
            }
            if document
                .insert(section.to_owned(), BTreeMap::new())
                .is_some()
            {
                anyhow::bail!("duplicate section [{section}] in {name}:{line_number}");
            }
            current_section = Some(section.to_owned());
            current_key = None;
            continue;
        }

        if line.chars().next().is_some_and(char::is_whitespace) {
            let section = current_section.as_ref().ok_or_else(|| {
                anyhow::anyhow!("continuation before a section in {name}:{line_number}")
            })?;
            let key = current_key.as_ref().ok_or_else(|| {
                anyhow::anyhow!("continuation before an option in {name}:{line_number}")
            })?;
            let value = document
                .get_mut(section)
                .and_then(|options| options.get_mut(key))
                .expect("current INI option exists");
            if !value.is_empty() {
                value.push('\n');
            }
            value.push_str(trimmed);
            continue;
        }

        let section = current_section
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("option before a section in {name}:{line_number}"))?;
        let delimiter = match (trimmed.find('='), trimmed.find(':')) {
            (Some(equal), Some(colon)) => equal.min(colon),
            (Some(equal), None) => equal,
            (None, Some(colon)) => colon,
            (None, None) => anyhow::bail!("malformed option in {name}:{line_number}"),
        };
        let key = trimmed[..delimiter].trim();
        if key.is_empty() {
            anyhow::bail!("empty option name in {name}:{line_number}");
        }
        let options = document
            .get_mut(section)
            .expect("current INI section exists");
        if options
            .insert(key.to_owned(), trimmed[delimiter + 1..].trim().to_owned())
            .is_some()
        {
            anyhow::bail!("duplicate option {key} in {name}:{line_number}");
        }
        current_key = Some(key.to_owned());
    }

    Ok(document)
}

fn table_at<'a>(value: &'a toml::Value, path: &str, source: &str) -> Result<&'a toml::Table> {
    value
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("{source} [{path}] must be a table"))
}

fn optional_toml_addopts(table: &toml::Table, source: &str) -> Result<Option<Vec<String>>> {
    table
        .get("addopts")
        .map(|value| toml_addopts(value, source))
        .transpose()
}

fn pytest_config_addopts(
    name: &str,
    contents: &str,
    dialect: PytestConfigDialect,
) -> Result<PytestConfigMatch> {
    if matches!(name, "pytest.toml" | ".pytest.toml") {
        if dialect != PytestConfigDialect::Nine {
            return Ok(PytestConfigMatch::NotMatched);
        }
        let document = toml::from_str::<toml::Value>(contents)
            .with_context(|| format!("failed to parse {name}"))?;
        let addopts = document
            .get("pytest")
            .map(|pytest| table_at(pytest, "pytest", name))
            .transpose()?
            .map(|pytest| optional_toml_addopts(pytest, name))
            .transpose()?
            .flatten();
        return Ok(PytestConfigMatch::Matched(addopts));
    }

    if name == "pyproject.toml" {
        let document =
            toml::from_str::<toml::Value>(contents).context("failed to parse pyproject.toml")?;
        let Some(tool) = document.get("tool") else {
            return Ok(PytestConfigMatch::NotMatched);
        };
        let tool = table_at(tool, "tool", name)?;
        let Some(pytest) = tool.get("pytest") else {
            return Ok(PytestConfigMatch::NotMatched);
        };
        let pytest = table_at(pytest, "tool.pytest", name)?;
        let ini = pytest
            .get("ini_options")
            .map(|ini| table_at(ini, "tool.pytest.ini_options", name))
            .transpose()?;

        if dialect != PytestConfigDialect::Nine {
            return Ok(match ini {
                Some(ini) => PytestConfigMatch::Matched(optional_toml_addopts(ini, name)?),
                None => PytestConfigMatch::NotMatched,
            });
        }

        let native_has_values = pytest.keys().any(|key| key != "ini_options");
        if native_has_values && ini.is_some() {
            anyhow::bail!(
                "pyproject.toml defines both [tool.pytest] and [tool.pytest.ini_options]"
            );
        }
        return Ok(if native_has_values {
            PytestConfigMatch::Matched(optional_toml_addopts(pytest, name)?)
        } else if let Some(ini) = ini {
            PytestConfigMatch::Matched(optional_toml_addopts(ini, name)?)
        } else {
            // An empty native table is not a pytest configuration; pytest 9
            // continues discovery and may later use this file only as fallback.
            PytestConfigMatch::NotMatched
        });
    }

    let document = parse_pytest_ini(name, contents)?;
    if name == "setup.cfg" {
        if let Some(options) = document.get("tool:pytest") {
            return Ok(PytestConfigMatch::Matched(
                options
                    .get("addopts")
                    .map(|value| split_pytest_addopts(value))
                    .transpose()
                    .with_context(|| format!("invalid addopts in {name}"))?,
            ));
        }
        if document.contains_key("pytest") {
            anyhow::bail!("setup.cfg uses unsupported [pytest]; rename it to [tool:pytest]");
        }
        return Ok(PytestConfigMatch::NotMatched);
    }

    let dedicated =
        name == "pytest.ini" || (name == ".pytest.ini" && dialect == PytestConfigDialect::Nine);
    let Some(options) = document.get("pytest") else {
        return Ok(if dedicated {
            PytestConfigMatch::Matched(None)
        } else {
            PytestConfigMatch::NotMatched
        });
    };
    Ok(PytestConfigMatch::Matched(
        options
            .get("addopts")
            .map(|value| split_pytest_addopts(value))
            .transpose()
            .with_context(|| format!("invalid addopts in {name}"))?,
    ))
}

fn empty_pytest_config() -> PathBuf {
    #[cfg(windows)]
    let path = PathBuf::from("NUL");
    #[cfg(not(windows))]
    let path = PathBuf::from("/dev/null");
    path
}

fn read_pytest_config(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::symlink_metadata(path) {
                Ok(metadata) if !metadata.is_file() => Ok(None),
                Err(metadata_error) if metadata_error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(None)
                }
                _ => Err(error).with_context(|| {
                    format!("pytest config {} exists but cannot be read", path.display())
                }),
            };
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "pytest config {} exists but cannot be inspected",
                    path.display()
                )
            });
        }
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("pytest config {} exists but cannot be read", path.display())
            });
        }
    };
    String::from_utf8(bytes)
        .map(Some)
        .with_context(|| format!("pytest config {} is not valid UTF-8", path.display()))
}

fn selected_pytest_config(
    root: &Path,
    dialect: PytestConfigDialect,
) -> Result<(PathBuf, Option<Vec<String>>)> {
    let names = match dialect {
        PytestConfigDialect::LegacyPreHidden => PYTEST_PRE_HIDDEN_CONFIG_FILES,
        PytestConfigDialect::LegacyHidden | PytestConfigDialect::Legacy => {
            PYTEST_LEGACY_CONFIG_FILES
        }
        PytestConfigDialect::Nine => PYTEST_NINE_CONFIG_FILES,
    };
    let mut pyproject_fallback = None;
    for name in names {
        let path = root.join(name);
        let Some(contents) = read_pytest_config(&path)? else {
            continue;
        };
        if *name == "pyproject.toml" {
            pyproject_fallback = Some(path.clone());
        }
        match pytest_config_addopts(name, &contents, dialect)? {
            PytestConfigMatch::NotMatched => {}
            PytestConfigMatch::Matched(addopts) => return Ok((path, addopts)),
        }
    }

    if matches!(
        dialect,
        PytestConfigDialect::Legacy | PytestConfigDialect::Nine
    ) && let Some(pyproject) = pyproject_fallback
    {
        return Ok((pyproject, None));
    }

    // An explicit empty config prevents pytest from walking above the reviewed
    // tree and inheriting an ambient parent project's addopts. These are the
    // platform null devices, so no checkout file needs to be created.
    Ok((empty_pytest_config(), None))
}

fn bounded_pytest_invocation_with_auto_workers(
    root: &Path,
    base_env: &[(String, String)],
    worker_limit: u32,
    inherited_addopts: Option<&str>,
    inherited_auto_workers: Option<&str>,
    dialect: PytestConfigDialect,
) -> Result<PytestInvocation> {
    let worker_limit = worker_limit.max(1);
    let (config_path, config_addopts) = selected_pytest_config(root, dialect)?;
    let inherited_addopts = inherited_addopts
        .map(split_pytest_addopts)
        .transpose()
        .context("invalid PYTEST_ADDOPTS")?;
    if let Some(config_addopts) = &config_addopts {
        validate_pytest_addopts_safety(config_addopts, "pytest config addopts")?;
    }
    if let Some(inherited_addopts) = &inherited_addopts {
        validate_pytest_addopts_safety(inherited_addopts, "PYTEST_ADDOPTS")?;
    }
    let mut args = vec![
        "-v".to_owned(),
        "-c".to_owned(),
        config_path.display().to_string(),
        "--rootdir".to_owned(),
        root.display().to_string(),
    ];
    let config_disposition = config_addopts
        .as_deref()
        .map(xdist_worker_disposition)
        .transpose()?
        .unwrap_or(XdistWorkerDisposition::Absent);
    let inherited_disposition = inherited_addopts
        .as_deref()
        .map(xdist_worker_disposition)
        .transpose()?
        .unwrap_or(XdistWorkerDisposition::Absent);
    let effective_disposition = match &inherited_disposition {
        XdistWorkerDisposition::Absent => &config_disposition,
        _ => &inherited_disposition,
    };
    if matches!(
        effective_disposition,
        XdistWorkerDisposition::Requested(request)
            if xdist_request_exceeds_limit(request, worker_limit)
    ) {
        // Pytest prepends ini/PYTEST_ADDOPTS before explicit CLI arguments, so
        // this final option clamps both `-n auto` and an explicit `-n N`.
        args.extend(["-n".to_owned(), worker_limit.to_string()]);
    }
    let mut env = base_env.to_vec();
    env.retain(|(key, _)| key != "PYTEST_XDIST_AUTO_NUM_WORKERS");
    let auto_workers = inherited_auto_workers
        .map(normalize_xdist_auto_workers)
        .transpose()?
        .map(|workers| {
            if xdist_request_exceeds_limit(
                &XdistWorkerRequest::Count(workers.clone()),
                worker_limit,
            ) {
                worker_limit.to_string()
            } else {
                workers
            }
        })
        .unwrap_or_else(|| worker_limit.to_string());
    env.push(("PYTEST_XDIST_AUTO_NUM_WORKERS".to_owned(), auto_workers));
    Ok((args, env))
}

#[cfg(test)]
fn bounded_pytest_invocation(
    root: &Path,
    base_env: &[(String, String)],
    worker_limit: u32,
    inherited_addopts: Option<&str>,
    dialect: PytestConfigDialect,
) -> Result<PytestInvocation> {
    bounded_pytest_invocation_with_auto_workers(
        root,
        base_env,
        worker_limit,
        inherited_addopts,
        None,
        dialect,
    )
}

fn parse_pytest_version(value: &str) -> Result<(PytestConfigDialect, String)> {
    let version = value
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("pytest "))
        .and_then(|suffix| suffix.split_whitespace().next())
        .map(|token| {
            token
                .trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.')
                .to_owned()
        })
        .filter(|version| !version.is_empty())
        .ok_or_else(|| anyhow::anyhow!("pytest --version did not report a parseable version"))?;
    let major = version
        .split('.')
        .next()
        .and_then(|major| major.parse::<u64>().ok())
        .filter(|major| *major > 0)
        .ok_or_else(|| anyhow::anyhow!("pytest --version reported invalid version {version:?}"))?;
    let minor = version
        .split('.')
        .nth(1)
        .map(|minor| {
            minor
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .filter(|minor| !minor.is_empty())
        .and_then(|minor| minor.parse::<u64>().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("pytest --version did not report a usable minor in {version:?}")
        })?;
    let dialect = match (major, minor) {
        (6, _) | (7, 0..=1) => PytestConfigDialect::LegacyPreHidden,
        (7, minor) if minor >= 2 => PytestConfigDialect::LegacyHidden,
        (8, 0) => PytestConfigDialect::LegacyHidden,
        (8, minor) if minor >= 1 => PytestConfigDialect::Legacy,
        (9, _) => PytestConfigDialect::Nine,
        _ => anyhow::bail!("pytest {version} uses an unsupported config discovery dialect"),
    };
    Ok((dialect, version))
}

/// Ask the exact pytest launcher used by the check which discovery dialect it
/// implements. The null config, explicit root and scrubbed addopts/plugin env
/// keep ambient parents and plugins out of this version oracle.
async fn probe_pytest_runtime(
    root: &Path,
    base_env: &[(String, String)],
    use_uv: bool,
) -> Result<(PytestConfigDialect, String)> {
    let null_config = empty_pytest_config().display().to_string();
    let root_dir = root.display().to_string();
    let mut args = Vec::new();
    if use_uv {
        args.extend(["run".to_owned(), "pytest".to_owned()]);
    }
    args.extend([
        "-c".to_owned(),
        null_config,
        "--rootdir".to_owned(),
        root_dir,
        "--version".to_owned(),
    ]);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let env = pytest_probe_env(base_env);
    let command = if use_uv { "uv" } else { "pytest" };
    let output = run_command_with_timeout_and_env(command, &arg_refs, root, 30, &env)
        .await
        .with_context(|| format!("failed to run isolated {command} pytest version probe"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let evidence = format!("{stdout}\n{stderr}");
    if !output.status.success() {
        anyhow::bail!(
            "isolated pytest version probe failed with exit {:?}: {}",
            output.status.code(),
            evidence.trim()
        );
    }
    parse_pytest_version(&evidence).with_context(|| {
        format!(
            "cannot select a pytest config dialect from probe output: {}",
            evidence.trim()
        )
    })
}

fn pytest_probe_env(base_env: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = base_env.to_vec();
    env.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "PYTEST_ADDOPTS" | "PYTEST_DISABLE_PLUGIN_AUTOLOAD" | "PYTEST_PLUGINS"
        )
    });
    env.extend([
        ("PYTEST_ADDOPTS".to_owned(), String::new()),
        ("PYTEST_DISABLE_PLUGIN_AUTOLOAD".to_owned(), "1".to_owned()),
        ("PYTEST_PLUGINS".to_owned(), String::new()),
    ]);
    env
}

/// Refuse project metadata that resolves outside the tree being judged.
///
/// The tools read the FILES, not the directory. A reviewed commit that replaces
/// `pyproject.toml` with a link to an external manifest has uv discover another
/// project — its configuration, its dependency set, its lockfile — while
/// provenance records an exact `snapshot` scan and the cache stores the result
/// under the reviewed commit. `uv run` is given neither `--no-project` nor
/// `--locked`, so nothing downstream re-asks the question.
///
/// Applied to the local checkout as well, for the same reason the Cargo guards
/// are ([`super::cargo`]): a working tree that tracks its metadata as a link to
/// somewhere else earns another project's verdict just as effectively.
///
/// Symlinks are not the target — escape is. Metadata linked to a real file
/// INSIDE the tree resolves back inside and passes. A path that cannot be
/// canonicalised is simply not there: the tools reporting a missing project is a
/// truthful failure of this tree, not a foreign one's verdict.
fn metadata_stays_in_project(
    root: &Path,
    ignore_discovered_uv_config: bool,
    use_uv: bool,
) -> Result<()> {
    let Ok(resolved_root) = root.canonicalize() else {
        return Ok(());
    };
    for name in PYTHON_PROJECT_METADATA {
        if !use_uv && matches!(*name, "uv.toml" | "uv.lock") {
            // Direct Ruff/Mypy/Pytest do not consume uv's configuration or
            // lockfile. Their presence cannot affect this invocation.
            continue;
        }
        if ignore_discovered_uv_config && *name == "uv.toml" {
            // UV_CONFIG_FILE replaces discovery of uv.toml, while
            // UV_NO_CONFIG disables discovery altogether. An explicit file was
            // already resolved and contained by checked_uv_environment_with.
            continue;
        }
        let path = root.join(name);
        let Ok(resolved) = path.canonicalize() else {
            continue;
        };
        if !resolved.starts_with(&resolved_root) {
            anyhow::bail!(
                "{} resolves outside the tree under review ({}), so a verdict earned there would \
                 describe another project",
                path.display(),
                root.display(),
            );
        }
    }
    Ok(())
}

/// Mirror uv's boolish environment parser for `--no-config`.
///
/// This must be decided before reading project-owned uv configuration: when the
/// flag is enabled, uv ignores discovered `uv.toml` and `[tool.uv]` settings.
/// Invalid values stay loud because uv would reject the invocation as well.
fn uv_no_config_enabled(inherited: &mut impl FnMut(&str) -> Option<OsString>) -> Result<bool> {
    let Some(value) = inherited("UV_NO_CONFIG") else {
        return Ok(false);
    };
    let Some(value) = value.to_str() else {
        anyhow::bail!("UV_NO_CONFIG is not valid UTF-8, so its uv boolean value is unknown");
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "on" | "1" => Ok(true),
        "false" | "f" | "no" | "n" | "off" | "0" => Ok(false),
        _ => anyhow::bail!(
            "UV_NO_CONFIG must be a boolean understood by uv (true/false, yes/no, on/off, 1/0), got {value:?}"
        ),
    }
}

/// Resolve uv's environment-owned path selectors before a command can replace
/// the reviewed substrate. A path is resolved from uv's exact command cwd. An
/// explicit config may name any file inside the tree; project/cwd redirects
/// must resolve to the root itself because even a nested project would change
/// which dependencies and source `uv run ... .` judges.
fn checked_uv_environment_with(
    root: &Path,
    inherited: &mut impl FnMut(&str) -> Option<OsString>,
) -> Result<Option<PathBuf>> {
    let resolved_root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve reviewed Python root {}", root.display()))?;

    let explicit_config = inherited("UV_CONFIG_FILE")
        .map(|value| resolve_uv_environment_path(root, &resolved_root, "UV_CONFIG_FILE", value))
        .transpose()?;
    if let Some(path) = explicit_config.as_ref()
        && !path.starts_with(&resolved_root)
    {
        anyhow::bail!(
            "UV_CONFIG_FILE resolves outside the tree under review ({}), so uv would read foreign configuration",
            path.display(),
        );
    }

    for key in ["UV_PROJECT", "UV_WORKING_DIR", "UV_WORKING_DIRECTORY"] {
        let Some(value) = inherited(key) else {
            continue;
        };
        let path = resolve_uv_environment_path(root, &resolved_root, key, value)?;
        if path != resolved_root {
            anyhow::bail!(
                "{key} resolves to {} instead of the exact reviewed Python root {}, so uv would judge another substrate",
                path.display(),
                resolved_root.display(),
            );
        }
    }

    Ok(explicit_config)
}

fn resolve_uv_environment_path(
    root: &Path,
    resolved_root: &Path,
    key: &str,
    value: OsString,
) -> Result<PathBuf> {
    if value.is_empty() {
        anyhow::bail!("{key} is empty, so its uv path authority is ambiguous");
    }
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.canonicalize().with_context(|| {
        format!(
            "{key} points to {} which cannot be resolved from the reviewed Python root {}",
            path.display(),
            resolved_root.display(),
        )
    })
}

/// Read only the project-owned scalar settings prview is about to override.
/// uv.toml wins wholesale over `[tool.uv]` in pyproject.toml; UV_CONFIG_FILE,
/// when present and contained, replaces both discovered authorities. With
/// UV_NO_CONFIG, neither discovered authority participates, while an explicit
/// UV_CONFIG_FILE remains authoritative just as it does for uv itself.
fn project_uv_concurrency_limits(
    root: &Path,
    explicit_config: Option<&Path>,
    no_discovered_config: bool,
) -> Result<super::UvConcurrencyLimits> {
    if let Some(path) = explicit_config {
        return uv_concurrency_limits_from_file(path, false);
    }
    if no_discovered_config {
        return Ok(super::UvConcurrencyLimits::default());
    }

    let uv_toml = root.join("uv.toml");
    if uv_toml
        .try_exists()
        .with_context(|| format!("cannot inspect {}", uv_toml.display()))?
    {
        return uv_concurrency_limits_from_file(&uv_toml, false);
    }

    let pyproject = root.join("pyproject.toml");
    if pyproject
        .try_exists()
        .with_context(|| format!("cannot inspect {}", pyproject.display()))?
    {
        return uv_concurrency_limits_from_file(&pyproject, true);
    }
    Ok(super::UvConcurrencyLimits::default())
}

fn uv_concurrency_limits_from_file(
    path: &Path,
    embedded_in_pyproject: bool,
) -> Result<super::UvConcurrencyLimits> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read uv configuration {}", path.display()))?;
    let contents = std::str::from_utf8(&bytes)
        .with_context(|| format!("uv configuration {} is not UTF-8", path.display()))?;
    let parsed = toml::from_str::<toml::Value>(contents)
        .with_context(|| format!("failed to parse uv configuration {}", path.display()))?;

    let table = if embedded_in_pyproject {
        let Some(tool) = parsed.get("tool") else {
            return Ok(super::UvConcurrencyLimits::default());
        };
        let Some(tool) = tool.as_table() else {
            anyhow::bail!("{} has a non-table [tool] authority", path.display());
        };
        let Some(uv) = tool.get("uv") else {
            return Ok(super::UvConcurrencyLimits::default());
        };
        uv.as_table().ok_or_else(|| {
            anyhow::anyhow!("{} has a non-table [tool.uv] authority", path.display())
        })?
    } else {
        parsed
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("{} is not a uv configuration table", path.display()))?
    };

    Ok(super::UvConcurrencyLimits {
        downloads: positive_uv_concurrency_limit(table, "concurrent-downloads", path)?,
        builds: positive_uv_concurrency_limit(table, "concurrent-builds", path)?,
        installs: positive_uv_concurrency_limit(table, "concurrent-installs", path)?,
    })
}

fn positive_uv_concurrency_limit(
    table: &toml::value::Table,
    key: &str,
    path: &Path,
) -> Result<Option<u32>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(integer) = value.as_integer() else {
        anyhow::bail!(
            "{key} in {} must be a positive integer, got {value}",
            path.display(),
        );
    };
    let limit = u32::try_from(integer)
        .ok()
        .filter(|limit| *limit > 0)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{key} in {} must be a positive integer, got {value}",
                path.display(),
            )
        })?;
    Ok(Some(limit))
}

/// Name of the environment for the substrate this run analyses.
///
/// The reviewed commit IS the dependency set, so it names the environment. When
/// no off-`HEAD` commit resolves while the scan still happens elsewhere (an
/// injected scan dir), the snapshot path stands in: unknown provenance must not
/// collapse two different substrates onto one environment.
fn reviewed_env_token(config: &Config, scan_dir: &Path) -> String {
    off_head_target_commit(config)
        .unwrap_or_else(|| format!("snapshot-{}", cache::key_token(&scan_dir.to_string_lossy())))
}

/// Environments kept regardless of age — the working set of a repo under review.
const UV_ENVS_KEPT: usize = 3;

/// How long an environment is untouchable after its last use. A review does not
/// run for a day, so anything older cannot belong to a live run.
const UV_ENV_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Marker refreshed on every use, so reuse (which only writes deep inside the
/// environment) still counts as recent activity.
const UV_ENV_USED_MARKER: &str = ".prview-used";

/// Serialises marking and pruning across processes. A plain file, so
/// [`prune_uv_envs`]'s directory filter passes over it.
const UV_PRUNE_LOCK: &str = ".prview-prune.lock";

/// Record this environment as used and drop the ones that are neither recent nor
/// part of the working set.
///
/// Per-commit isolation trades one directory per repository for one per reviewed
/// commit, so without a bound a busy repository would leave a virtualenv behind
/// for every commit ever reviewed — hundreds of megabytes each. The bound is
/// deliberately timid: the newest [`UV_ENVS_KEPT`] survive whatever their age,
/// and nothing used within [`UV_ENV_MIN_AGE`] is touched, so a concurrent (or
/// merely slow) review cannot have its environment deleted underneath it.
///
/// Age alone does not make the bound safe, because two reviews run
/// concurrently: one process can read an environment's timestamp just before
/// another refreshes it, and then delete the directory once that other process
/// has already started `uv run`. Marking and pruning are therefore one critical
/// section, serialised across processes by [`UV_PRUNE_LOCK`] — no other prview
/// can observe this root between our mark and our sweep.
///
/// The lock is opportunistic. Pruning is housekeeping, so a root already locked
/// by a live review is simply left to that review: we still record OUR use,
/// which is what protects this environment from the next sweep, and skip the
/// sweep itself. (`prune_uv_envs` re-reads each candidate immediately before
/// removing it, so a mark that lands outside the lock still wins.)
///
/// That leaves one window open by choice: a review that could not take the lock
/// marks OUTSIDE it, so its mark can land between the sweeper's final re-read
/// and its `remove_dir_all`. Closing it means marking under the lock, which
/// turns every off-`HEAD` Python review into a waiter on another process's
/// housekeeping. The trade is not worth it here: the window is the width of one
/// `remove_dir_all` call, it only opens for an environment simultaneously idle
/// for a day, outside the working set, and being started right now — and its
/// consequence is a loud `uv` failure on one gate, never a verdict attributed to
/// the wrong substrate. If that failure is ever actually seen, marking under a
/// bounded-wait acquisition is the fix.
///
/// Nothing is created here: an absent root means no environment exists yet, and
/// pre-creating the directory would leave uv an empty non-environment to reject.
fn mark_and_prune_uv_envs(root: &Path, env_dir: &Path) {
    if !root.is_dir() {
        return;
    }
    let lock = crate::storage::acquire_lock_at(&root.join(UV_PRUNE_LOCK)).ok();
    if env_dir.is_dir() {
        let _ = std::fs::write(env_dir.join(UV_ENV_USED_MARKER), b"");
    }
    if lock.is_some() {
        prune_uv_envs(root, UV_ENVS_KEPT, UV_ENV_MIN_AGE);
    }
    drop(lock);
}

/// Pure half of [`mark_and_prune_uv_envs`]: remove environments beyond the
/// `keep` most recently used that have also been idle for at least `min_age`.
fn prune_uv_envs(root: &Path, keep: usize, min_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut envs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| (last_used(&p), p))
        .collect();

    // Newest first, so the tail is what the working set does not cover.
    envs.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
    let now = std::time::SystemTime::now();
    let idle_for = |used| now.duration_since(used).unwrap_or_default();
    for (used, path) in envs.into_iter().skip(keep) {
        if idle_for(used) < min_age {
            continue;
        }
        // Re-read immediately before deleting. The listing above is a snapshot,
        // and a review that could not take the prune lock still marks the
        // environment it is about to use; that mark must beat a verdict formed
        // from a stat taken before it landed.
        if idle_for(last_used(&path)) < min_age {
            continue;
        }
        let _ = std::fs::remove_dir_all(path);
    }
}

/// When an environment was last used: the marker if this prview wrote one, the
/// directory's own timestamp otherwise (an environment from an older prview, or
/// one created but never reused).
fn last_used(env_dir: &Path) -> std::time::SystemTime {
    let marker = env_dir.join(UV_ENV_USED_MARKER);
    std::fs::metadata(&marker)
        .or_else(|_| std::fs::metadata(env_dir))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// Classify a ruff run from its exit status and combined output.
///
/// A missing tool is a setup gap, not a lint failure. When uv wraps a ruff that
/// is not installed it emits "error: Failed to spawn: `ruff`" with a non-zero
/// exit; that must classify as Skipped (mirroring [`mypy_status`], PR #1
/// b1697d4) rather than a lint Failed that would falsely dent the gate in every
/// Python repo without ruff. A genuine non-zero exit with lint findings stays
/// Failed.
fn ruff_status(success: bool, combined: &str) -> CheckStatus {
    if success {
        CheckStatus::Passed
    } else if tool_spawn_failure_in_output(combined) {
        CheckStatus::Skipped
    } else {
        CheckStatus::Failed
    }
}

#[async_trait]
impl Check for RuffCheck {
    fn name(&self) -> &str {
        "Ruff"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.runs_python_checks() {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if let Some(reason) = missing_reviewed_python_project(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // Ruff also depends on dedicated config, CLI policy and the installed
        // environment. Replaying a python-source-only key can produce a stale
        // PASS, so persistent replay stays off until that proof is complete.
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let use_uv = which::which("uv").is_ok();
        let plan = plan_python_tool_run(config, use_uv)?;
        let run_dir = &plan.cwd;
        let output = if use_uv {
            run_command_with_env("uv", &["run", "ruff", "check", "."], run_dir, &plan.env).await?
        } else {
            run_command_with_env("ruff", &["check", "."], run_dir, &plan.env).await?
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = ruff_status(output.status.success(), &combined);

        let cmd_str = if use_uv {
            "uv run ruff check ."
        } else {
            "ruff check ."
        };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: cmd_str.to_string(),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

/// Classify a mypy run from its exit status and combined output.
///
/// A missing tool is a setup gap, not a type error: uv emits
/// "error: Failed to spawn: `mypy` / No such file or directory" when mypy is
/// not installed, which would otherwise be misread as a type error -> Skipped.
fn mypy_status(success: bool, combined: &str) -> CheckStatus {
    if success {
        CheckStatus::Passed
    } else if tool_spawn_failure_in_output(combined) {
        // uv emits "error: Failed to spawn: `mypy`" when mypy is not installed.
        // Match only that unambiguous launcher marker — never a bare "no such
        // file or directory", which mypy itself prints in real diagnostics
        // (matching it would turn a genuine failure into an invisible pass).
        CheckStatus::Skipped
    } else if combined.contains("error:") {
        CheckStatus::Failed
    } else {
        CheckStatus::Warnings
    }
}

#[async_trait]
impl Check for MypyCheck {
    fn name(&self) -> &str {
        "Mypy"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.runs_python_checks() {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if let Some(reason) = missing_reviewed_python_project(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if !config.run_lint {
            return super::CheckEligibility::Skip("lint disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    fn cache_key(&self, _config: &Config) -> Option<String> {
        // Mypy consumes config and environment state not covered by the old
        // python hash. An incomplete key is not an equivalence proof.
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        let use_uv = which::which("uv").is_ok();
        let plan = plan_python_tool_run(config, use_uv)?;
        let run_dir = &plan.cwd;
        let output = if use_uv {
            run_command_with_env("uv", &["run", "mypy", "."], run_dir, &plan.env).await?
        } else {
            run_command_with_env("mypy", &["."], run_dir, &plan.env).await?
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = mypy_status(output.status.success(), &combined);

        let cmd_str = if use_uv { "uv run mypy ." } else { "mypy ." };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: cmd_str.to_string(),
                    tool_version: None,
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

#[async_trait]
impl Check for PytestCheck {
    fn name(&self) -> &str {
        "Pytest"
    }

    fn check_eligibility(&self, config: &Config) -> super::CheckEligibility {
        if !config.profile.runs_python_checks() {
            return super::CheckEligibility::Skip(format!(
                "profile {}",
                config.profile.kind.as_str().to_lowercase()
            ));
        }
        if let Some(reason) = missing_reviewed_python_project(config) {
            return super::CheckEligibility::Skip(reason);
        }
        if config.is_fast_remote_only_standard() && !config.run_tests {
            return super::CheckEligibility::Skip("fast remote-only preset".to_string());
        }
        if !config.run_tests {
            return super::CheckEligibility::Skip("tests disabled".to_string());
        }
        super::CheckEligibility::Run
    }

    // Tests are not cached - they should always run fresh
    fn cache_key(&self, _config: &Config) -> Option<String> {
        None
    }

    async fn run(&self, config: &Config) -> Result<CheckResult> {
        let start = std::time::Instant::now();
        let started_at = Local::now().to_rfc3339();

        // Run from the reviewed substrate, not the local checkout: with a PR or
        // remote target, `config.repo_root` still holds whatever branch happens
        // to be checked out locally, so pytest would report a foreign branch's
        // failures against the PR (PRV-PYTEST-HEAD). Ruff, Mypy and the sibling
        // test runner Vitest all resolve their cwd through `plan_check_run`;
        // Pytest was the sole outlier. For a local review the plan resolves back
        // to `repo_root`, so that path is unchanged.
        let use_uv = which::which("uv").is_ok();
        let plan = plan_python_tool_run(config, use_uv)?;
        let run_dir = &plan.cwd;
        let (pytest_dialect, pytest_version) =
            probe_pytest_runtime(run_dir, &plan.env, use_uv).await?;
        let inherited_addopts = checked_pytest_addopts(std::env::var("PYTEST_ADDOPTS"))?;
        let inherited_auto_workers =
            checked_xdist_auto_workers(std::env::var("PYTEST_XDIST_AUTO_NUM_WORKERS"))?;
        let (pytest_args, pytest_env) = bounded_pytest_invocation_with_auto_workers(
            run_dir,
            &plan.env,
            config.resource_plan.worker_limit,
            inherited_addopts.as_deref(),
            inherited_auto_workers.as_deref(),
            pytest_dialect,
        )?;
        let pytest_arg_refs: Vec<&str> = pytest_args.iter().map(String::as_str).collect();

        let output = if use_uv {
            let mut uv_args = vec!["run", "pytest"];
            uv_args.extend(pytest_arg_refs.iter().copied());
            run_command_with_timeout_and_env(
                "uv",
                &uv_args,
                run_dir,
                TEST_TIMEOUT_SECS,
                &pytest_env,
            )
            .await?
        } else {
            run_command_with_timeout_and_env(
                "pytest",
                &pytest_arg_refs,
                run_dir,
                TEST_TIMEOUT_SECS,
                &pytest_env,
            )
            .await?
        };
        let finished_at = Local::now().to_rfc3339();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}\n{}", stdout, stderr);

        let status = if output.status.success() {
            CheckStatus::Passed
        } else {
            CheckStatus::Failed
        };

        let pytest_command = pytest_args.join(" ");
        let cmd_str = if use_uv {
            format!("uv run pytest {pytest_command}")
        } else {
            format!("pytest {pytest_command}")
        };
        Ok(CheckResult {
            name: self.name().to_string(),
            status,
            duration: start.elapsed(),
            output: combined.clone(),
            cached: false,
            provenance: Some(
                CheckProvenance {
                    command: cmd_str,
                    tool_version: Some(pytest_version),
                    cwd: run_dir.display().to_string(),
                    exit_code: output.status.code(),
                    started_at,
                    finished_at,
                    hard_fail_signatures: find_hard_fail_signatures(&combined),
                    cache_key: self.cache_key(config),
                    target_sha: None,
                    tree_state: None,
                }
                .with_scan_substrate(self.name(), run_dir, &config.repo_root),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{test_config_builder, test_python_profile};

    fn create_test_config(has_pyproject: bool, run_lint: bool, run_tests: bool) -> Config {
        test_config_builder()
            .profile(test_python_profile(has_pyproject))
            .run_lint(run_lint)
            .run_tests(run_tests)
            .do_fetch(false)
            .use_cache(false)
            .create_zip(false)
            .build()
    }

    fn planned_env_value<'a>(run: &'a PythonRun, key: &str) -> &'a str {
        run.env
            .iter()
            .find_map(|(actual, value)| (actual == key).then_some(value.as_str()))
            .unwrap_or_else(|| panic!("missing {key} in Python run environment"))
    }

    /// Two commits: the reviewed one carries no Python at all, the checked-out
    /// one does. Returns (repo, reviewed commit).
    fn repo_whose_target_dropped_python() -> (tempfile::TempDir, String) {
        use crate::git::cmd::git_cmd;

        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let run_git = |args: &[&str]| {
            let out = git_cmd()
                .args(args)
                .current_dir(root)
                .output()
                .expect("git command");
            assert!(out.status.success(), "git {args:?} failed");
        };
        run_git(&["init", "-q", "-b", "main"]);
        run_git(&["config", "user.email", "prview@example.test"]);
        run_git(&["config", "user.name", "prview test"]);
        run_git(&["config", "commit.gpgsign", "false"]);

        // Reviewed commit: a pure Rust tree, no Python whatsoever.
        std::fs::write(root.join("README.md"), "rust only\n").expect("write");
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "no python here"]);
        let target = String::from_utf8(
            git_cmd()
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .expect("utf8")
        .trim()
        .to_string();

        // Checked-out commit: the Python project the local profile detects.
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"x\"\n").expect("write");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::write(root.join("src/app.py"), "def main():\n    pass\n").expect("write");
        run_git(&["add", "-A"]);
        run_git(&["commit", "-q", "-m", "add python"]);

        (tmp, target)
    }

    /// A target that is not a Python project must not be judged by Python
    /// checks. Pytest is the sharp edge: it exits 5 for "no tests collected",
    /// and that blocking failure was attributed to a target the check does not
    /// apply to at all.
    #[test]
    fn python_checks_do_not_run_against_a_target_without_python() {
        let (repo, target) = repo_whose_target_dropped_python();
        let mut config = create_test_config(true, true, true);
        config.repo_root = repo.path().to_path_buf();
        config.target = Some(target);

        let reason = missing_reviewed_python_project(&config)
            .expect("a target with no Python is not a Python project");
        assert!(
            reason.contains("not a Python project"),
            "the skip must say why: {reason}",
        );

        for eligibility in [
            PytestCheck.check_eligibility(&config),
            RuffCheck.check_eligibility(&config),
            MypyCheck.check_eligibility(&config),
        ] {
            assert_eq!(
                eligibility,
                super::super::CheckEligibility::Skip(reason.clone()),
                "every Python check must skip with the reviewed-tree reason",
            );
        }
    }

    /// The guard must not manufacture skips: a target that still carries Python
    /// keeps running, and so does a local review, where the checkout IS the
    /// target and git is never consulted.
    #[test]
    fn a_target_that_still_has_python_keeps_running() {
        let (repo, _dropped) = repo_whose_target_dropped_python();
        let mut config = create_test_config(true, true, true);
        config.repo_root = repo.path().to_path_buf();

        // HEAD carries the Python project, so a review of it is not off-HEAD at
        // all and the guard stays out of the way.
        assert_eq!(missing_reviewed_python_project(&config), None);
        assert_eq!(
            PytestCheck.check_eligibility(&config),
            super::super::CheckEligibility::Run,
        );

        config.target = Some("main".to_string());
        assert_eq!(missing_reviewed_python_project(&config), None);
    }

    #[test]
    fn test_ruff_check_name() {
        let check = RuffCheck;
        assert_eq!(check.name(), "Ruff");
    }

    /// A review must never write into the operator's environment. The snapshot
    /// symlinks their `.venv`, and `uv run` syncs the project environment before
    /// executing — so an off-HEAD python check has to be pointed at a
    /// prview-owned environment instead of the symlinked one.
    #[test]
    fn python_run_off_head_isolates_uv_from_the_operator_environment() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let run = plan_python_run(&config).expect("plan");

        assert_eq!(run.cwd, scan_dir.path());
        let expected_env = config
            .uv_env_dir_for(&reviewed_env_token(&config, scan_dir.path()))
            .display()
            .to_string();
        assert!(
            run.env
                .iter()
                .any(|(key, value)| { key == "UV_PROJECT_ENVIRONMENT" && value == &expected_env })
        );
        for key in [
            "UV_CONCURRENT_DOWNLOADS",
            "UV_CONCURRENT_BUILDS",
            "UV_CONCURRENT_INSTALLS",
            "CARGO_BUILD_JOBS",
        ] {
            assert!(
                run.env
                    .iter()
                    .any(|(actual, value)| actual == key && value == "1"),
                "{key} must cap the later uv run, not only pre-sync",
            );
        }
        let env_dir = PathBuf::from(expected_env);
        assert!(
            !env_dir.starts_with(repo_root.path()),
            "the reviewed sync must not reach the operator's checkout (its .venv is symlinked \
             into the snapshot)",
        );
        assert!(
            !env_dir.starts_with(scan_dir.path()),
            "an environment inside the throwaway snapshot is reinstalled on every run",
        );
        assert!(
            env_dir.starts_with(config.uv_env_root()),
            "the environment stays inside the repo's prview-owned root",
        );
    }

    /// uv reads the project metadata, not the directory. A reviewed commit that
    /// replaces project, lock, or pytest config with a link to an external file
    /// makes ruff, mypy and pytest configure themselves — and uv resolve the
    /// dependency set — from a project this review does not describe, while the
    /// verdict is cached under the reviewed commit. Same refusal as the Cargo
    /// manifest guard.
    #[test]
    #[cfg(unix)]
    fn python_metadata_linked_out_of_the_snapshot_is_refused() {
        for name in [
            "pyproject.toml",
            "uv.toml",
            "uv.lock",
            "pytest.ini",
            "pytest.toml",
        ] {
            let repo_root = tempfile::tempdir().expect("repo_root tempdir");
            let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
            let outside = tempfile::tempdir().expect("outside tempdir");

            let foreign = outside.path().join(name);
            std::fs::write(&foreign, "[project]\nname = \"someone-else\"\n")
                .expect("write foreign");
            std::os::unix::fs::symlink(&foreign, scan_dir.path().join(name)).expect("symlink");

            let mut config = create_test_config(true, true, true);
            config.repo_root = repo_root.path().to_path_buf();
            config.scan_dir_override = Some(scan_dir.path().to_path_buf());

            let Err(err) = plan_python_run(&config) else {
                panic!("{name} resolving outside the snapshot must not earn a verdict");
            };
            let message = err.to_string();
            assert!(
                message.contains(name),
                "the refusal must name the file that escapes: {message}",
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn git_backed_uv_toml_linked_out_of_an_off_head_snapshot_is_refused() {
        let home = tempfile::tempdir().expect("prview home");
        let _home = crate::config::override_test_prview_home(home.path().to_path_buf());
        let repo = tempfile::tempdir().expect("repo");
        let outside = tempfile::tempdir().expect("outside");
        let root = repo.path();
        run_git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\nversion = \"0.1.0\"\n",
        )
        .expect("pyproject");
        run_git(root, &["add", "pyproject.toml"]);
        run_git(
            root,
            &[
                "-c",
                "user.name=prview test",
                "-c",
                "user.email=prview@example.test",
                "commit",
                "-q",
                "-m",
                "python project",
            ],
        );

        let foreign = outside.path().join("foreign-uv.toml");
        std::fs::write(&foreign, "concurrent-builds = 1\n").expect("foreign config");
        std::os::unix::fs::symlink(&foreign, root.join("uv.toml")).expect("uv.toml symlink");
        run_git(root, &["add", "uv.toml"]);
        run_git(
            root,
            &[
                "-c",
                "user.name=prview test",
                "-c",
                "user.email=prview@example.test",
                "commit",
                "-q",
                "-m",
                "foreign uv config",
            ],
        );
        let target = String::from_utf8(
            crate::git::git_cmd()
                .args(["rev-parse", "HEAD"])
                .current_dir(root)
                .output()
                .expect("rev-parse")
                .stdout,
        )
        .expect("UTF-8 sha")
        .trim()
        .to_owned();

        std::fs::remove_file(root.join("uv.toml")).expect("remove symlink");
        write_commit(root, "uv.toml", "concurrent-builds = 2\n");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.to_path_buf();
        config.target = Some(target);
        let Err(error) = plan_python_run_with_env(&config, |_| None) else {
            panic!("the target's external uv.toml must not earn a verdict");
        };
        assert!(
            error.to_string().contains("uv.toml"),
            "the refusal must name the escaping authority: {error}",
        );
    }

    /// The guard is about escape, not about symlinks: metadata that resolves
    /// back inside the reviewed tree is the tree's own.
    #[test]
    #[cfg(unix)]
    fn python_metadata_inside_the_snapshot_is_accepted() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");

        let real = scan_dir.path().join("packaging/pyproject.toml");
        std::fs::create_dir_all(real.parent().expect("parent")).expect("packaging dir");
        std::fs::write(&real, "[project]\nname = \"reviewed\"\n").expect("write metadata");
        std::os::unix::fs::symlink(&real, scan_dir.path().join("pyproject.toml")).expect("symlink");

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        assert_eq!(plan_python_run(&config).expect("plan").cwd, scan_dir.path(),);
    }

    #[test]
    #[cfg(unix)]
    fn uv_toml_linked_inside_the_snapshot_is_accepted() {
        let root = tempfile::tempdir().expect("reviewed root");
        let config_dir = root.path().join("config");
        std::fs::create_dir(&config_dir).expect("config dir");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n",
        )
        .expect("pyproject");
        std::fs::write(config_dir.join("limits.toml"), "concurrent-builds = 1\n").expect("limits");
        std::os::unix::fs::symlink(config_dir.join("limits.toml"), root.path().join("uv.toml"))
            .expect("uv.toml symlink");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_run_with_env(&config, |_| None).expect("contained uv.toml");
        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_BUILDS"), "1");
    }

    #[test]
    fn uv_project_concurrency_obeys_authority_and_per_pool_precedence() {
        let root = tempfile::tempdir().expect("reviewed root");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n\n[tool.uv]\nconcurrent-downloads = 1\nconcurrent-builds = 1\nconcurrent-installs = 1\n",
        )
        .expect("pyproject");
        std::fs::write(
            root.path().join("uv.toml"),
            "concurrent-builds = 2\nconcurrent-installs = 8\n",
        )
        .expect("uv.toml");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_run_with_env(&config, |_| None).expect("project uv limits");

        assert_eq!(
            planned_env_value(&run, "UV_CONCURRENT_DOWNLOADS"),
            "4",
            "uv.toml wins wholesale; a missing key must not fall through to [tool.uv]",
        );
        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_BUILDS"), "2");
        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_INSTALLS"), "4");
        assert_eq!(
            planned_env_value(&run, "CARGO_BUILD_JOBS"),
            "4",
            "uv configuration does not own Cargo's backend pool",
        );
    }

    #[test]
    fn pyproject_tool_uv_concurrency_is_a_project_ceiling() {
        let root = tempfile::tempdir().expect("reviewed root");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n\n[tool.uv]\nconcurrent-downloads = 3\nconcurrent-builds = 1\nconcurrent-installs = 8\n",
        )
        .expect("pyproject");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_run_with_env(&config, |_| None).expect("[tool.uv] limits");

        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_DOWNLOADS"), "3");
        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_BUILDS"), "1");
        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_INSTALLS"), "4");
    }

    #[test]
    fn safe_explicit_uv_config_is_the_concurrency_authority() {
        let root = tempfile::tempdir().expect("reviewed root");
        std::fs::create_dir(root.path().join("config")).expect("config dir");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n\n[tool.uv]\nconcurrent-builds = 1\n",
        )
        .expect("pyproject");
        std::fs::write(root.path().join("uv.toml"), "concurrent-builds = 2\n").expect("uv.toml");
        std::fs::write(
            root.path().join("config/explicit.toml"),
            "concurrent-builds = 3\n",
        )
        .expect("explicit config");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_run_with_env(&config, |key| {
            (key == "UV_CONFIG_FILE").then(|| OsString::from("config/explicit.toml"))
        })
        .expect("contained explicit config");
        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_BUILDS"), "3");
    }

    #[test]
    #[cfg(unix)]
    fn safe_explicit_uv_config_replaces_discovered_uv_toml() {
        let root = tempfile::tempdir().expect("reviewed root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::create_dir(root.path().join("config")).expect("config dir");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n",
        )
        .expect("pyproject");
        std::fs::write(
            root.path().join("config/explicit.toml"),
            "concurrent-builds = 3\n",
        )
        .expect("explicit config");
        let foreign = outside.path().join("uv.toml");
        std::fs::write(&foreign, "concurrent-builds = 1\n").expect("foreign config");
        std::os::unix::fs::symlink(&foreign, root.path().join("uv.toml"))
            .expect("discovered uv.toml symlink");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_run_with_env(&config, |key| {
            (key == "UV_CONFIG_FILE").then(|| OsString::from("config/explicit.toml"))
        })
        .expect("safe explicit config replaces discovered uv.toml");

        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_BUILDS"), "3");
    }

    #[test]
    fn direct_python_tools_ignore_uv_only_configuration() {
        let root = tempfile::tempdir().expect("reviewed root");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n",
        )
        .expect("pyproject");
        std::fs::write(root.path().join("uv.toml"), "concurrent-builds = [\n")
            .expect("malformed uv.toml");
        std::fs::write(root.path().join("uv.lock"), b"\xff").expect("non-UTF-8 uv lock");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_tool_run_with_env(&config, false, |key| match key {
            "UV_NO_CONFIG" => Some(OsString::from("not-a-boolean")),
            "UV_CONFIG_FILE" => Some(OsString::from("missing-uv-config.toml")),
            "UV_CONCURRENT_BUILDS" => Some(OsString::from("not-a-limit")),
            "CARGO_BUILD_JOBS" => Some(OsString::from("2")),
            _ => None,
        })
        .expect("a direct tool does not consume uv-only authority");

        assert_eq!(run.cwd, root.path());
        assert_eq!(
            run.env,
            vec![("CARGO_BUILD_JOBS".to_owned(), "2".to_owned())]
        );
    }

    #[test]
    fn invalid_text_uv_no_config_is_loud() {
        let mut inherited =
            |key: &str| (key == "UV_NO_CONFIG").then(|| OsString::from("sometimes"));
        let error = uv_no_config_enabled(&mut inherited)
            .expect_err("uv rejects values outside its boolish vocabulary");

        assert!(
            error.to_string().contains("UV_NO_CONFIG") && error.to_string().contains("boolean"),
            "the refusal must identify the invalid uv boolean: {error}",
        );
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_uv_no_config_is_loud() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![0xff]);
        let mut inherited = |key: &str| (key == "UV_NO_CONFIG").then(|| invalid.clone());
        let error = uv_no_config_enabled(&mut inherited)
            .expect_err("uv cannot parse a non-UTF-8 boolean environment value");

        assert!(
            error.to_string().contains("UV_NO_CONFIG")
                && error.to_string().contains("not valid UTF-8"),
            "the refusal must identify the undecodable uv boolean: {error}",
        );
    }

    /// `UV_NO_CONFIG` is uv's environment spelling of `--no-config`: project
    /// configuration is not discovered, so prview must neither reject an
    /// ignored uv.toml symlink nor silently restore limits from `[tool.uv]`.
    #[test]
    #[cfg(unix)]
    fn uv_no_config_ignores_discovered_configuration_before_planning() {
        let root = tempfile::tempdir().expect("reviewed root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n\n[tool.uv]\nconcurrent-builds = 2\n",
        )
        .expect("pyproject");
        let foreign = outside.path().join("uv.toml");
        std::fs::write(&foreign, "concurrent-builds = 1\n").expect("foreign config");
        std::os::unix::fs::symlink(&foreign, root.path().join("uv.toml")).expect("uv.toml symlink");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_run_with_env(&config, |key| {
            (key == "UV_NO_CONFIG").then(|| OsString::from("yes"))
        })
        .expect("ignored discovered config must not participate in the plan");

        assert_eq!(
            planned_env_value(&run, "UV_CONCURRENT_BUILDS"),
            "4",
            "the run envelope, not ignored uv.toml or [tool.uv], is authoritative",
        );

        let Err(error) = plan_python_run_with_env(&config, |key| {
            (key == "UV_NO_CONFIG").then(|| OsString::from("false"))
        }) else {
            panic!("UV_NO_CONFIG=false must leave discovered uv.toml authoritative");
        };
        assert!(
            error.to_string().contains("uv.toml"),
            "the disabled flag must preserve containment of discovered config: {error}",
        );
    }

    /// Disabling uv configuration discovery does not waive containment for the
    /// project manifest: uv still consumes it as project/dependency metadata,
    /// and ruff, mypy and pytest may consume their own tables from the file.
    #[test]
    #[cfg(unix)]
    fn uv_no_config_keeps_independently_consumed_metadata_contained() {
        let root = tempfile::tempdir().expect("reviewed root");
        let outside = tempfile::tempdir().expect("outside");
        let foreign = outside.path().join("pyproject.toml");
        std::fs::write(&foreign, "[project]\nname = \"foreign\"\n").expect("foreign project");
        std::os::unix::fs::symlink(&foreign, root.path().join("pyproject.toml"))
            .expect("pyproject symlink");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        let Err(error) = plan_python_run_with_env(&config, |key| {
            (key == "UV_NO_CONFIG").then(|| OsString::from("1"))
        }) else {
            panic!("foreign project metadata remains outside the reviewed tree");
        };

        assert!(
            error.to_string().contains("pyproject.toml"),
            "the refusal must name the independently consumed metadata: {error}",
        );
    }

    /// `--no-config` disables discovery; it does not disable an explicit
    /// `--config-file`. uv gives UV_CONFIG_FILE the same semantics, so prview
    /// must still contain and read it when both environment variables are set.
    #[test]
    fn uv_no_config_keeps_explicit_config_authoritative() {
        let root = tempfile::tempdir().expect("reviewed root");
        std::fs::create_dir(root.path().join("config")).expect("config dir");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n\n[tool.uv]\nconcurrent-builds = 1\n",
        )
        .expect("pyproject");
        std::fs::write(
            root.path().join("config/explicit.toml"),
            "concurrent-builds = 3\n",
        )
        .expect("explicit config");

        let mut config = create_test_config(true, true, true);
        config.repo_root = root.path().to_path_buf();
        config.resource_plan.worker_limit = 4;
        let run = plan_python_run_with_env(&config, |key| match key {
            "UV_NO_CONFIG" => Some(OsString::from("true")),
            "UV_CONFIG_FILE" => Some(OsString::from("config/explicit.toml")),
            _ => None,
        })
        .expect("explicit config remains authoritative");

        assert_eq!(planned_env_value(&run, "UV_CONCURRENT_BUILDS"), "3");
    }

    #[test]
    fn uv_path_redirects_must_stay_on_the_exact_reviewed_root() {
        let root = tempfile::tempdir().expect("reviewed root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\nname = \"reviewed\"\n",
        )
        .expect("pyproject");
        let foreign_config = outside.path().join("uv.toml");
        std::fs::write(&foreign_config, "concurrent-builds = 1\n").expect("foreign config");

        for (key, value) in [
            ("UV_CONFIG_FILE", foreign_config.as_os_str()),
            ("UV_PROJECT", outside.path().as_os_str()),
            ("UV_WORKING_DIR", outside.path().as_os_str()),
            ("UV_WORKING_DIRECTORY", outside.path().as_os_str()),
        ] {
            let mut config = create_test_config(true, true, true);
            config.repo_root = root.path().to_path_buf();
            let value = value.to_os_string();
            let Err(error) = plan_python_run_with_env(&config, |candidate| {
                (candidate == key).then(|| value.clone())
            }) else {
                panic!("a foreign {key} selector must fail closed");
            };
            assert!(
                error.to_string().contains(key),
                "the refusal must name {key}: {error}",
            );
        }

        for key in ["UV_PROJECT", "UV_WORKING_DIR", "UV_WORKING_DIRECTORY"] {
            let mut config = create_test_config(true, true, true);
            config.repo_root = root.path().to_path_buf();
            let value = root.path().as_os_str().to_os_string();
            plan_python_run_with_env(&config, |candidate| {
                (candidate == key).then(|| value.clone())
            })
            .unwrap_or_else(|error| panic!("exact {key} must be accepted: {error}"));
        }
    }

    #[test]
    fn invalid_uv_project_concurrency_is_loud() {
        let root = tempfile::tempdir().expect("reviewed root");
        for contents in [
            "concurrent-builds = 0\n",
            "concurrent-builds = -1\n",
            "concurrent-builds = \"many\"\n",
            "concurrent-builds = [\n",
        ] {
            std::fs::write(root.path().join("uv.toml"), contents).expect("uv.toml");
            let error = project_uv_concurrency_limits(root.path(), None, false)
                .expect_err("an unknown project ceiling must not become absent");
            assert!(
                error.to_string().contains("uv configuration")
                    || error.to_string().contains("concurrent-builds"),
                "the error must identify uv's invalid authority: {error}",
            );
        }
    }

    #[test]
    fn non_utf8_uv_project_concurrency_is_loud() {
        let root = tempfile::tempdir().expect("reviewed root");
        std::fs::write(root.path().join("uv.toml"), b"\xff").expect("uv.toml");
        let error = project_uv_concurrency_limits(root.path(), None, false)
            .expect_err("non-UTF-8 project policy must not become absent");
        assert!(error.to_string().contains("not UTF-8"));
    }

    /// One environment per repository was still shared state: two prview
    /// processes reviewing different commits synced incompatible dependency sets
    /// into the same directory, each resynchronising under the other's running
    /// checks. Different substrates must get different environments.
    #[test]
    fn uv_environments_are_separated_per_reviewed_substrate() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let first_snapshot = tempfile::tempdir().expect("first snapshot");
        let second_snapshot = tempfile::tempdir().expect("second snapshot");

        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();

        config.scan_dir_override = Some(first_snapshot.path().to_path_buf());
        let first = plan_python_run(&config).expect("plan");
        config.scan_dir_override = Some(second_snapshot.path().to_path_buf());
        let second = plan_python_run(&config).expect("plan");

        assert_ne!(
            first.env, second.env,
            "two reviews of different substrates must not share one uv environment",
        );
        // Same substrate, same environment: reuse is what keeps this affordable.
        config.scan_dir_override = Some(first_snapshot.path().to_path_buf());
        assert_eq!(plan_python_run(&config).expect("plan").env, first.env);
    }

    /// Per-commit isolation trades one directory per repo for one per reviewed
    /// commit, so the working set has to be bounded — a virtualenv per commit
    /// ever reviewed is hundreds of megabytes each.
    #[test]
    fn stale_uv_environments_are_pruned_outside_the_working_set() {
        let root = tempfile::tempdir().expect("uv-env root");
        let mut envs = Vec::new();
        for name in ["one", "two", "three", "four"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            // Distinct marker mtimes, newest last.
            std::fs::write(dir.join(UV_ENV_USED_MARKER), b"").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            envs.push(dir);
        }

        // Nothing recent is ever removed, however many there are.
        prune_uv_envs(root.path(), 1, Duration::from_secs(3600));
        for dir in &envs {
            assert!(dir.is_dir(), "a live environment must survive: {dir:?}");
        }

        // Past the age floor, only the working set stays.
        prune_uv_envs(root.path(), 2, Duration::ZERO);
        assert!(!envs[0].is_dir() && !envs[1].is_dir(), "stale envs stay");
        assert!(
            envs[2].is_dir() && envs[3].is_dir(),
            "the newest environments are the working set",
        );
    }

    /// The age floor alone does not make pruning safe: two reviews run at once,
    /// and one can read an environment's timestamp just before the other
    /// refreshes it, then delete the directory after that other review's
    /// `uv run` has begun. While another live process holds the root, this one
    /// records its own use and touches nothing else.
    #[test]
    fn a_review_holding_the_prune_lock_keeps_the_sweep_to_itself() {
        let root = tempfile::tempdir().expect("uv-env root");
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        let mut envs = Vec::new();
        for name in ["one", "two", "three", "four", "five"] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            // Idle well past the age floor and beyond the working set, so an
            // unguarded sweep would delete them.
            let marker = std::fs::File::create(dir.join(UV_ENV_USED_MARKER)).unwrap();
            marker.set_modified(long_ago).unwrap();
            envs.push(dir);
        }

        // Another live prview is mid-sweep. Ownership is the OS lock on the
        // open handle; file contents are diagnostic metadata only.
        let _held = crate::storage::acquire_lock_at(&root.path().join(UV_PRUNE_LOCK))
            .expect("fixture owns the prune lock");

        mark_and_prune_uv_envs(root.path(), &envs[0]);

        for dir in &envs {
            assert!(
                dir.is_dir(),
                "a locked root belongs to the review holding it: {dir:?}",
            );
        }
        assert!(
            envs[0].join(UV_ENV_USED_MARKER).exists(),
            "our own use must still be recorded — that is what protects it next sweep",
        );
        assert!(
            root.path().join(UV_PRUNE_LOCK).is_file(),
            "a lock we never acquired must not be cleared on the way out",
        );
    }

    /// Pruning must never be what creates the directory tree: uv rejects an
    /// existing directory that is not a valid environment.
    #[test]
    fn marking_creates_nothing_when_no_environment_exists_yet() {
        let home = tempfile::tempdir().expect("home");
        let root = home.path().join("uv-env/repo");
        let env_dir = root.join("commit");

        mark_and_prune_uv_envs(&root, &env_dir);

        assert!(!root.exists(), "an absent root must stay absent");
        assert!(!env_dir.exists(), "uv creates the environment, not prview");
    }

    /// A local review keeps the operator's environment path, while every uv
    /// child still inherits the run's descendant caps.
    #[test]
    fn python_run_local_target_is_unchanged() {
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        let mut config = create_test_config(true, true, true);
        config.repo_root = repo_root.path().to_path_buf();

        let run = plan_python_run(&config).expect("plan");

        assert_eq!(run.cwd, repo_root.path());
        assert!(
            run.env
                .iter()
                .all(|(key, value)| key != "UV_PROJECT_ENVIRONMENT" && value == "1"),
            "a local review keeps its environment path but caps uv descendants",
        );
        assert_eq!(run.env.len(), 4);
    }

    #[test]
    fn pytest_xdist_requests_are_clamped_to_the_run_worker_limit() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\naddopts = '-n auto'\n",
        )
        .unwrap();

        let (args, env) =
            bounded_pytest_invocation(root.path(), &[], 1, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(
            args,
            [
                "-v".to_owned(),
                "-c".to_owned(),
                root.path().join("pyproject.toml").display().to_string(),
                "--rootdir".to_owned(),
                root.path().display().to_string(),
                "-n".to_owned(),
                "1".to_owned(),
            ]
        );
        assert!(
            env.iter()
                .any(|(key, value)| { key == "PYTEST_XDIST_AUTO_NUM_WORKERS" && value == "1" })
        );

        std::fs::write(
            root.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\n",
        )
        .unwrap();
        let (args, _) = bounded_pytest_invocation(
            root.path(),
            &[],
            2,
            Some("-n 12"),
            PytestConfigDialect::Nine,
        )
        .expect("pytest invocation");
        assert_eq!(&args[args.len() - 2..], ["-n", "2"]);
    }

    #[test]
    fn unrelated_xdist_text_does_not_inject_a_missing_plugin_option() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[project]\ndescription = 'documentation mentions pytest -n auto'\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("setup.cfg"),
            "[metadata]\ndescription = examples use -n 12\n",
        )
        .unwrap();

        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 1, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");

        assert!(!args.iter().any(|arg| arg == "-n"));
        assert_eq!(
            args[2],
            root.path().join("pyproject.toml").display().to_string()
        );
    }

    #[test]
    fn multiline_ini_addopts_are_clamped() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pytest.ini"),
            "[pytest]\naddopts = -q\n    --numprocesses=logical\n",
        )
        .unwrap();

        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 3, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");

        assert_eq!(&args[args.len() - 2..], ["-n", "3"]);
    }

    #[test]
    fn pytest_ini_colon_xdist_request_is_clamped() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pytest.ini"),
            "[pytest]\naddopts: -q -n 16\n",
        )
        .unwrap();

        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");

        assert_eq!(&args[args.len() - 2..], ["-n", "2"]);

        std::fs::write(
            root.path().join("pytest.ini"),
            "[pytest]\naddopts: -q -n=12\n",
        )
        .unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 1, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(&args[args.len() - 2..], ["-n", "1"]);
    }

    #[test]
    fn pytest_nine_toml_array_and_hidden_ini_are_clamped() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pytest.toml"),
            "[pytest]\naddopts = ['-q', '-n', '16']\n",
        )
        .unwrap();

        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 3, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(&args[args.len() - 2..], ["-n", "3"]);

        std::fs::remove_file(root.path().join("pytest.toml")).unwrap();
        std::fs::write(
            root.path().join(".pytest.ini"),
            "[pytest]\naddopts = --numprocesses=logical\n",
        )
        .unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 1, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(&args[args.len() - 2..], ["-n", "1"]);
    }

    #[test]
    fn pytest_config_precedence_uses_one_selected_source() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\naddopts = '-n 16'\n",
        )
        .unwrap();
        std::fs::write(root.path().join("pytest.ini"), "[pytest]\naddopts = -q\n").unwrap();

        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 1, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(
            args[2],
            root.path().join("pytest.ini").display().to_string()
        );
        assert!(!args.iter().any(|arg| arg == "-n"));

        // A dedicated pytest TOML matches even when empty and wins over INI.
        std::fs::write(root.path().join("pytest.toml"), "").unwrap();
        std::fs::write(
            root.path().join("pytest.ini"),
            "[pytest]\naddopts = -n 32\n",
        )
        .unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 1, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(
            args[2],
            root.path().join("pytest.toml").display().to_string()
        );
        assert!(!args.iter().any(|arg| arg == "-n"));
    }

    #[test]
    fn pytest_never_inherits_a_parent_projects_config() {
        let parent = tempfile::tempdir().expect("ambient parent");
        std::fs::write(
            parent.path().join("pytest.ini"),
            "[pytest]\naddopts = -n 64 --ignore=tests\n",
        )
        .unwrap();
        let root = parent.path().join("reviewed");
        std::fs::create_dir(&root).unwrap();

        let (args, _) = bounded_pytest_invocation(&root, &[], 1, None, PytestConfigDialect::Nine)
            .expect("pytest invocation");

        assert_eq!(args[2], empty_pytest_config().display().to_string());
        assert_eq!(args[4], root.display().to_string());
        assert!(!args.iter().any(|arg| arg == "-n"));
    }

    #[test]
    fn serial_pytest_keeps_its_command_but_caps_a_plugin_auto_pool() {
        let root = tempfile::tempdir().expect("pytest project");
        let base_env = vec![("UV_PROJECT_ENVIRONMENT".to_owned(), "env".to_owned())];

        let (args, env) =
            bounded_pytest_invocation(root.path(), &base_env, 1, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");

        assert_eq!(args[0], "-v");
        assert_eq!(args[1], "-c");
        assert!(!args.iter().any(|arg| arg == "-n"));
        assert_eq!(
            env,
            [
                ("UV_PROJECT_ENVIRONMENT".to_owned(), "env".to_owned()),
                ("PYTEST_XDIST_AUTO_NUM_WORKERS".to_owned(), "1".to_owned()),
            ]
        );
    }

    #[test]
    fn pytest_version_selects_the_runtime_config_dialect() {
        assert_eq!(
            parse_pytest_version("pytest 6.2.5\n").expect("pytest 6 version"),
            (PytestConfigDialect::LegacyPreHidden, "6.2.5".to_owned())
        );
        assert_eq!(
            parse_pytest_version("pytest 7.1.0\n").expect("pytest 7.1 version"),
            (PytestConfigDialect::LegacyPreHidden, "7.1.0".to_owned())
        );
        assert_eq!(
            parse_pytest_version("pytest 7.2.0\n").expect("pytest 7.2 version"),
            (PytestConfigDialect::LegacyHidden, "7.2.0".to_owned())
        );
        assert_eq!(
            parse_pytest_version("pytest 8.0.2\n").expect("pytest 8.0 version"),
            (PytestConfigDialect::LegacyHidden, "8.0.2".to_owned())
        );
        assert_eq!(
            parse_pytest_version("pytest 8.1.0\n").expect("pytest 8.1 version"),
            (PytestConfigDialect::Legacy, "8.1.0".to_owned())
        );
        assert_eq!(
            parse_pytest_version("pytest 8.4.2\n").expect("pytest 8 version"),
            (PytestConfigDialect::Legacy, "8.4.2".to_owned())
        );
        assert_eq!(
            parse_pytest_version("launcher note\npytest 9.1.0\n").expect("pytest 9 version"),
            (PytestConfigDialect::Nine, "9.1.0".to_owned())
        );
        assert!(parse_pytest_version("pytest development-build").is_err());
        assert!(parse_pytest_version("pytest 5.4.3").is_err());
        assert!(parse_pytest_version("pytest 10.0.0").is_err());
        assert!(parse_pytest_version("not pytest output").is_err());
    }

    #[test]
    fn pytest_version_probe_scrubs_all_ambient_plugin_inputs() {
        let env = pytest_probe_env(&[
            ("KEEP".to_owned(), "value".to_owned()),
            ("PYTEST_ADDOPTS".to_owned(), "-n 99".to_owned()),
            ("PYTEST_DISABLE_PLUGIN_AUTOLOAD".to_owned(), "0".to_owned()),
            ("PYTEST_PLUGINS".to_owned(), "ambient.plugin".to_owned()),
        ]);
        assert_eq!(
            env,
            [
                ("KEEP".to_owned(), "value".to_owned()),
                ("PYTEST_ADDOPTS".to_owned(), String::new()),
                ("PYTEST_DISABLE_PLUGIN_AUTOLOAD".to_owned(), "1".to_owned()),
                ("PYTEST_PLUGINS".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn pytest_addopts_use_python_shlex_token_boundaries_without_comments() {
        let quoted_flag = split_pytest_addopts(r#""-n" 8"#).expect("quoted flag");
        assert_eq!(
            xdist_worker_disposition(&quoted_flag).expect("supported worker count"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("8".to_owned()))
        );

        let quoted_pair = split_pytest_addopts(r#""-n 8" -q"#).expect("quoted pair");
        assert_eq!(
            xdist_worker_disposition(&quoted_pair).expect("attached quoted worker count"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("8".to_owned()))
        );

        let escaped_flag = split_pytest_addopts(r#"\-n 4"#).expect("escaped flag");
        assert_eq!(
            xdist_worker_disposition(&escaped_flag).expect("supported worker count"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("4".to_owned()))
        );

        let unquoted_hash = split_pytest_addopts("#marker -n 3").expect("literal hash");
        assert_eq!(unquoted_hash[0], "#marker");
        assert_eq!(
            xdist_worker_disposition(&unquoted_hash).expect("supported worker count"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("3".to_owned()))
        );

        let quoted_hash = split_pytest_addopts(r#"-k '#tag'"#).expect("quoted hash");
        assert_eq!(quoted_hash, ["-k", "#tag"]);
        assert_eq!(
            xdist_worker_disposition(&quoted_hash).expect("no worker count"),
            XdistWorkerDisposition::Absent
        );

        let escaped_hash = split_pytest_addopts(r#"\#tag -q"#).expect("escaped hash");
        assert_eq!(escaped_hash, ["#tag", "-q"]);
    }

    #[test]
    fn effective_xdist_worker_option_honors_disable_and_last_value() {
        let disposition = |value| {
            let tokens = split_pytest_addopts(value).expect("valid addopts");
            xdist_worker_disposition(&tokens).expect("supported worker count")
        };

        assert_eq!(disposition("-n 0"), XdistWorkerDisposition::Disabled);
        assert_eq!(disposition("-n0"), XdistWorkerDisposition::Disabled);
        assert_eq!(disposition("-n00"), XdistWorkerDisposition::Disabled);
        assert_eq!(disposition("-n +0"), XdistWorkerDisposition::Disabled);
        assert_eq!(disposition("-n 0_0"), XdistWorkerDisposition::Disabled);
        assert_eq!(disposition("-n -4"), XdistWorkerDisposition::Disabled);
        assert_eq!(
            disposition("--numprocesses=0"),
            XdistWorkerDisposition::Disabled
        );
        assert_eq!(
            disposition("--numprocesses 0"),
            XdistWorkerDisposition::Disabled
        );
        assert_eq!(disposition("-n 8 -n 0"), XdistWorkerDisposition::Disabled);
        assert_eq!(
            disposition("-n0 --numprocesses=logical"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Dynamic)
        );
        assert_eq!(
            disposition("--numprocesses=4 -n=0 -n auto"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Dynamic)
        );
        assert_eq!(
            disposition("-n001"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("1".to_owned()))
        );
        assert_eq!(
            disposition("-n +16"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("16".to_owned()))
        );
        assert_eq!(
            disposition("-n ' +16 '"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("16".to_owned()))
        );
        assert_eq!(
            disposition("--numprocesses=1_000"),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("1000".to_owned()))
        );
        assert_eq!(
            disposition(r#""--numprocesses= 16 ""#),
            XdistWorkerDisposition::Requested(XdistWorkerRequest::Count("16".to_owned()))
        );
        for invalid in [
            "-n invalid",
            "-n 1__0",
            "--numprocesses=invalid",
            "-n ١٦",
            "-n",
        ] {
            let tokens = split_pytest_addopts(invalid).expect("valid shlex");
            assert!(
                xdist_worker_disposition(&tokens).is_err(),
                "unsupported worker count must fail closed: {invalid}"
            );
        }
    }

    #[test]
    fn pytest_xdist_disable_overrides_config_and_environment_in_order() {
        let root = tempfile::tempdir().expect("pytest project");
        let config = root.path().join("pytest.ini");
        std::fs::write(&config, "[pytest]\naddopts = -n 0\n").unwrap();

        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("disabled config");
        assert!(!args.iter().any(|arg| arg == "-n"));

        std::fs::write(&config, "[pytest]\naddopts = -n 1\n").unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("lower explicit request");
        assert!(!args.iter().any(|arg| arg == "-n"));

        std::fs::write(&config, "[pytest]\naddopts = -n001\n").unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("zero-padded lower explicit request");
        assert!(!args.iter().any(|arg| arg == "-n"));

        std::fs::write(&config, "[pytest]\naddopts = -n auto\n").unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("dynamic request");
        assert_eq!(&args[args.len() - 2..], ["-n", "2"]);

        std::fs::write(&config, "[pytest]\naddopts = \"-n 1\"\n").unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("quoted attached lower request");
        assert!(!args.iter().any(|arg| arg == "-n"));

        std::fs::write(&config, "[pytest]\naddopts = \"-n 16\"\n").unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("quoted attached higher request");
        assert_eq!(&args[args.len() - 2..], ["-n", "2"]);

        std::fs::write(&config, "[pytest]\naddopts = -n 8\n").unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, Some("-n 0"), PytestConfigDialect::Nine)
                .expect("environment disables config request");
        assert!(!args.iter().any(|arg| arg == "-n"));

        std::fs::write(&config, "[pytest]\naddopts = --numprocesses=0\n").unwrap();
        let (args, _) = bounded_pytest_invocation(
            root.path(),
            &[],
            2,
            Some("--numprocesses=8"),
            PytestConfigDialect::Nine,
        )
        .expect("environment enables workers after config disable");
        assert_eq!(&args[args.len() - 2..], ["-n", "2"]);
    }

    #[test]
    fn malformed_pytest_addopts_fail_closed() {
        assert!(split_pytest_addopts("-k 'unterminated").is_err());
        assert!(split_pytest_addopts("-q \\").is_err());

        let root = tempfile::tempdir().expect("pytest project");
        let error = bounded_pytest_invocation(
            root.path(),
            &[],
            1,
            Some("-k 'unterminated"),
            PytestConfigDialect::Nine,
        )
        .expect_err("malformed ambient addopts must fail");
        assert!(error.to_string().contains("PYTEST_ADDOPTS"));
    }

    #[test]
    fn pytest_option_terminator_cannot_bypass_safety_arguments() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pytest.ini"),
            "[pytest]\naddopts = -n auto --\n",
        )
        .unwrap();
        assert!(
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .is_err()
        );

        std::fs::write(root.path().join("pytest.ini"), "[pytest]\n").unwrap();
        for inherited in ["-n auto --", "-- -n auto"] {
            let error = bounded_pytest_invocation(
                root.path(),
                &[],
                2,
                Some(inherited),
                PytestConfigDialect::Nine,
            )
            .expect_err("option terminator must fail closed");
            assert!(error.to_string().contains("PYTEST_ADDOPTS"));
        }
    }

    #[test]
    fn pytest_xdist_gateway_options_fail_closed_in_config_and_environment() {
        let root = tempfile::tempdir().expect("pytest project");
        let config = root.path().join("pytest.ini");
        for option in [
            "--tx=ssh=remote//python=python3",
            "--px=ssh=proxy//chdir=/tmp",
        ] {
            std::fs::write(&config, format!("[pytest]\naddopts = {option}\n")).unwrap();
            let error =
                bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                    .expect_err("config gateway must fail closed");
            assert!(error.to_string().contains("pytest config addopts"));
            assert!(error.to_string().contains(&option[..4]));
        }

        std::fs::write(&config, "[pytest]\n").unwrap();
        for inherited in [
            "--tx popen --tx=ssh=remote//python=python3",
            "--px proxy-spec",
            "--px=ssh=proxy//chdir=/tmp",
        ] {
            let error = bounded_pytest_invocation(
                root.path(),
                &[],
                2,
                Some(inherited),
                PytestConfigDialect::Nine,
            )
            .expect_err("environment gateway must fail closed");
            assert!(error.to_string().contains("PYTEST_ADDOPTS"));
            assert!(error.to_string().contains("--t") || error.to_string().contains("--p"));
        }
    }

    #[test]
    fn absent_pytest_addopts_is_distinct_from_an_unreadable_value() {
        assert_eq!(
            checked_pytest_addopts(Err(std::env::VarError::NotPresent))
                .expect("an absent variable is allowed"),
            None
        );
        assert_eq!(
            checked_pytest_addopts(Ok("-n 2".to_owned())).expect("Unicode value"),
            Some("-n 2".to_owned())
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let error = checked_pytest_addopts(Err(std::env::VarError::NotUnicode(
                std::ffi::OsString::from_vec(vec![0xff]),
            )))
            .expect_err("non-Unicode worker control must fail closed");
            assert!(error.to_string().contains("not valid Unicode"));
        }
    }

    #[test]
    fn inherited_xdist_auto_worker_cap_is_validated_without_global_env_mutation() {
        assert_eq!(
            checked_xdist_auto_workers(Err(std::env::VarError::NotPresent))
                .expect("absent auto-worker cap"),
            None
        );
        assert_eq!(
            checked_xdist_auto_workers(Ok("1_000".to_owned())).expect("positive cap"),
            Some("1000".to_owned())
        );
        assert_eq!(
            checked_xdist_auto_workers(Ok(" 2 ".to_owned())).expect("spaced positive cap"),
            Some("2".to_owned())
        );
        for invalid in ["0", "-1", "invalid", "١٦"] {
            assert!(
                checked_xdist_auto_workers(Ok(invalid.to_owned())).is_err(),
                "invalid inherited auto-worker cap must fail: {invalid}"
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            assert!(
                checked_xdist_auto_workers(Err(std::env::VarError::NotUnicode(
                    std::ffi::OsString::from_vec(vec![0xff]),
                )))
                .is_err()
            );
        }
    }

    #[test]
    fn inherited_xdist_auto_worker_cap_is_only_lowered() {
        fn env_value(env: &[(String, String)]) -> Option<&str> {
            env.iter()
                .find(|(key, _)| key == "PYTEST_XDIST_AUTO_NUM_WORKERS")
                .map(|(_, value)| value.as_str())
        }

        let root = tempfile::tempdir().expect("pytest project");

        let (_, env) = bounded_pytest_invocation_with_auto_workers(
            root.path(),
            &[],
            2,
            None,
            Some("1"),
            PytestConfigDialect::Nine,
        )
        .expect("lower inherited cap");
        assert_eq!(env_value(&env), Some("1"));

        let (_, env) = bounded_pytest_invocation_with_auto_workers(
            root.path(),
            &[],
            2,
            None,
            Some("10"),
            PytestConfigDialect::Nine,
        )
        .expect("higher inherited cap");
        assert_eq!(env_value(&env), Some("2"));

        let (_, env) = bounded_pytest_invocation_with_auto_workers(
            root.path(),
            &[],
            2,
            None,
            None,
            PytestConfigDialect::Nine,
        )
        .expect("default cap");
        assert_eq!(env_value(&env), Some("2"));

        assert!(
            bounded_pytest_invocation_with_auto_workers(
                root.path(),
                &[],
                2,
                None,
                Some("invalid"),
                PytestConfigDialect::Nine,
            )
            .is_err()
        );
    }

    #[test]
    fn pytest_toml_array_preserves_argument_boundaries_and_clamps_attached_counts() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pytest.toml"),
            "[pytest]\naddopts = ['-n 8', '-q']\n",
        )
        .unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(&args[args.len() - 2..], ["-n", "2"]);

        std::fs::write(
            root.path().join("pytest.toml"),
            "[pytest]\naddopts = ['-n', '8', '-q']\n",
        )
        .unwrap();
        let (args, _) =
            bounded_pytest_invocation(root.path(), &[], 2, None, PytestConfigDialect::Nine)
                .expect("pytest invocation");
        assert_eq!(&args[args.len() - 2..], ["-n", "2"]);
    }

    #[test]
    fn pytest_eight_and_nine_use_distinct_discovery_orders() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pytest.toml"),
            "[pytest]\naddopts = ['-n', '9']\n",
        )
        .unwrap();
        std::fs::write(root.path().join("pytest.ini"), "[pytest]\naddopts = -q\n").unwrap();

        let (legacy_path, legacy_addopts) =
            selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
                .expect("legacy config");
        assert_eq!(legacy_path, root.path().join("pytest.ini"));
        assert_eq!(legacy_addopts, Some(vec!["-q".to_owned()]));

        let (nine_path, nine_addopts) =
            selected_pytest_config(root.path(), PytestConfigDialect::Nine)
                .expect("pytest 9 config");
        assert_eq!(nine_path, root.path().join("pytest.toml"));
        assert_eq!(nine_addopts, Some(vec!["-n".to_owned(), "9".to_owned()]));
    }

    #[test]
    fn hidden_empty_ini_differs_between_pytest_eight_and_nine() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(root.path().join(".pytest.ini"), "").unwrap();
        std::fs::write(root.path().join("tox.ini"), "[pytest]\naddopts = -n 5\n").unwrap();

        let (legacy_path, _) = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect("legacy config");
        assert_eq!(legacy_path, root.path().join("tox.ini"));

        let (nine_path, _) = selected_pytest_config(root.path(), PytestConfigDialect::Nine)
            .expect("pytest 9 config");
        assert_eq!(nine_path, root.path().join(".pytest.ini"));

        std::fs::write(root.path().join("pytest.ini"), "").unwrap();
        let (legacy_path, _) = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect("legacy empty pytest.ini");
        assert_eq!(legacy_path, root.path().join("pytest.ini"));
    }

    #[test]
    fn hidden_ini_name_enters_discovery_in_pytest_seven_two() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join(".pytest.ini"),
            "[pytest]\naddopts = -n 7\n",
        )
        .unwrap();
        std::fs::write(root.path().join("tox.ini"), "[pytest]\naddopts = -q\n").unwrap();

        let (pre_hidden_path, _) =
            selected_pytest_config(root.path(), PytestConfigDialect::LegacyPreHidden)
                .expect("pytest 7.1 config");
        assert_eq!(pre_hidden_path, root.path().join("tox.ini"));

        let (hidden_path, hidden_addopts) =
            selected_pytest_config(root.path(), PytestConfigDialect::LegacyHidden)
                .expect("pytest 7.2 config");
        assert_eq!(hidden_path, root.path().join(".pytest.ini"));
        assert_eq!(hidden_addopts, Some(vec!["-n".to_owned(), "7".to_owned()]));
    }

    #[test]
    fn sectionless_pyproject_fallback_starts_in_pytest_eight_one() {
        let root = tempfile::tempdir().expect("pytest project");
        let pyproject = root.path().join("pyproject.toml");
        std::fs::write(&pyproject, "[project]\nname = 'fixture'\n").unwrap();

        for dialect in [
            PytestConfigDialect::LegacyPreHidden,
            PytestConfigDialect::LegacyHidden,
        ] {
            let (path, addopts) =
                selected_pytest_config(root.path(), dialect).expect("pre-8.1 config discovery");
            assert_eq!(path, empty_pytest_config());
            assert_eq!(addopts, None);
        }

        for dialect in [PytestConfigDialect::Legacy, PytestConfigDialect::Nine] {
            let (path, addopts) =
                selected_pytest_config(root.path(), dialect).expect("8.1+ config discovery");
            assert_eq!(path, pyproject);
            assert_eq!(addopts, None);
        }
    }

    #[test]
    fn pytest_nine_empty_native_table_continues_then_falls_back() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(root.path().join("pyproject.toml"), "[tool.pytest]\n").unwrap();
        std::fs::write(root.path().join("tox.ini"), "[pytest]\naddopts = -n 4\n").unwrap();

        let (path, addopts) = selected_pytest_config(root.path(), PytestConfigDialect::Nine)
            .expect("lower config after empty native table");
        assert_eq!(path, root.path().join("tox.ini"));
        assert_eq!(addopts, Some(vec!["-n".to_owned(), "4".to_owned()]));

        std::fs::remove_file(root.path().join("tox.ini")).unwrap();
        let (path, addopts) = selected_pytest_config(root.path(), PytestConfigDialect::Nine)
            .expect("pyproject fallback");
        assert_eq!(path, root.path().join("pyproject.toml"));
        assert_eq!(addopts, None);
    }

    #[test]
    fn pytest_native_and_legacy_pyproject_conflict_is_version_aware() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[tool.pytest]\naddopts = ['-n', '9']\n\n[tool.pytest.ini_options]\naddopts = '-q'\n",
        )
        .unwrap();

        let legacy = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect("pytest 8 ignores native keys");
        assert_eq!(legacy.1, Some(vec!["-q".to_owned()]));

        let error = selected_pytest_config(root.path(), PytestConfigDialect::Nine)
            .expect_err("pytest 9 rejects conflicting tables");
        assert!(error.to_string().contains("defines both"));

        std::fs::write(
            root.path().join("pyproject.toml"),
            "[tool.pytest]\naddopts = ['-n', '9']\n\n[tool.pytest.ini_options]\n",
        )
        .unwrap();
        assert!(
            selected_pytest_config(root.path(), PytestConfigDialect::Nine).is_err(),
            "even an empty legacy table conflicts with native pytest 9 keys"
        );
    }

    #[test]
    fn selected_config_ignores_malformed_lower_priority_files() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(root.path().join("pytest.toml"), "[pytest]\n").unwrap();
        std::fs::write(root.path().join("pytest.ini"), "[pytest\n").unwrap();

        let (path, _) = selected_pytest_config(root.path(), PytestConfigDialect::Nine)
            .expect("lower config is outside discovery after a winner");
        assert_eq!(path, root.path().join("pytest.toml"));
    }

    #[test]
    fn malformed_existing_pytest_configs_fail_closed() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(root.path().join("pytest.toml"), "[pytest\n").unwrap();
        assert!(selected_pytest_config(root.path(), PytestConfigDialect::Nine).is_err());
        let legacy = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect("pytest 8 ignores pytest.toml");
        assert_eq!(legacy.0, empty_pytest_config());

        std::fs::remove_file(root.path().join("pytest.toml")).unwrap();
        std::fs::write(root.path().join("pytest.ini"), "[pytest\n").unwrap();
        assert!(selected_pytest_config(root.path(), PytestConfigDialect::Legacy).is_err());
        assert!(selected_pytest_config(root.path(), PytestConfigDialect::Nine).is_err());

        std::fs::remove_file(root.path().join("pytest.ini")).unwrap();
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[tool.pytest.ini_options\n",
        )
        .unwrap();
        assert!(selected_pytest_config(root.path(), PytestConfigDialect::Legacy).is_err());
        assert!(selected_pytest_config(root.path(), PytestConfigDialect::Nine).is_err());
    }

    #[test]
    fn malformed_ini_bom_and_indented_sections_fail_closed() {
        assert!(parse_pytest_ini("pytest.ini", "\u{feff}[pytest]\naddopts = -q\n").is_err());
        assert!(parse_pytest_ini("pytest.ini", "  [pytest]\naddopts = -q\n").is_err());
        assert!(parse_pytest_ini("pytest.ini", "[pytest#comment]\naddopts = -q\n").is_err());
        assert!(parse_pytest_ini("pytest.ini", "[pytest]\nunexpected\n").is_err());
    }

    #[test]
    fn setup_cfg_pytest_section_is_fatal_but_tool_pytest_is_valid() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(root.path().join("setup.cfg"), "[pytest]\naddopts = -q\n").unwrap();
        let error = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect_err("setup.cfg [pytest] must fail");
        assert!(error.to_string().contains("[tool:pytest]"));
        assert!(selected_pytest_config(root.path(), PytestConfigDialect::Nine).is_err());

        std::fs::write(
            root.path().join("setup.cfg"),
            "[tool:pytest]\naddopts = -n logical\n",
        )
        .unwrap();
        let (path, addopts) = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect("valid setup.cfg");
        assert_eq!(path, root.path().join("setup.cfg"));
        assert_eq!(addopts, Some(vec!["-n".to_owned(), "logical".to_owned()]));
    }

    #[test]
    fn non_utf8_config_fails_closed_and_non_files_are_ignored() {
        let root = tempfile::tempdir().expect("pytest project");
        std::fs::write(root.path().join("pytest.ini"), [0xff, 0xfe]).unwrap();
        let error = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect_err("non-UTF-8 config must fail");
        assert!(error.to_string().contains("not valid UTF-8"));

        std::fs::remove_file(root.path().join("pytest.ini")).unwrap();
        std::fs::create_dir(root.path().join("pytest.ini")).unwrap();
        let (path, _) = selected_pytest_config(root.path(), PytestConfigDialect::Legacy)
            .expect("pytest ignores config names that are not files");
        assert_eq!(path, empty_pytest_config());
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_existing_pytest_config_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("pytest project");
        let path = root.path().join("pytest.ini");
        std::fs::write(&path, "[pytest]\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = read_pytest_config(&path);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            result.is_err(),
            "an unreadable existing config is not absent"
        );
    }

    #[test]
    fn test_mypy_check_name() {
        let check = MypyCheck;
        assert_eq!(check.name(), "Mypy");
    }

    #[test]
    fn test_pytest_check_name() {
        let check = PytestCheck;
        assert_eq!(check.name(), "Pytest");
    }

    #[test]
    fn test_ruff_check_can_run_with_pyproject_and_lint() {
        let config = create_test_config(true, true, false);
        let check = RuffCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_ruff_check_cannot_run_without_pyproject() {
        let config = create_test_config(false, true, false);
        let check = RuffCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_ruff_check_cannot_run_without_lint() {
        let config = create_test_config(true, false, false);
        let check = RuffCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_mypy_check_can_run_with_pyproject_and_lint() {
        let config = create_test_config(true, true, false);
        let check = MypyCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_mypy_check_cannot_run_without_pyproject() {
        let config = create_test_config(false, true, false);
        let check = MypyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_mypy_check_cannot_run_without_lint() {
        let config = create_test_config(true, false, false);
        let check = MypyCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_pytest_check_can_run_with_pyproject_and_tests() {
        let config = create_test_config(true, false, true);
        let check = PytestCheck;
        assert_eq!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Run
        );
    }

    #[test]
    fn test_pytest_check_cannot_run_without_pyproject() {
        let config = create_test_config(false, false, true);
        let check = PytestCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_pytest_check_cannot_run_without_tests_flag() {
        let config = create_test_config(true, false, false);
        let check = PytestCheck;
        assert!(matches!(
            check.check_eligibility(&config),
            super::super::CheckEligibility::Skip(_)
        ));
    }

    #[test]
    fn test_ruff_disables_unsafe_persistent_cache() {
        let config = create_test_config(true, true, false);
        let check = RuffCheck;
        assert!(check.cache_key(&config).is_none());
    }

    #[test]
    fn test_mypy_disables_unsafe_persistent_cache() {
        let config = create_test_config(true, true, false);
        let check = MypyCheck;
        assert!(check.cache_key(&config).is_none());
    }

    #[test]
    fn test_pytest_check_no_cache_key() {
        let config = create_test_config(true, false, true);
        let check = PytestCheck;
        let key = check.cache_key(&config);
        assert!(key.is_none());
    }

    // ── ruff missing-tool => Skipped, real lint failure => Failed ──

    #[test]
    fn test_ruff_status_spawn_fail_is_skipped() {
        // uv wrapping a missing ruff emits this; it must be Skipped (parity
        // with mypy), never a lint Failed that dents the gate in every Python
        // repo without ruff.
        let combined =
            "\nerror: Failed to spawn: `ruff`\n  Caused by: No such file or directory (os error 2)";
        assert_eq!(
            ruff_status(false, combined),
            CheckStatus::Skipped,
            "missing ruff must classify as Skipped, not Failed"
        );
    }

    #[test]
    fn test_ruff_status_command_not_found_is_skipped() {
        assert_eq!(
            ruff_status(false, "ruff: command not found"),
            CheckStatus::Skipped,
            "a bare 'command not found' missing ruff must be Skipped"
        );
    }

    #[test]
    fn test_ruff_status_real_lint_failure_is_failed() {
        let combined = "src/x.py:1:1: F401 [*] `os` imported but unused\nFound 1 error.\n";
        assert_eq!(
            ruff_status(false, combined),
            CheckStatus::Failed,
            "genuine lint findings must classify as Failed"
        );
    }

    #[test]
    fn test_ruff_status_success_is_passed() {
        assert_eq!(ruff_status(true, "All checks passed!"), CheckStatus::Passed);
    }

    // ── PV-01: mypy missing-tool => Skipped, real type error => Failed ──

    #[test]
    fn test_mypy_status_spawn_fail_is_skipped() {
        let combined =
            "\nerror: Failed to spawn: `mypy`\n  Caused by: No such file or directory (os error 2)";
        assert_eq!(
            mypy_status(false, combined),
            CheckStatus::Skipped,
            "uv spawn-fail must classify as Skipped, not Failed"
        );
    }

    #[test]
    fn test_mypy_status_real_type_error_is_failed() {
        let combined = "src/x.py:3: error: Incompatible return value type\nFound 1 error in 1 file";
        assert_eq!(
            mypy_status(false, combined),
            CheckStatus::Failed,
            "a real ': error:' line must classify as Failed"
        );
    }

    #[test]
    fn test_mypy_status_real_error_with_enoent_text_is_failed() {
        // P1 regression: a genuine mypy failure whose text contains "no such
        // file or directory" must stay Failed, not be misread as a missing tool.
        let combined = "src/a.py:10: error: Cannot find module: No such file or directory\nFound 1 error in 1 file";
        assert_eq!(
            mypy_status(false, combined),
            CheckStatus::Failed,
            "a real failure containing 'no such file or directory' must stay Failed"
        );
    }

    #[test]
    fn test_mypy_status_success_is_passed() {
        assert_eq!(
            mypy_status(true, "Success: no issues found"),
            CheckStatus::Passed
        );
    }

    use std::path::Path;

    fn run_git(repo: &Path, args: &[&str]) {
        let status = crate::git::git_cmd()
            .args(args)
            .current_dir(repo)
            .status()
            .expect("git command");
        assert!(status.success(), "git {args:?} failed with {status}");
    }

    fn write_commit(repo: &Path, name: &str, body: &str) -> String {
        std::fs::write(repo.join(name), body).expect("write fixture");
        run_git(repo, &["add", name]);
        run_git(
            repo,
            &[
                "-c",
                "user.name=prview test",
                "-c",
                "user.email=prview@example.test",
                "commit",
                "-m",
                name,
            ],
        );
        let output = crate::git::git_cmd()
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .expect("rev-parse");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Incomplete language fingerprints must not become persistent truth for
    /// either the local tree or an off-HEAD snapshot.
    #[test]
    fn language_checks_disable_persistent_cache_across_substrates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_path = tmp.path();
        run_git(repo_path, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo_path.join("pyproject.toml"),
            "[project]\nname = \"test\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        run_git(repo_path, &["add", "pyproject.toml"]);
        let first = write_commit(repo_path, "main.py", "def hello():\n    pass\n");
        write_commit(
            repo_path,
            "main.py",
            "import os\n\ndef hello():\n    pass\n",
        );

        let config_for = |target: Option<&str>| {
            let mut builder = test_config_builder()
                .profile(test_python_profile(true))
                .run_lint(true)
                .run_tests(true)
                .do_fetch(false)
                .repo_root(repo_path.to_path_buf());
            if let Some(target) = target {
                builder = builder.target(Some(target));
            }
            builder.build()
        };

        let local = config_for(None);
        let off_head = config_for(Some(first.as_str()));
        assert!(RuffCheck.cache_key(&local).is_none());
        assert!(RuffCheck.cache_key(&off_head).is_none());
        assert!(MypyCheck.cache_key(&local).is_none());
        assert!(MypyCheck.cache_key(&off_head).is_none());
    }

    #[tokio::test]
    async fn test_ruff_runs_on_fetched_target_in_remote_mode() {
        if which::which("ruff").is_err() && which::which("uv").is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo_path = tmp.path();
        run_git(repo_path, &["init", "-q", "-b", "main"]);

        // Write pyproject.toml so Ruff eligibility passes
        std::fs::write(
            repo_path.join("pyproject.toml"),
            "[project]\nname = \"test\"\nversion = \"0.1.0\"\n\n[tool.ruff]",
        )
        .unwrap();
        run_git(repo_path, &["add", "pyproject.toml"]);

        // 1. Commit clean state
        let clean_content = "def hello():\n    print('hello')\n";
        let clean_commit = write_commit(repo_path, "main.py", clean_content);

        // 2. Commit dirty state with unused import
        let dirty_content = "import os\n\ndef hello():\n    print('hello')\n";
        let dirty_commit = write_commit(repo_path, "main.py", dirty_content);

        // Scenario A: HEAD is checked out at clean_commit (working tree clean),
        // but target is dirty_commit. Ruff must analyze dirty_commit and report failure.
        run_git(repo_path, &["checkout", "-q", "-f", &clean_commit]);

        let config_a = test_config_builder()
            .profile(test_python_profile(true))
            .run_lint(true)
            .target(Some(dirty_commit.as_str()))
            .repo_root(repo_path.to_path_buf())
            .build();

        let check = RuffCheck;
        let result_a = check.run(&config_a).await.expect("ruff run scenario A");
        assert_eq!(
            result_a.status,
            CheckStatus::Failed,
            "Ruff must fail because fetched target commit has an unused import. Output: {}",
            result_a.output
        );

        // Scenario B: HEAD is checked out at dirty_commit (working tree dirty),
        // but target is clean_commit. Ruff must analyze clean_commit and pass.
        run_git(repo_path, &["checkout", "-q", "-f", &dirty_commit]);

        let config_b = test_config_builder()
            .profile(test_python_profile(true))
            .run_lint(true)
            .target(Some(clean_commit.as_str()))
            .repo_root(repo_path.to_path_buf())
            .build();

        let result_b = check.run(&config_b).await.expect("ruff run scenario B");
        assert_eq!(
            result_b.status,
            CheckStatus::Passed,
            "Ruff must pass because fetched target commit is clean. Output: {}",
            result_b.output
        );
    }

    /// PRV-PYTEST-HEAD regression: Pytest must run in the reviewed substrate
    /// (`plan.scan_dir`), never in `config.repo_root`.
    ///
    /// With a PR/remote target, `repo_root` still holds whatever branch happens
    /// to be checked out locally, so the pre-fix code reported a foreign
    /// branch's test failures against the PR. The fixture makes the two
    /// directories disagree on purpose: `repo_root` holds a FAILING test and the
    /// scan dir a PASSING one, so running in the wrong place is not merely
    /// observable — it flips the verdict.
    #[tokio::test]
    async fn test_pytest_runs_in_scan_dir_not_repo_root() {
        if which::which("pytest").is_err() && which::which("uv").is_err() {
            return;
        }

        // repo_root == the stale local checkout: its test FAILS.
        let repo_root = tempfile::tempdir().expect("repo_root tempdir");
        std::fs::write(
            repo_root.path().join("test_stale_local.py"),
            "def test_from_repo_root():\n    assert False, 'pytest ran in repo_root'\n",
        )
        .unwrap();

        // scan_dir == the reviewed target snapshot: its test PASSES.
        let scan_dir = tempfile::tempdir().expect("scan_dir tempdir");
        std::fs::write(
            scan_dir.path().join("test_reviewed_head.py"),
            "def test_from_scan_dir():\n    assert True\n",
        )
        .unwrap();

        let mut config = test_config_builder()
            .profile(test_python_profile(true))
            .run_tests(true)
            .repo_root(repo_root.path().to_path_buf())
            .do_fetch(false)
            .use_cache(false)
            .build();
        config.scan_dir_override = Some(scan_dir.path().to_path_buf());

        let result = PytestCheck.run(&config).await.expect("pytest run");

        assert_eq!(
            result.status,
            CheckStatus::Passed,
            "Pytest must run the reviewed snapshot's passing test, not repo_root's \
             failing one. Output: {}",
            result.output
        );
        assert!(
            !result.output.contains("pytest ran in repo_root"),
            "Pytest executed in repo_root instead of the reviewed scan dir. Output: {}",
            result.output
        );

        // Provenance must not claim a cwd the run never used.
        let cwd = result.provenance.expect("provenance").cwd;
        assert_eq!(
            std::fs::canonicalize(&cwd).unwrap(),
            std::fs::canonicalize(scan_dir.path()).unwrap(),
            "provenance cwd must report the reviewed scan dir"
        );
    }
}
