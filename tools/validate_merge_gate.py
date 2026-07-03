#!/usr/bin/env python3
"""Validate MERGE_GATE.json contract (schema 1.0/2.0/2.1)."""

from __future__ import annotations

import json
import sys
from datetime import datetime
from pathlib import Path
from typing import Any, Iterable


VALID_CLASSES = {"PASS", "SKIP", "FAIL", "INFO"}
VALID_SEVERITIES = {"block", "warn", "ignore"}
VALID_MODES = {"shadow", "warn", "block"}
VALID_PROFILES = {"auto", "js", "rust", "python", "mixed", "generic"}
# Verdict vocabulary is unified on PASS/CONDITIONAL/BLOCK (PV-03/04). The former
# `HOLD` synonym is retired: legacy_verdict() now emits CONDITIONAL for every
# review-required/degraded run, so a freshly generated gate never carries HOLD.
VALID_VERDICTS = {"PASS", "CONDITIONAL", "BLOCK"}


def err(msg: str) -> None:
    print(f"ERROR: {msg}", file=sys.stderr)


def ensure_keys(obj: dict[str, Any], keys: Iterable[str], ctx: str) -> list[str]:
    missing = [k for k in keys if k not in obj]
    return [f"{ctx}: missing key '{k}'" for k in missing]


def require_non_empty_string(value: Any, ctx: str, issues: list[str]) -> None:
    if not isinstance(value, str) or not value.strip():
        issues.append(f"{ctx} must be a non-empty string")


def require_boolean(value: Any, ctx: str, issues: list[str]) -> None:
    if not isinstance(value, bool):
        issues.append(f"{ctx} must be boolean")


def require_non_negative_number(value: Any, ctx: str, issues: list[str]) -> None:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or value < 0:
        issues.append(f"{ctx} must be a non-negative number")


def require_non_negative_integer(value: Any, ctx: str, issues: list[str]) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        issues.append(f"{ctx} must be a non-negative integer")


def require_iso_datetime(value: Any, ctx: str, issues: list[str]) -> None:
    if not isinstance(value, str) or not value.strip():
        issues.append(f"{ctx} must be an ISO datetime string")
        return

    normalized = value
    if value.endswith("Z"):
        normalized = f"{value[:-1]}+00:00"

    try:
        datetime.fromisoformat(normalized)
    except ValueError:
        issues.append(f"{ctx} must be an ISO datetime string")


def normalize_inline_status(value: Any) -> str | None:
    if not isinstance(value, str) or not value.strip():
        return None

    return value.strip().replace("_", "").lower()


def validate(path: Path) -> list[str]:
    issues: list[str] = []
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:  # noqa: BLE001
        return [f"invalid JSON: {exc}"]

    if not isinstance(data, dict):
        return ["root must be an object"]

    issues.extend(
        ensure_keys(
            data,
            [
                "schema_version",
                "generated_at",
                "bridge_stage",
                "target",
                "bases",
                "profile",
                "policy",
                "checks",
                "inline_findings",
                "decision",
                "files",
            ],
            "root",
        )
    )
    if issues:
        return issues

    if not isinstance(data["schema_version"], str):
        issues.append("schema_version must be a string")
    elif data["schema_version"] not in ("1.0", "2.0", "2.1"):
        issues.append("schema_version must be '1.0', '2.0', or '2.1'")
    require_iso_datetime(data["generated_at"], "generated_at", issues)
    if (
        isinstance(data["bridge_stage"], bool)
        or not isinstance(data["bridge_stage"], int)
        or not (0 <= data["bridge_stage"] <= 4)
    ):
        issues.append("bridge_stage must be integer in range 0..4")

    require_non_empty_string(data["target"], "target", issues)
    if not isinstance(data["bases"], list):
        issues.append("bases must be an array of non-empty strings")
    elif not data["bases"]:
        issues.append("bases must contain at least one ref")
    else:
        for idx, base in enumerate(data["bases"]):
            require_non_empty_string(base, f"bases[{idx}]", issues)

    profile = data["profile"]
    require_non_empty_string(profile, "profile", issues)
    if isinstance(profile, str) and profile.strip().lower() not in VALID_PROFILES:
        issues.append("profile must be one of auto|js|rust|python|mixed|generic")

    policy = data["policy"]
    if not isinstance(policy, dict):
        issues.append("policy must be an object")
    else:
        issues.extend(
            ensure_keys(policy, ["version", "mode", "default_severity", "source"], "policy")
        )
        if isinstance(policy.get("version"), bool) or not isinstance(policy.get("version"), int):
            issues.append("policy.version must be integer")
        elif policy["version"] < 1:
            issues.append("policy.version must be >= 1")
        mode = policy.get("mode")
        severity = policy.get("default_severity")
        if mode not in VALID_MODES:
            issues.append("policy.mode must be one of shadow|warn|block")
        if severity not in VALID_SEVERITIES:
            issues.append("policy.default_severity must be one of block|warn|ignore")
        require_non_empty_string(policy.get("source"), "policy.source", issues)

    checks = data["checks"]
    if not isinstance(checks, list):
        issues.append("checks must be an array")
    else:
        for idx, check in enumerate(checks):
            ctx = f"checks[{idx}]"
            if not isinstance(check, dict):
                issues.append(f"{ctx} must be an object")
                continue
            issues.extend(
                ensure_keys(
                    check,
                    [
                        "id",
                        "name",
                        "status",
                        "class",
                        "severity",
                        "blocking",
                        "duration_secs",
                        "evidence",
                    ],
                    ctx,
                )
            )
            require_non_empty_string(check.get("id"), f"{ctx}.id", issues)
            require_non_empty_string(check.get("name"), f"{ctx}.name", issues)
            require_non_empty_string(check.get("status"), f"{ctx}.status", issues)
            if check.get("class") not in VALID_CLASSES:
                issues.append(f"{ctx}.class must be one of {sorted(VALID_CLASSES)}")
            if check.get("severity") not in VALID_SEVERITIES:
                issues.append(f"{ctx}.severity must be one of {sorted(VALID_SEVERITIES)}")
            require_boolean(check.get("blocking"), f"{ctx}.blocking", issues)
            require_non_negative_number(check.get("duration_secs"), f"{ctx}.duration_secs", issues)
            require_non_empty_string(check.get("evidence"), f"{ctx}.evidence", issues)

    inline = data["inline_findings"]
    if not isinstance(inline, dict):
        issues.append("inline_findings must be an object")
    else:
        issues.extend(
            ensure_keys(
                inline,
                ["file", "status", "severity", "blocking", "findings_count"],
                "inline_findings",
            )
        )
        if normalize_inline_status(inline.get("status")) not in {
            "passed",
            "warnings",
            "failed",
            "notrun",
        }:
            issues.append(
                "inline_findings.status must be one of passed|warnings|failed|not_run "
                "(case-insensitive, '_' optional)"
            )
        if inline.get("severity") not in VALID_SEVERITIES:
            issues.append("inline_findings.severity must be one of block|warn|ignore")
        findings_count = inline.get("findings_count")
        inline_file = inline.get("file")
        if findings_count == 0:
            if inline_file is not None:
                issues.append("inline_findings.file must be null when findings_count is 0")
        else:
            require_non_empty_string(inline_file, "inline_findings.file", issues)
            if isinstance(inline_file, str) and not inline_file.endswith("INLINE_FINDINGS.sarif"):
                issues.append("inline_findings.file must end with 'INLINE_FINDINGS.sarif'")
        require_boolean(inline.get("blocking"), "inline_findings.blocking", issues)
        require_non_negative_integer(
            findings_count, "inline_findings.findings_count", issues
        )

    decision = data["decision"]
    if not isinstance(decision, dict):
        issues.append("decision must be an object")
    else:
        issues.extend(
            ensure_keys(
                decision,
                ["verdict", "allow_merge", "decision_reason", "blocking_issues"],
                "decision",
            )
        )
        verdict = decision.get("verdict")
        if verdict not in VALID_VERDICTS:
            issues.append(f"decision.verdict must be one of {sorted(VALID_VERDICTS)}")
        require_boolean(decision.get("allow_merge"), "decision.allow_merge", issues)
        require_non_empty_string(decision.get("decision_reason"), "decision.decision_reason", issues)
        # allow_merge is a DERIVED field (schema 2.1, PV-03): it is true iff the
        # verdict is a clean PASS, so the contradictory state `allow_merge: true`
        # beside a CONDITIONAL/BLOCK verdict is rejected here as a schema break.
        allow_merge = decision.get("allow_merge")
        if isinstance(allow_merge, bool) and verdict in VALID_VERDICTS:
            if allow_merge != (verdict == "PASS"):
                issues.append(
                    "decision.allow_merge must equal (verdict == 'PASS'): "
                    f"got allow_merge={allow_merge} with verdict={verdict!r}"
                )
        if not isinstance(decision.get("blocking_issues"), list):
            issues.append("decision.blocking_issues must be an array")
        else:
            for idx, issue in enumerate(decision["blocking_issues"]):
                require_non_empty_string(issue, f"decision.blocking_issues[{idx}]", issues)
            # A hard block must never permit merge.
            if decision["blocking_issues"] and allow_merge:
                issues.append(
                    "decision.allow_merge must be false when blocking_issues is non-empty"
                )

    files = data["files"]
    if not isinstance(files, dict):
        issues.append("files must be an object")
    else:
        issues.extend(
            ensure_keys(
                files,
                ["merge_gate_json", "inline_findings"],
                "files",
            )
        )
        expected_suffixes = {
            "merge_gate_json": "MERGE_GATE.json",
        }
        for key, expected in expected_suffixes.items():
            value = files.get(key)
            require_non_empty_string(value, f"files.{key}", issues)
            if isinstance(value, str) and not value.endswith(expected):
                issues.append(f"files.{key} must end with '{expected}'")
        inline_path = files.get("inline_findings")
        findings_count = (
            data.get("inline_findings", {}).get("findings_count")
            if isinstance(data.get("inline_findings"), dict)
            else None
        )
        if findings_count == 0:
            if inline_path is not None:
                issues.append("files.inline_findings must be null when findings_count is 0")
        else:
            require_non_empty_string(inline_path, "files.inline_findings", issues)
            if isinstance(inline_path, str) and not inline_path.endswith("INLINE_FINDINGS.sarif"):
                issues.append("files.inline_findings must end with 'INLINE_FINDINGS.sarif'")

    return issues


def main() -> int:
    if len(sys.argv) != 2:
        err("usage: validate_merge_gate.py <path-to-MERGE_GATE.json>")
        return 2

    path = Path(sys.argv[1]).expanduser()
    if not path.exists():
        err(f"file not found: {path}")
        return 2

    issues = validate(path)
    if issues:
        for issue in issues:
            err(issue)
        return 1

    print(f"OK: {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
