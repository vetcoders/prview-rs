#!/usr/bin/env python3
"""Prove the safe deep-review envelope on a real mixed-language fixture."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import pathlib
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any

WHOLE_MACHINE_TOOLS = ("cargo", "vitest", "semgrep", "tsc", "eslint", "stylelint")
# The census keeps every distinct command it sampled, up to this many. A deep
# review of the fixture produces far fewer; the cap only bounds a pathological
# run.
MAX_OBSERVED_COMMANDS = 400
REQUIRED_RUN_CHECKS = {
    "cargo": "Cargo check",
    "vitest": "Vitest",
    "semgrep": "Semgrep scan",
    "tsc": "TypeScript",
    "eslint": "ESLint",
    "stylelint": "Stylelint",
    "clippy": "Clippy",
    "rustfmt": "Rustfmt",
}
# Clippy and Rustfmt both invoke the `cargo` executable (`cargo clippy`,
# `cargo fmt --check`), so the process census already counts them under the
# "cargo" whole-machine tool in WHOLE_MACHINE_TOOLS above; they are not a
# second whole-machine parent to track. They still need their own live,
# non-cached, passed row in RUN.json, since a missing toolchain component
# makes cargo itself run while these two checks fail or are skipped.
REQUIRED_LIVE_CHECKS_ONLY = ("clippy", "rustfmt")


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


def has_successful_live_check(run: dict[str, Any] | None, expected_name: str) -> bool:
    """Return true only when the exact gate ran live and passed."""
    for row in (run or {}).get("checks") or []:
        if not isinstance(row, dict):
            continue
        if row.get("name") != expected_name:
            continue
        return row.get("status") == "passed" and row.get("cached") is False
    return False


def run_checked(command: list[str], cwd: pathlib.Path, log: pathlib.Path) -> None:
    with log.open("a", encoding="utf-8") as stream:
        stream.write(f"$ {' '.join(command)}\n")
        stream.flush()
        completed = subprocess.run(
            command,
            cwd=cwd,
            stdout=stream,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
            timeout=300,
        )
    if completed.returncode != 0:
        raise RuntimeError(
            f"setup command exited {completed.returncode}: {' '.join(command)}"
        )


NORMALIZED_MATH_JS = (
    "export function add(left, right) {\n"
    "  return Number(left) + Number(right);\n"
    "}\n"
)

# A second public function plus its test, so the Rust half of the `mixed` case
# is a real source edit inside the root package rather than a whitespace churn.
EXTENDED_LIB_RS = """pub fn add(left: i32, right: i32) -> i32 {
    left + right
}

pub fn triple(value: i32) -> i32 {
    value * 3
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds_two_numbers() {
        assert_eq!(super::add(2, 3), 5);
    }

    #[test]
    fn triples_a_number() {
        assert_eq!(super::triple(3), 9);
    }
}
"""

# Neither selector can see this file: vitest walks the static import graph and
# cargo walks package membership, while a data file is read at runtime. It is
# the contract's `Unknown` class, and it must widen both ecosystems.
UNKNOWN_INPUT_YAML = "max_items: 32\n"


def commit_candidate(
    repo: pathlib.Path, log: pathlib.Path, paths: list[str], message: str
) -> None:
    run_checked(["git", "add", *paths], repo, log)
    run_checked(["git", "commit", "-m", message], repo, log)


def mutate_js_only(repo: pathlib.Path, log: pathlib.Path) -> None:
    (repo / "src" / "math.js").write_text(NORMALIZED_MATH_JS, encoding="utf-8")
    commit_candidate(repo, log, ["src/math.js"], "fix: normalize numeric inputs")


def mutate_mixed(repo: pathlib.Path, log: pathlib.Path) -> None:
    (repo / "src" / "math.js").write_text(NORMALIZED_MATH_JS, encoding="utf-8")
    (repo / "src" / "lib.rs").write_text(EXTENDED_LIB_RS, encoding="utf-8")
    commit_candidate(
        repo,
        log,
        ["src/math.js", "src/lib.rs"],
        "feat: normalize numeric inputs and add triple",
    )


def mutate_unknown_input(repo: pathlib.Path, log: pathlib.Path) -> None:
    (repo / "src" / "math.js").write_text(NORMALIZED_MATH_JS, encoding="utf-8")
    (repo / "src" / "limits.yaml").write_text(UNKNOWN_INPUT_YAML, encoding="utf-8")
    commit_candidate(
        repo,
        log,
        ["src/math.js", "src/limits.yaml"],
        "feat: normalize numeric inputs and declare limits",
    )


def prepare_fixture(
    source: pathlib.Path,
    work: pathlib.Path,
    log: pathlib.Path,
    mutate: Any,
) -> pathlib.Path:
    repo = work / "mixed-review"
    shutil.copytree(
        source,
        repo,
        ignore=shutil.ignore_patterns(
            "node_modules", "target", ".loctree", ".acceptance-pack"
        ),
    )
    run_checked(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], repo, log)
    run_checked(["git", "init", "--initial-branch=main"], repo, log)
    run_checked(["git", "config", "user.name", "prview acceptance"], repo, log)
    run_checked(
        ["git", "config", "user.email", "acceptance@invalid.example"], repo, log
    )
    run_checked(["git", "add", "."], repo, log)
    run_checked(["git", "commit", "-m", "test: add bounded runtime fixture"], repo, log)
    run_checked(["git", "switch", "-c", "candidate"], repo, log)
    mutate(repo, log)
    return repo


ProcessTable = dict[int, tuple[int, str, str]]


def process_table() -> ProcessTable:
    completed = subprocess.run(
        ["ps", "-eo", "pid=,ppid=,stat=,args="],
        capture_output=True,
        text=True,
        check=False,
    )
    table: ProcessTable = {}
    for line in completed.stdout.splitlines():
        parts = line.strip().split(None, 3)
        if len(parts) != 4:
            continue
        try:
            table[int(parts[0])] = (int(parts[1]), parts[2], parts[3])
        except ValueError:
            continue
    return table


def descendants(table: ProcessTable, root: int) -> ProcessTable:
    owned: ProcessTable = {}
    frontier = {root}
    while frontier:
        children = {
            pid
            for pid, (ppid, _state, _command) in table.items()
            if ppid in frontier and pid not in owned
        }
        for pid in children:
            owned[pid] = table[pid]
        frontier = children
    return owned


def live_pids(pids: set[int]) -> set[int]:
    table = process_table()
    return {pid for pid in pids if pid in table and not table[pid][1].startswith("Z")}


def force_kill_pids(pids: set[int]) -> None:
    own_group = os.getpgrp()
    process_groups: set[int] = set()
    for pid in pids:
        try:
            group = os.getpgid(pid)
        except ProcessLookupError:
            continue
        if group > 0 and group != own_group:
            process_groups.add(group)
    for group in process_groups:
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    for pid in pids:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def cancel_after_timeout(
    process: subprocess.Popen[str], census: dict[str, Any]
) -> dict[str, Any]:
    captured = set(descendants(process_table(), process.pid))
    os.kill(process.pid, signal.SIGINT)
    grace_deadline = time.monotonic() + 10.0
    while time.monotonic() < grace_deadline:
        if process.poll() is None:
            current = descendants(process_table(), process.pid)
            captured.update(current)
            sample_owned_tree(process.pid, census)
        if process.poll() is not None and not live_pids(captured):
            break
        time.sleep(0.05)

    remaining = live_pids(captured)
    forced = bool(remaining) or process.poll() is None
    if forced:
        force_kill_pids(remaining | {process.pid})
    try:
        exit_code = process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        force_kill_pids({process.pid})
        exit_code = process.wait(timeout=5)
        forced = True
    time.sleep(0.1)
    return {
        "method": "sigint_then_force",
        "exit_code": exit_code,
        "forced": forced,
        "captured_pids": sorted(captured),
        "remaining_pids": sorted(live_pids(captured)),
    }


def force_stop(process: subprocess.Popen[str]) -> int:
    owned = set(descendants(process_table(), process.pid))
    force_kill_pids(owned | {process.pid})
    return process.wait(timeout=5)


def tool_for(command: str) -> str | None:
    lowered = command.lower()
    executable = pathlib.Path(lowered.split()[0]).name if lowered.split() else ""
    if "vitest" in lowered or "tinypool" in lowered:
        return "vitest"
    if "semgrep" in lowered:
        return "semgrep"
    if executable in {"cargo", "cargo-audit", "cargo-clippy", "cargo-geiger"}:
        return "cargo"
    if executable in {"tsc", "eslint", "stylelint"}:
        return executable
    if "node_modules/.bin/tsc" in lowered:
        return "tsc"
    if "node_modules/.bin/eslint" in lowered:
        return "eslint"
    if "node_modules/.bin/stylelint" in lowered:
        return "stylelint"
    return None


def read_linux_environment(pid: int) -> dict[str, str]:
    path = pathlib.Path("/proc") / str(pid) / "environ"
    try:
        raw = path.read_bytes()
    except OSError:
        return {}
    result: dict[str, str] = {}
    for item in raw.split(b"\0"):
        if b"=" not in item:
            continue
        key, value = item.split(b"=", 1)
        result[key.decode(errors="replace")] = value.decode(errors="replace")
    return result


def cap_argument(command: str, flag: str) -> str | None:
    parts = command.split()
    for index, part in enumerate(parts):
        if part == flag and index + 1 < len(parts):
            return parts[index + 1]
        if part.startswith(f"{flag}="):
            return part.split("=", 1)[1]
    return None


def is_semgrep_worker(command: str) -> bool:
    """Separate scan workers from Semgrep's RPC coordinator processes."""
    lowered = f" {command.lower()} "
    return (
        "semgrep-core" in lowered
        and " -rpc " not in lowered
        and not command.startswith("(")
    )


def sample_owned_tree(root_pid: int, census: dict[str, Any]) -> None:
    owned = descendants(process_table(), root_pid)
    tool_by_pid = {
        pid: tool
        for pid, (_ppid, state, command) in owned.items()
        if not state.startswith("Z") and (tool := tool_for(command)) is not None
    }
    tool_roots: list[tuple[int, str]] = []
    for pid, tool in tool_by_pid.items():
        ancestor = owned[pid][0]
        while ancestor in owned and tool_by_pid.get(ancestor) != tool:
            ancestor = owned[ancestor][0]
        if tool_by_pid.get(ancestor) != tool:
            tool_roots.append((pid, tool))

    active_tools = {tool for _pid, tool in tool_roots}
    rustc = 0
    vitest_workers = 0
    semgrep_workers = 0
    semgrep_core_processes = 0
    commands: list[str] = []

    for pid, (_ppid, state, command) in owned.items():
        if state.startswith("Z"):
            continue
        commands.append(command)
        # Every pid this run ever owned. "The deadline left nothing behind" is
        # only provable against the set of children that actually existed.
        census["seen_pids"].add(pid)
        # The whole-run command census. "No `cargo test` process existed" is
        # only provable against the set of commands actually observed, so the
        # sampler keeps them (bounded, so a long run cannot grow unboundedly).
        if len(census["observed_commands"]) < MAX_OBSERVED_COMMANDS:
            census["observed_commands"].add(command)
        elif command not in census["observed_commands"]:
            # The cap is reached and this command was dropped. Every claim of
            # the form "no such process existed" is now unprovable from the
            # census, and the assertions that make it must fail rather than
            # pass on an incomplete set.
            census["truncated"] = True
        tool = tool_by_pid.get(pid)
        if tool:
            active_tools.add(tool)
            census["seen_tools"][tool] = True
        executable = pathlib.Path(command.split()[0]).name if command.split() else ""
        if executable in {"rustc", "clippy-driver"}:
            rustc += 1
        if "tinypool" in command.lower():
            vitest_workers += 1
        if "semgrep-core" in command.lower():
            semgrep_core_processes += 1
            if is_semgrep_worker(command):
                semgrep_workers += 1
        if tool == "cargo":
            jobs = read_linux_environment(pid).get("CARGO_BUILD_JOBS")
            if jobs:
                census["observed_caps"]["cargo_build_jobs"].add(jobs)
        if tool == "vitest":
            workers = cap_argument(command, "--maxWorkers")
            if workers:
                census["observed_caps"]["vitest_max_workers"].add(workers)
        if tool == "semgrep":
            jobs = cap_argument(command, "--jobs")
            if jobs:
                census["observed_caps"]["semgrep_jobs"].add(jobs)

    census["samples"] += 1
    census["max_whole_machine_parents"] = max(
        census["max_whole_machine_parents"], len(tool_roots)
    )
    census["max_descendants"]["rustc"] = max(census["max_descendants"]["rustc"], rustc)
    census["max_descendants"]["vitest_workers"] = max(
        census["max_descendants"]["vitest_workers"], vitest_workers
    )
    census["max_descendants"]["semgrep_core"] = max(
        census["max_descendants"]["semgrep_core"], semgrep_workers
    )
    census["max_descendants"]["semgrep_core_processes"] = max(
        census["max_descendants"]["semgrep_core_processes"], semgrep_core_processes
    )

    parent_labels = [f"{tool}:{pid}" for pid, tool in sorted(tool_roots)]
    signature = (
        tuple(parent_labels),
        rustc,
        vitest_workers,
        semgrep_workers,
        semgrep_core_processes,
    )
    if signature != census["last_signature"]:
        census["transitions"].append(
            {
                "at_secs": round(time.monotonic() - census["started_monotonic"], 3),
                "active_tools": sorted(active_tools),
                "tool_parents": parent_labels,
                "rustc": rustc,
                "vitest_workers": vitest_workers,
                "semgrep_workers": semgrep_workers,
                "semgrep_core_processes": semgrep_core_processes,
                "commands": sorted(set(commands))[:12],
            }
        )
        census["last_signature"] = signature


def machine_observation() -> dict[str, Any]:
    memory_kib = None
    meminfo = pathlib.Path("/proc/meminfo")
    if meminfo.exists():
        for line in meminfo.read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                memory_kib = int(line.split()[1])
                break
    try:
        load = list(os.getloadavg())
    except OSError:
        load = None
    return {
        "platform": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "memory_total_kib": memory_kib,
        "load_1_5_15": load,
        "runner_name": os.environ.get("RUNNER_NAME"),
        "runner_os": os.environ.get("RUNNER_OS"),
        "runner_arch": os.environ.get("RUNNER_ARCH"),
        "github_run_id": os.environ.get("GITHUB_RUN_ID"),
        "github_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
    }


def read_json(path: pathlib.Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def source_tree_observation(root: pathlib.Path) -> dict[str, Any]:
    head = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    status = subprocess.run(
        ["git", "-C", str(root), "status", "--porcelain"],
        capture_output=True,
        text=True,
        check=False,
    )
    return {
        "root": str(root),
        "head_sha": head.stdout.strip() if head.returncode == 0 else None,
        "dirty": bool(status.stdout.strip()) if status.returncode == 0 else None,
        "git_errors": [
            message
            for message in [
                head.stderr.strip() if head.returncode != 0 else "",
                status.stderr.strip() if status.returncode != 0 else "",
            ]
            if message
        ],
    }


def binary_observation(path: pathlib.Path) -> dict[str, Any]:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    probe = subprocess.run(
        [str(path), "--build-source-sha"],
        capture_output=True,
        text=True,
        check=False,
        timeout=10,
    )
    return {
        "path": str(path),
        "sha256": digest.hexdigest(),
        "embedded_source_sha": probe.stdout.strip() if probe.returncode == 0 else None,
        "probe_exit_code": probe.returncode,
        "probe_stderr": probe.stderr.strip() or None,
    }


def add_assertion(violations: list[str], condition: bool, message: str) -> None:
    if not condition:
        violations.append(message)


# ---------------------------------------------------------------------------
# Change-scoped execution: what each case proves
# ---------------------------------------------------------------------------

CARGO_TEST_CHECK = "Cargo test"
VITEST_CHECK = "Vitest"
NO_TESTS_RELATED = "no tests related to the change"
# What a check publishes as its `command` when it spawned no process at all.
NO_COMMAND_RECORDED = "<no command recorded>"


def check_row(document: dict[str, Any] | None, name: str) -> dict[str, Any]:
    """The named row of a RUN.json / MERGE_GATE.json check list, or `{}`."""
    rows = (document or {}).get("checks") or []
    for row in rows:
        if isinstance(row, dict) and row.get("name") == name:
            return row
    return {}


def scope_of(document: dict[str, Any] | None, name: str) -> dict[str, Any]:
    scope = check_row(document, name).get("scope")
    return scope if isinstance(scope, dict) else {}


def gate_command(pack: pathlib.Path, gate_id: str) -> str:
    """The command a gate actually spawned, from its own result artifact."""
    result = read_json(pack / "20_quality" / f"{gate_id}.result.json") or {}
    command = result.get("command")
    return command if isinstance(command, str) else ""


def install_cargo_shim(work: pathlib.Path, env: dict[str, str]) -> pathlib.Path | None:
    """Put a logging `cargo` in front of PATH; return the log it writes.

    The census samples the process table every 50ms, which can only ever say
    "we did not happen to see it". This shim is the other kind of evidence: a
    `cargo` that every invocation must pass through, so its log is a complete
    record of what ran rather than a sample of it.

    The real cargo is resolved BEFORE the shim reaches PATH — `which` would
    otherwise find the shim and it would exec itself forever. Resolution keeps
    whatever `cargo` the machine uses, rustup proxy included: the proxy execs
    the toolchain binary directly and never re-resolves the name, so the shim
    dispatches exactly as an unshimmed run would. `exec` also means the shim
    leaves no extra process in the tree, so descendant counts and the
    `CARGO_BUILD_JOBS` census read the same processes with the same
    environment they always did.
    """
    real = shutil.which("cargo", path=env.get("PATH"))
    if real is None:
        return None
    shim_dir = work / "cargo-shim"
    shim_dir.mkdir(parents=True, exist_ok=True)
    log = work / "cargo-invocations.log"
    shim = shim_dir / "cargo"
    shim.write_text(
        "#!/bin/sh\n"
        f"printf '%s\\n' \"$*\" >> {json.dumps(str(log))}\n"
        f"exec {json.dumps(str(real))} \"$@\"\n",
        encoding="utf-8",
    )
    shim.chmod(0o755)
    env["PATH"] = f"{shim_dir}{os.pathsep}{env.get('PATH', '')}"
    return log


def cargo_shim_invocations(log: pathlib.Path | None) -> list[str]:
    """Every `cargo` argument line the shim recorded, in order."""
    if log is None or not log.exists():
        return []
    return [
        line.strip()
        for line in log.read_text(encoding="utf-8", errors="replace").splitlines()
        if line.strip()
    ]


def shim_cargo_test_invocations(census: dict[str, Any]) -> list[str]:
    """The recorded invocations that are `cargo test`, by subcommand."""
    found = []
    for invocation in census.get("cargo_invocations") or []:
        parts = invocation.split()
        # The subcommand is the first non-flag word: `cargo +nightly test` and
        # `cargo test` are both a test run, `cargo clippy --tests` is not.
        subcommand = next(
            (part for part in parts if not part.startswith(("-", "+"))), None
        )
        if subcommand == "test":
            found.append(invocation)
    return found


def observed_cargo_test_commands(census: dict[str, Any]) -> list[str]:
    """Every sampled process that is a `cargo test` invocation."""
    found = []
    for command in census["observed_commands"]:
        parts = command.split()
        if not parts:
            continue
        if pathlib.Path(parts[0]).name != "cargo":
            continue
        if "test" in parts[1:2]:
            found.append(command)
    return found


def assert_mixed(
    violations: list[str],
    run: dict[str, Any] | None,
    gate: dict[str, Any] | None,
    pack: pathlib.Path,
    census: dict[str, Any],
) -> None:
    """A Rust edit and a JS edit: both suites narrow, neither is skipped."""
    cargo = check_row(run, CARGO_TEST_CHECK)
    cargo_scope = scope_of(run, CARGO_TEST_CHECK)
    add_assertion(
        violations,
        cargo.get("status") == "passed" and cargo.get("cached") is False,
        "Cargo test did not run live and pass",
    )
    add_assertion(
        violations,
        cargo_scope.get("mode") == "change-scoped",
        f"Cargo test scope mode is {cargo_scope.get('mode')!r}, not change-scoped",
    )
    add_assertion(
        violations,
        cargo_scope.get("selected") == 1,
        f"Cargo test selected {cargo_scope.get('selected')!r} packages, not 1",
    )
    add_assertion(
        violations,
        cargo_scope.get("universe") == 2,
        f"Cargo test universe is {cargo_scope.get('universe')!r}, not the 2 workspace members",
    )
    cargo_selector = cargo_scope.get("selector") or ""
    add_assertion(
        violations,
        "-p prview-bounded-runtime-fixture" in cargo_selector,
        f"Cargo selector does not name the changed package: {cargo_selector!r}",
    )
    add_assertion(
        violations,
        "unrelated" not in cargo_selector,
        f"Cargo selector reaches a package nothing depends on: {cargo_selector!r}",
    )
    cargo_command = gate_command(pack, "cargo_test")
    add_assertion(
        violations,
        cargo_selector != "" and cargo_selector in cargo_command,
        f"reported selector {cargo_selector!r} is not part of {cargo_command!r}",
    )
    add_assertion(
        violations,
        "-p prview-bounded-runtime-unrelated" not in cargo_command,
        f"the narrowed command still tested the unrelated package: {cargo_command!r}",
    )
    # Same claim, from the other side: the shim records every cargo invocation,
    # so the narrowed test run has to appear there exactly as the pack describes
    # it. The pack says what prview believes it ran; this says what ran.
    shimmed = shim_cargo_test_invocations(census)
    add_assertion(
        violations,
        any("-p prview-bounded-runtime-fixture" in invocation for invocation in shimmed),
        f"the cargo shim recorded no narrowed test run: {shimmed}",
    )
    add_assertion(
        violations,
        all(
            "-p prview-bounded-runtime-unrelated" not in invocation
            for invocation in shimmed
        ),
        f"a cargo test run reached the unrelated package: {shimmed}",
    )

    vitest = check_row(run, VITEST_CHECK)
    vitest_scope = scope_of(run, VITEST_CHECK)
    add_assertion(
        violations,
        vitest.get("status") == "passed" and vitest.get("cached") is False,
        "Vitest did not run live and pass",
    )
    add_assertion(
        violations,
        vitest_scope.get("mode") == "change-scoped",
        f"Vitest scope mode is {vitest_scope.get('mode')!r}, not change-scoped",
    )
    vitest_selector = vitest_scope.get("selector") or ""
    add_assertion(
        violations,
        "related" in vitest_selector and "src/math.js" in vitest_selector,
        f"Vitest selector does not name the related selection: {vitest_selector!r}",
    )
    # Contract §7: Vitest counts TEST FILES, and the fixture has exactly one
    # importing `src/math.js`. Counting the changed source instead would happen
    # to give 1 here too, so the selector above (one source) and this count
    # (one test file) are asserted as the separate facts they are.
    add_assertion(
        violations,
        vitest_scope.get("selected") == 1,
        f"Vitest selected {vitest_scope.get('selected')!r} test files, not the 1 "
        "that imports the changed source",
    )
    vitest_command = gate_command(pack, "tests")
    add_assertion(
        violations,
        vitest_selector != "" and vitest_selector in vitest_command,
        f"reported selector {vitest_selector!r} is not part of the Vitest command",
    )
    # A narrowed Vitest run is only believable if it can prove what it executed,
    # and the proof is the JSON reporter. Without these flags the check would be
    # back to reading the tool's prose.
    add_assertion(
        violations,
        "--reporter=json" in vitest_command and "--outputFile.json=" in vitest_command,
        f"the narrowed Vitest command carries no JSON reporter: {vitest_command!r}",
    )

    caveats = ((gate or {}).get("decision") or {}).get("review_caveats") or []
    for name in (CARGO_TEST_CHECK, VITEST_CHECK):
        add_assertion(
            violations,
            any(
                isinstance(caveat, str)
                and name in caveat
                and "change-scoped test selection" in caveat
                for caveat in caveats
            ),
            f"MERGE_GATE does not caveat the narrowed {name} run",
        )


def assert_js_only(
    violations: list[str],
    run: dict[str, Any] | None,
    gate: dict[str, Any] | None,
    pack: pathlib.Path,
    census: dict[str, Any],
) -> None:
    """A JS-only change: the Rust suite has nothing to run, and runs nothing."""
    vitest = check_row(run, VITEST_CHECK)
    vitest_scope = scope_of(run, VITEST_CHECK)
    add_assertion(
        violations,
        vitest.get("status") == "passed" and vitest.get("cached") is False,
        "Vitest did not run live and pass",
    )
    add_assertion(
        violations,
        vitest_scope.get("mode") == "change-scoped",
        f"Vitest scope mode is {vitest_scope.get('mode')!r}, not change-scoped",
    )
    add_assertion(
        violations,
        vitest_scope.get("selected") == 1,
        f"Vitest selected {vitest_scope.get('selected')!r} test files, not the 1 "
        "that imports the changed source",
    )
    vitest_command = gate_command(pack, "tests")
    add_assertion(
        violations,
        "--reporter=json" in vitest_command and "--outputFile.json=" in vitest_command,
        f"the narrowed Vitest command carries no JSON reporter: {vitest_command!r}",
    )

    cargo = check_row(run, CARGO_TEST_CHECK)
    cargo_scope = scope_of(run, CARGO_TEST_CHECK)
    add_assertion(
        violations,
        cargo.get("status") == "skipped",
        f"Cargo test status is {cargo.get('status')!r}, not skipped",
    )
    add_assertion(
        violations,
        cargo.get("cached") is False,
        "Cargo test was replayed from cache instead of deciding live",
    )
    add_assertion(
        violations,
        cargo_scope.get("mode") == "change-scoped",
        f"Cargo test scope mode is {cargo_scope.get('mode')!r}, not change-scoped",
    )
    add_assertion(
        violations,
        cargo_scope.get("selected") == 0,
        f"Cargo test selected {cargo_scope.get('selected')!r} packages, not 0",
    )
    add_assertion(
        violations,
        cargo_scope.get("selector") is None,
        f"an empty selection published a selector: {cargo_scope.get('selector')!r}",
    )
    # The decisive one: an empty selection must cost nothing. The canonical
    # witness is the `cargo` shim every invocation passes through — a complete
    # record, unlike the process census, which can only say what it happened to
    # sample. The shim must have recorded SOMETHING (this review runs Cargo
    # check and Clippy), or it was not on PATH and its silence proves nothing.
    add_assertion(
        violations,
        bool(census.get("cargo_invocations")),
        "the cargo shim recorded no invocation at all, so it cannot witness anything",
    )
    shimmed = shim_cargo_test_invocations(census)
    add_assertion(
        violations,
        not shimmed,
        f"cargo test was invoked for an empty selection: {shimmed}",
    )
    # The process census corroborates it, and says so only while it is complete.
    add_assertion(
        violations,
        not census.get("truncated"),
        "the process census hit its command cap, so it cannot witness an absence",
    )
    stray = observed_cargo_test_commands(census)
    add_assertion(
        violations,
        not stray,
        f"a cargo test process ran for an empty selection: {stray}",
    )
    # The pack must tell the same story: no command recorded for the gate that
    # ran nothing, in the very artifact a reader would check.
    cargo_command = gate_command(pack, "cargo_test")
    add_assertion(
        violations,
        cargo_command == NO_COMMAND_RECORDED,
        f"the skipped Cargo test gate published {cargo_command!r}, not {NO_COMMAND_RECORDED!r}",
    )

    gate_cargo = check_row(gate, CARGO_TEST_CHECK)
    add_assertion(
        violations,
        gate_cargo.get("outcome") == "skipped",
        f"MERGE_GATE outcome for Cargo test is {gate_cargo.get('outcome')!r}, not skipped",
    )
    add_assertion(
        violations,
        gate_cargo.get("blocking") is False,
        "a suite with nothing to run blocked the merge",
    )
    add_assertion(
        violations,
        NO_TESTS_RELATED in str(gate_cargo.get("reason") or ""),
        f"MERGE_GATE does not state why Cargo test ran nothing: {gate_cargo.get('reason')!r}",
    )
    decision = (gate or {}).get("decision") or {}
    add_assertion(
        violations,
        decision.get("verdict") != "BLOCK",
        f"verdict is {decision.get('verdict')!r} on a change with no related Rust tests",
    )
    caveats = decision.get("review_caveats") or []
    add_assertion(
        violations,
        any(
            isinstance(caveat, str)
            and CARGO_TEST_CHECK in caveat
            and NO_TESTS_RELATED in caveat
            for caveat in caveats
        ),
        "MERGE_GATE does not caveat the skipped Cargo test suite",
    )


def assert_unknown_input(
    violations: list[str],
    run: dict[str, Any] | None,
    gate: dict[str, Any] | None,
    pack: pathlib.Path,
    census: dict[str, Any],
) -> None:
    """A file neither selector can see: doubt widens BOTH suites."""
    for name, gate_id in ((CARGO_TEST_CHECK, "cargo_test"), (VITEST_CHECK, "tests")):
        row = check_row(run, name)
        scope = scope_of(run, name)
        add_assertion(
            violations,
            row.get("status") == "passed" and row.get("cached") is False,
            f"{name} did not run live and pass",
        )
        add_assertion(
            violations,
            scope.get("mode") == "full",
            f"{name} scope mode is {scope.get('mode')!r}, not full",
        )
        add_assertion(
            violations,
            scope.get("selector") is None,
            f"{name} published a selector for a full run: {scope.get('selector')!r}",
        )
        add_assertion(
            violations,
            "unsupported input: src/limits.yaml" in str(scope.get("reason") or ""),
            f"{name} does not name the file that widened the run: {scope.get('reason')!r}",
        )
        command = gate_command(pack, gate_id)
        add_assertion(
            violations,
            " -p " not in f" {command} " and " related " not in f" {command} ",
            f"{name} ran a narrowed command despite a full decision: {command!r}",
        )
        # The JSON reporter belongs to a narrowed Vitest run and to nothing
        # else: a full run is judged by its exit code, exactly as it always was.
        add_assertion(
            violations,
            "--reporter=json" not in command,
            f"{name} ran a full command carrying the narrowed run's reporter: {command!r}",
        )


def evaluate_deadline(
    receipt: dict[str, Any],
    census: dict[str, Any],
    pack: pathlib.Path,
    log: pathlib.Path,
    case: dict[str, Any],
) -> None:
    """Contract §10: an expired run reports no verdict, and leaves nothing running.

    A deliberately tiny budget on the same mixed fixture the other cases use.
    The run is stopped while its checks are still working, so there is no pack
    at all — the artifact-stage half of the contract (an `INCOMPLETE.json`
    naming the deadline) is asserted at unit level, where the seam can be hit
    deterministically.
    """
    del case
    violations = receipt["violations"]
    log_text = log.read_text(encoding="utf-8", errors="replace") if log.exists() else ""
    receipt["pack"] = {
        "run_json": (pack / "00_summary" / "RUN.json").exists(),
        "merge_gate_json": (pack / "00_summary" / "MERGE_GATE.json").exists(),
        "incomplete_json": (pack / "00_summary" / "INCOMPLETE.json").exists(),
    }

    add_assertion(
        violations,
        receipt["process"]["exit_code"] == 3,
        f"an expired run must exit 3, got {receipt['process']['exit_code']}",
    )
    add_assertion(
        violations,
        not receipt["process"]["timed_out"],
        "the deadline did not stop the run; the harness had to",
    )
    add_assertion(
        violations,
        "deadline" in log_text.lower(),
        "the run did not say that its deadline stopped it",
    )
    add_assertion(
        violations,
        not receipt["pack"]["merge_gate_json"] and not receipt["pack"]["run_json"],
        "an expired run published a verdict-shaped surface",
    )
    published = [
        surface
        for surface in (
            "report.json",
            "dashboard.html",
            "review.html",
            "PR_REVIEW.md",
            "00_summary/MERGE_GATE.md",
        )
        if (pack / surface).exists()
    ]
    add_assertion(
        violations,
        not published,
        f"an expired run left success-shaped surfaces behind: {published}",
    )
    remaining = sorted(live_pids(set(census["seen_pids"])))
    receipt["orphans"] = remaining
    add_assertion(
        violations,
        not remaining,
        f"the expired run left live descendants behind: {remaining}",
    )


CASES: dict[str, dict[str, Any]] = {
    "mixed": {"mutate": mutate_mixed, "assert_scope": assert_mixed},
    "js-only": {"mutate": mutate_js_only, "assert_scope": assert_js_only},
    "unknown-input": {
        "mutate": mutate_unknown_input,
        "assert_scope": assert_unknown_input,
    },
    "deadline": {
        "mutate": mutate_mixed,
        "extra_args": ["--deadline", "2s"],
        "evaluate": evaluate_deadline,
    },
}


def evaluate(
    receipt: dict[str, Any],
    census: dict[str, Any],
    pack: pathlib.Path,
    log: pathlib.Path,
    case: dict[str, Any],
) -> None:
    run_path = pack / "00_summary" / "RUN.json"
    incomplete_path = pack / "00_summary" / "INCOMPLETE.json"
    gate_path = pack / "00_summary" / "MERGE_GATE.json"
    run = read_json(run_path)
    gate = read_json(gate_path)
    resources = (run or {}).get("resources", {})
    cap = resources.get("child_worker_limit")
    violations = receipt["violations"]

    log_text = log.read_text(encoding="utf-8", errors="replace") if log.exists() else ""
    transitions = {
        "queued": "Queued:" in log_text,
        "running": "Running:" in log_text,
        "schedule": "Schedule:" in log_text,
    }
    receipt["cli_trace"] = transitions
    receipt["run_resources"] = resources
    receipt["pack"] = {
        "run_json": run_path.exists(),
        "sanity_json": (pack / "00_summary" / "SANITY.json").exists(),
        "merge_gate_json": (pack / "00_summary" / "MERGE_GATE.json").exists(),
        "incomplete_json": incomplete_path.exists(),
        "incomplete": read_json(incomplete_path),
    }

    add_assertion(
        violations, receipt["process"]["exit_code"] == 0, "prview did not exit 0"
    )
    add_assertion(
        violations,
        not receipt["process"]["timed_out"],
        "prview exceeded the harness timeout",
    )
    add_assertion(violations, bool(run), "final RUN.json is missing or invalid")
    add_assertion(
        violations, not incomplete_path.exists(), "run left an INCOMPLETE.json marker"
    )
    add_assertion(
        violations,
        resources.get("requested_budget") == "safe",
        "RUN requested budget is not safe",
    )
    add_assertion(
        violations,
        resources.get("effective_budget") == "safe",
        "RUN effective budget is not safe",
    )
    add_assertion(
        violations,
        resources.get("parent_permits") == 1,
        "safe parent permit count is not one",
    )
    add_assertion(violations, cap == 1, "safe child worker cap is not one")
    add_assertion(
        violations,
        "--deep" in (receipt.get("command") or []),
        "acceptance command is not a --deep review",
    )
    add_assertion(
        violations,
        "--resource-budget" not in (receipt.get("command") or []),
        "acceptance command overrides the CLI default resource budget",
    )
    receipt["run_checks"] = [
        {
            "name": row.get("name"),
            "status": row.get("status"),
            "cached": row.get("cached"),
        }
        for row in (run or {}).get("checks") or []
        if isinstance(row, dict)
    ]
    for tool in WHOLE_MACHINE_TOOLS:
        add_assertion(
            violations,
            census["seen_tools"][tool],
            f"no real {tool} process was observed",
        )
        check_name = REQUIRED_RUN_CHECKS[tool]
        add_assertion(
            violations,
            has_successful_live_check(run, check_name),
            f"RUN.json does not contain a live successful {check_name} gate",
        )
    for tool in REQUIRED_LIVE_CHECKS_ONLY:
        check_name = REQUIRED_RUN_CHECKS[tool]
        add_assertion(
            violations,
            has_successful_live_check(run, check_name),
            f"RUN.json does not contain a live successful {check_name} gate",
        )
    add_assertion(
        violations,
        census["max_whole_machine_parents"] <= 1,
        "more than one whole-machine tool was active",
    )
    if isinstance(cap, int):
        add_assertion(
            violations,
            census["max_descendants"]["rustc"] <= cap,
            "rustc pool exceeded the selected cap",
        )
        add_assertion(
            violations,
            census["max_descendants"]["vitest_workers"] <= cap,
            "Vitest worker pool exceeded the selected cap",
        )
        add_assertion(
            violations,
            census["max_descendants"]["semgrep_core"] <= cap,
            "Semgrep worker pool exceeded the selected cap",
        )
    if platform.system() == "Linux":
        add_assertion(
            violations,
            census["observed_caps"]["cargo_build_jobs"] == {"1"},
            "Cargo processes did not consistently expose CARGO_BUILD_JOBS=1",
        )
    add_assertion(
        violations,
        census["observed_caps"]["vitest_max_workers"] == {"1"},
        "Vitest did not expose --maxWorkers 1",
    )
    add_assertion(
        violations,
        census["observed_caps"]["semgrep_jobs"] == {"1"},
        "Semgrep did not expose --jobs 1",
    )
    add_assertion(
        violations,
        all(transitions.values()),
        "CLI did not show schedule plus Queued/Running truth",
    )
    add_assertion(
        violations,
        receipt["pack"]["sanity_json"] and receipt["pack"]["merge_gate_json"],
        "final SANITY.json or MERGE_GATE.json is missing",
    )

    # What this case is actually here to prove, plus the evidence in readable
    # form: the receipt should let a reader check the claim without the pack.
    receipt["scope_evidence"] = {
        "cargo_test": {
            "scope": scope_of(run, CARGO_TEST_CHECK) or None,
            "command": gate_command(pack, "cargo_test") or None,
        },
        "vitest": {
            "scope": scope_of(run, VITEST_CHECK) or None,
            "command": gate_command(pack, "tests") or None,
        },
        "review_caveats": ((gate or {}).get("decision") or {}).get("review_caveats"),
        "verdict": ((gate or {}).get("decision") or {}).get("verdict"),
        "cargo_test_processes": observed_cargo_test_commands(census),
        "cargo_invocations": census.get("cargo_invocations") or [],
        "census_truncated": bool(census.get("truncated")),
    }
    case["assert_scope"](violations, run, gate, pack, census)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path)
    parser.add_argument("--fixture", type=pathlib.Path)
    parser.add_argument("--receipt-dir", required=True, type=pathlib.Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--timeout-seconds", type=int, default=1200)
    parser.add_argument("--initialize-only", action="store_true")
    parser.add_argument(
        "--case",
        action="append",
        dest="cases",
        choices=list(CASES),
        help=(
            "Which acceptance case to run; repeatable. Each case gets its own "
            "fixture history and its own receipt. Default: all of them."
        ),
    )
    return parser.parse_args()


def new_receipt(case_name: str, source_sha: str) -> dict[str, Any]:
    """A receipt that reads as a failure until the harness proves otherwise."""
    return {
        "schema": "prview.bounded-runtime-acceptance.v2",
        "case": case_name,
        "status": "failed",
        "source_sha": source_sha,
        "started_at": utc_now(),
        "finished_at": None,
        "machine": machine_observation(),
        "command": None,
        "process": {"exit_code": None, "timed_out": False, "termination": None},
        "census": None,
        "violations": [],
    }


def write_receipt(path: pathlib.Path, receipt: dict[str, Any]) -> None:
    path.write_text(
        json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def run_case(args: argparse.Namespace, case_name: str) -> dict[str, Any]:
    """Run one acceptance case end to end and write its receipt."""
    case = CASES[case_name]
    receipt_dir = args.receipt_dir / case_name
    receipt_dir.mkdir(parents=True, exist_ok=True)
    receipt_path = receipt_dir / "receipt.json"
    log_path = receipt_dir / "prview.log"
    receipt = new_receipt(case_name, args.source_sha)

    process: subprocess.Popen[str] | None = None
    census: dict[str, Any] = {
        "samples": 0,
        "started_monotonic": time.monotonic(),
        "last_signature": None,
        "max_whole_machine_parents": 0,
        "max_descendants": {
            "rustc": 0,
            "vitest_workers": 0,
            "semgrep_core": 0,
            "semgrep_core_processes": 0,
        },
        "seen_tools": {tool: False for tool in WHOLE_MACHINE_TOOLS},
        "observed_commands": set(),
        "seen_pids": set(),
        "truncated": False,
        "cargo_invocations": [],
        "observed_caps": {
            "cargo_build_jobs": set(),
            "vitest_max_workers": set(),
            "semgrep_jobs": set(),
        },
        "transitions": [],
    }

    try:
        source_root = pathlib.Path(__file__).resolve().parent.parent
        receipt["source_tree"] = source_tree_observation(source_root)
        binary = (
            args.binary.resolve()
            if args.binary is not None and args.binary.is_file()
            else None
        )
        receipt["binary"] = binary_observation(binary) if binary is not None else None
        add_assertion(
            receipt["violations"],
            re.fullmatch(r"[0-9a-fA-F]{40}", args.source_sha) is not None,
            "source SHA is not exact",
        )
        add_assertion(
            receipt["violations"],
            receipt["source_tree"]["head_sha"] == args.source_sha,
            "source SHA does not match the repository HEAD",
        )
        add_assertion(
            receipt["violations"],
            receipt["source_tree"]["dirty"] is False,
            "source repository is dirty",
        )
        add_assertion(
            receipt["violations"],
            binary is not None,
            "release binary is missing",
        )
        add_assertion(
            receipt["violations"],
            receipt["binary"] is not None
            and receipt["binary"]["probe_exit_code"] == 0,
            "release binary source probe failed",
        )
        add_assertion(
            receipt["violations"],
            receipt["binary"] is not None
            and str(receipt["binary"]["embedded_source_sha"] or "").lower()
            == args.source_sha.lower(),
            "release binary was not built from the requested source SHA",
        )
        add_assertion(
            receipt["violations"],
            args.fixture is not None and args.fixture.is_dir(),
            "fixture directory is missing",
        )
        if receipt["violations"]:
            raise RuntimeError("invalid harness inputs")
        with tempfile.TemporaryDirectory(prefix="prview-bounded-runtime-") as temp:
            work = pathlib.Path(temp)
            repo = prepare_fixture(
                args.fixture.resolve(), work, log_path, case["mutate"]
            )
            pack = repo / ".acceptance-pack"
            command = [
                str(binary),
                "--deep",
                "--no-cache",
                "--no-fetch",
                "--local-only",
                "--no-dashboard",
                "--no-zip",
                "--output-dir",
                str(pack),
                *case.get("extra_args", []),
                "candidate",
                "main",
            ]
            receipt["command"] = command
            env = os.environ.copy()
            env.update({"CI": "true", "NO_COLOR": "1"})
            # Canonical evidence for "which cargo commands ran". Installed last,
            # so PATH resolution for the real cargo happened against the
            # machine's own PATH.
            cargo_log = install_cargo_shim(work, env)
            with log_path.open("a", encoding="utf-8") as stream:
                stream.write(f"$ {' '.join(command)}\n")
                stream.flush()
                started = time.monotonic()
                process = subprocess.Popen(
                    command,
                    cwd=repo,
                    stdout=stream,
                    stderr=subprocess.STDOUT,
                    text=True,
                    env=env,
                    start_new_session=True,
                )
                deadline = started + args.timeout_seconds
                while process.poll() is None and time.monotonic() < deadline:
                    sample_owned_tree(process.pid, census)
                    time.sleep(0.05)
                if process.poll() is None:
                    receipt["process"]["timed_out"] = True
                    termination = cancel_after_timeout(process, census)
                    receipt["process"]["termination"] = termination
                    receipt["process"]["exit_code"] = termination["exit_code"]
                else:
                    receipt["process"]["exit_code"] = process.wait()
                receipt["process"]["wall_secs"] = round(time.monotonic() - started, 3)
            census["cargo_invocations"] = cargo_shim_invocations(cargo_log)
            case.get("evaluate", evaluate)(receipt, census, pack, log_path, case)
    finally:
        error = sys.exc_info()[1]
        if error is not None:
            receipt["violations"].append(
                f"harness error: {type(error).__name__}: {error}"
            )
        if process is not None and process.poll() is None:
            receipt["process"]["exit_code"] = force_stop(process)
        serializable_census = dict(census)
        serializable_census.pop("started_monotonic", None)
        serializable_census.pop("last_signature", None)
        serializable_census["observed_caps"] = {
            key: sorted(value) for key, value in census["observed_caps"].items()
        }
        serializable_census["observed_commands"] = sorted(census["observed_commands"])
        serializable_census["seen_pids"] = sorted(census["seen_pids"])
        receipt["census"] = serializable_census
        receipt["finished_at"] = utc_now()
        if not receipt["violations"]:
            receipt["status"] = "success"
        write_receipt(receipt_path, receipt)

    receipt["receipt_path"] = str(receipt_path)
    return receipt


def main() -> int:
    args = parse_args()
    selected = args.cases or list(CASES)
    args.receipt_dir.mkdir(parents=True, exist_ok=True)

    if args.initialize_only:
        # A failure-shaped receipt per case, so a run killed before it finishes
        # leaves evidence of every case it owed rather than silence.
        summary = []
        for case_name in selected:
            receipt_dir = args.receipt_dir / case_name
            receipt_dir.mkdir(parents=True, exist_ok=True)
            receipt = new_receipt(case_name, args.source_sha)
            receipt["violations"] = ["acceptance harness did not complete"]
            receipt["finished_at"] = utc_now()
            path = receipt_dir / "receipt.json"
            write_receipt(path, receipt)
            summary.append(
                {
                    "case": case_name,
                    "status": receipt["status"],
                    "receipt": str(path),
                }
            )
        print(json.dumps({"cases": summary}))
        return 0

    results = []
    for case_name in selected:
        receipt = run_case(args, case_name)
        results.append(
            {
                "case": case_name,
                "status": receipt["status"],
                "receipt": receipt.get("receipt_path"),
                "wall_secs": receipt["process"].get("wall_secs"),
                "violations": receipt["violations"],
            }
        )
        # Printed as each case finishes: a later case failing must not hide an
        # earlier case's evidence behind a killed process.
        print(json.dumps(results[-1]), flush=True)

    failed = [result["case"] for result in results if result["status"] != "success"]
    print(json.dumps({"cases": results, "failed": failed}))
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
