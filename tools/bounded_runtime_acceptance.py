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
REQUIRED_RUN_CHECKS = {
    "cargo": "Cargo check",
    "vitest": "Vitest",
    "semgrep": "Semgrep scan",
    "tsc": "TypeScript",
    "eslint": "ESLint",
    "stylelint": "Stylelint",
}


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


def prepare_fixture(
    source: pathlib.Path, work: pathlib.Path, log: pathlib.Path
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
    math = repo / "src" / "math.js"
    math.write_text(
        "export function add(left, right) {\n"
        "  return Number(left) + Number(right);\n"
        "}\n",
        encoding="utf-8",
    )
    run_checked(["git", "add", "src/math.js"], repo, log)
    run_checked(["git", "commit", "-m", "fix: normalize numeric inputs"], repo, log)
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


def evaluate(
    receipt: dict[str, Any],
    census: dict[str, Any],
    pack: pathlib.Path,
    log: pathlib.Path,
) -> None:
    run_path = pack / "00_summary" / "RUN.json"
    incomplete_path = pack / "00_summary" / "INCOMPLETE.json"
    run = read_json(run_path)
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


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=pathlib.Path)
    parser.add_argument("--fixture", type=pathlib.Path)
    parser.add_argument("--receipt-dir", required=True, type=pathlib.Path)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--timeout-seconds", type=int, default=1200)
    parser.add_argument("--initialize-only", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    args.receipt_dir.mkdir(parents=True, exist_ok=True)
    receipt_path = args.receipt_dir / "receipt.json"
    log_path = args.receipt_dir / "prview.log"
    receipt: dict[str, Any] = {
        "schema": "prview.bounded-runtime-acceptance.v2",
        "status": "failed",
        "source_sha": args.source_sha,
        "started_at": utc_now(),
        "finished_at": None,
        "machine": machine_observation(),
        "command": None,
        "process": {"exit_code": None, "timed_out": False, "termination": None},
        "census": None,
        "violations": [],
    }
    if args.initialize_only:
        receipt["violations"] = ["acceptance harness did not complete"]
        receipt["finished_at"] = utc_now()
        receipt_path.write_text(
            json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(json.dumps({"status": receipt["status"], "receipt": str(receipt_path)}))
        return 0

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
            repo = prepare_fixture(args.fixture.resolve(), work, log_path)
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
                "candidate",
                "main",
            ]
            receipt["command"] = command
            env = os.environ.copy()
            env.update({"CI": "true", "NO_COLOR": "1"})
            with log_path.open("a", encoding="utf-8") as stream:
                stream.write(f"$ {' '.join(command)}\n")
                stream.flush()
                process = subprocess.Popen(
                    command,
                    cwd=repo,
                    stdout=stream,
                    stderr=subprocess.STDOUT,
                    text=True,
                    env=env,
                    start_new_session=True,
                )
                deadline = time.monotonic() + args.timeout_seconds
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
            evaluate(receipt, census, pack, log_path)
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
        receipt["census"] = serializable_census
        receipt["finished_at"] = utc_now()
        if not receipt["violations"]:
            receipt["status"] = "success"
        receipt_path.write_text(
            json.dumps(receipt, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    print(
        json.dumps(
            {
                "status": receipt["status"],
                "receipt": str(receipt_path),
                "violations": receipt["violations"],
            }
        )
    )
    return 0 if receipt["status"] == "success" else 1


if __name__ == "__main__":
    sys.exit(main())
