#!/usr/bin/env python3
"""Validate MERGE_GATE.json contract (schema 1.0/2.0/2.1/2.2/2.3)."""

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
# schema 2.2: quality_failure_details entries name which check status produced
# them. Only "failure" entries may fail the quality gate, so a consumer told to
# filter on origin == "failure" cannot do that if the field is absent, mistyped,
# or spelled something else.
VALID_QUALITY_FAILURE_ORIGINS = {"failure", "warning"}
# The other two fields of the same entry. `name` is what a consumer reports and
# what the per-classification arrays are keyed by, so an empty one names no
# check at all; `classification` decides whether the entry gated this diff, and
# only `introduced`/`mixed`/`unclassified` do. Mirrors
# `QualityFailureClass::as_str` in src/artifacts/verdict.rs -- note the hyphen in
# `pre-existing`, which the sibling COUNT field spells `preexisting_*`.
VALID_QUALITY_FAILURE_CLASSES = {
    "introduced",
    "pre-existing",
    "mixed",
    "unclassified",
}
# Every spelling a check status is emitted as. Mirrors `CheckStatus::EMITTED` in
# src/checks/mod.rs, which a test there pins to `CheckStatus::as_str`.
#
# Case-SENSITIVE, unlike inline_findings.status below. The CLI counts warnings by
# comparing this vocabulary exactly, so a pack spelling `WARNINGS` is one the
# reader cannot read; accepting it here would certify an artifact that makes
# `--ci --fail-on-warnings` report a warning it cannot attribute. The inline
# field folds case because its writer has shipped legacy spellings; this one has
# only ever emitted lowercase.
VALID_CHECK_STATUSES = {"passed", "failed", "warnings", "skipped", "error"}
VALID_EXECUTION_STATES = {"executed", "skipped", "unavailable", "unknown"}
VALID_TOOL_OUTCOMES = {
    "passed",
    "findings_failed",
    "findings_warning",
    "system_error",
    "skipped",
    "unavailable",
    "unknown",
}
VALID_POLICY_CONCLUSIONS = {"satisfied", "advisory", "blocked"}
VALID_CONFIDENCE_IMPACTS = {"complete", "degraded", "incomplete"}
VALID_MERGE_IMPACTS = {"approve", "review_required", "block"}
# The two enum axes of `decision`, spelled exactly as serde writes them. Mirror
# `AnalysisStatus` and `MergeRecommendation` in src/policy/engine.rs -- both
# `#[serde(rename_all = "snake_case")]`, and a test there pins every variant to
# the spelling below.
#
# Case-SENSITIVE and canonical-only, like VALID_CHECK_STATUSES and unlike the
# READERS, which fold case and still accept the retired `hold` spelling when
# reading a pack off disk. That tolerance exists for artifacts already written;
# this file certifies freshly emitted ones, and the 2.2 emitter has only ever
# written these.
VALID_ANALYSIS_STATUSES = {"complete", "degraded", "incomplete"}
VALID_MERGE_RECOMMENDATIONS = {"approve", "review_required", "block"}
# Schema 2.3 separates strict enforcement from the stable verdict vocabulary.
# Mirrors `EnforcementDisposition` in src/policy/engine.rs.
VALID_ENFORCEMENT_DISPOSITIONS = {
    "clean",
    "warnings_only",
    "review_required",
    "block",
}
# Conservativeness rank of one decision axis: 1 = clean pass, 2 = held below a
# pass, 3 = blocked. Mirrors `rank_from_verdict`, `rank_from_merge_rec` and
# `rank_from_analysis_status` in src/gate.rs, which both readers reconcile
# through.
#
# MEMBERSHIP RULE: an axis ranks only when its value RULES OUT a milder outcome.
# `complete` and `quality_pass: true` are preconditions of PASS, not grants of
# it -- a complete, quality-clean run is still held at CONDITIONAL by a
# review-required recommendation -- so neither states a rank, and neither
# appears in these tables.
VERDICT_RANK = {"PASS": 1, "CONDITIONAL": 2, "BLOCK": 3}
MERGE_RECOMMENDATION_RANK = {"approve": 1, "review_required": 2, "block": 3}
ANALYSIS_STATUS_RANK = {"degraded": 2, "incomplete": 2}
# Mirrors `verdict_from_rank`: the word the readers publish for a rank.
VERDICT_FROM_RANK = {rank: verdict for verdict, rank in VERDICT_RANK.items()}


def schema_at_least(raw: Any, minimum: tuple[int, int]) -> bool:
    """Whether `raw` is a canonical MAJOR.MINOR at or above `minimum`.

    Mirrors the reader in `src/gate.rs`: components are compared as written, so
    a non-canonical spelling is never treated as a version at all.
    """
    if not isinstance(raw, str):
        return False
    parts = raw.split(".")
    if len(parts) != 2 or not all(p.isdigit() and (p == "0" or not p.startswith("0")) for p in parts):
        return False
    return (int(parts[0]), int(parts[1])) >= minimum


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


def check_quality_pass_agrees_with_details(
    decision: dict[str, Any], details: list[Any]
) -> list[str]:
    """Cross-check `quality_pass` against the details it is computed from.

    `quality_pass` is not an independent opinion. The writer sets it to
    `!QualityFailureSummary::has_new_failures()` and serializes the very same
    details 1:1 into `quality_failure_details`, so the two are one fact written
    twice and the check is an EQUIVALENCE, enforced in both directions.

    An entry gates the diff when its origin is `failure` AND its classification
    is anything but `pre-existing`. Both halves matter, and the second is the
    reason the obvious rule -- "a failure-origin entry forces quality_pass
    false" -- is WRONG: a purely pre-existing failure is emitted next to
    `quality_pass: true` on purpose, because it predates the diff and must not
    block the merge. `security_full_preexisting_semgrep_finding_is_advisory_only`
    in src/artifacts/merge_gate.rs produces exactly that pack, and rejecting it
    would make the validator cry wolf on a genuine one -- which costs more than
    the hole it closes, because a validator nobody trusts gates nothing.

    Only entries whose own shape already validated are counted, so a malformed
    detail is reported once as a shape error rather than twice.
    """
    quality_pass = decision.get("quality_pass")
    if not isinstance(quality_pass, bool):
        # From 2.2 the caller has already required a boolean, so a non-boolean
        # is reported once as a type error rather than twice. Below 2.2 this
        # function is not reached at all: absence there is an old pack, not a
        # contradiction, and there is no cross-field claim to check.
        return []
    gating = [
        detail.get("name")
        for detail in details
        if isinstance(detail, dict)
        and detail.get("origin") == "failure"
        and detail.get("classification") in VALID_QUALITY_FAILURE_CLASSES
        and detail.get("classification") != "pre-existing"
    ]
    if quality_pass and gating:
        return [
            "decision.quality_pass must be false when a quality_failure_details "
            "entry has origin 'failure' and a classification other than "
            f"'pre-existing': got quality_pass=true with {sorted(map(str, gating))}"
        ]
    if not quality_pass and not gating:
        return [
            "decision.quality_pass must be true when no quality_failure_details "
            "entry has origin 'failure' with a classification other than "
            "'pre-existing': got quality_pass=false with no such entry"
        ]
    return []


def check_blocker_flag_agrees_with_blocking_issues(decision: dict[str, Any]) -> list[str]:
    """Cross-check `policy_allow_merge` against the list it is computed from.

    Like `quality_pass`, this flag is not an independent opinion: the writer sets
    `policy_allow_merge = blocking_issues.is_empty()` (src/artifacts/merge_gate.rs)
    AFTER the last push to that list, and then serializes both verbatim into the
    same `json!` literal. Nothing else writes the field -- the other computation
    of the same formula, in src/artifacts/context.rs, feeds the dashboard, not
    this artifact. So the two are one fact written twice and the check is an
    EQUIVALENCE, enforced in both directions:
    `policy_allow_merge: true` beside real blockers, and `false` beside none,
    are equally unemittable.

    The reconciliation below already reads the pair -- but only in the harsher
    direction, where either half raises the rank the verdict must clear. That
    leaves the two halves free to contradict each other outright, which is the
    hole this closes: a pack whose blockers say "blocked" and whose flag says
    "policy let it through" certifies a state prview never produced, and readers
    that trust one half read the opposite of readers that trust the other.

    Reached only from schema 2.2, where both fields are required; below it,
    absence is an old pack rather than a contradiction and no cross-field claim
    exists to check.
    """
    policy_allow_merge = decision.get("policy_allow_merge")
    blocking_issues = decision.get("blocking_issues")
    if not isinstance(policy_allow_merge, bool) or not isinstance(blocking_issues, list):
        # Type errors are reported once, by the shape checks that own them.
        return []
    if policy_allow_merge and blocking_issues:
        return [
            "decision.policy_allow_merge must be false when blocking_issues is "
            "non-empty -- the writer derives the flag from that list: got "
            f"policy_allow_merge=true with {len(blocking_issues)} blocking_issues"
        ]
    if not policy_allow_merge and not blocking_issues:
        return [
            "decision.policy_allow_merge must be true when blocking_issues is "
            "empty -- the writer derives the flag from that list: got "
            "policy_allow_merge=false with no blocking_issues"
        ]
    return []


def check_decision_axes_agree_on_the_verdict(decision: dict[str, Any]) -> list[str]:
    """Reject a `verdict` milder than the axes stated beside it.

    Both readers reconcile a decision the same way: take the MAX rank across the
    axes the pack states, then publish every axis from that one number. So a
    verdict below that maximum is not a verdict any reader will honour -- the
    pack certifies one outcome and every consumer of it computes another. The
    reported hole was exactly that: `verdict: "PASS"` beside
    `analysis_status: "incomplete"`, `merge_recommendation: "block"` and
    `policy_allow_merge: false` validated OK, so an artifact both readers
    normalize to BLOCK carried a green certification.

    The emitter cannot produce such a pack. It derives `verdict` through
    `MergeRecommendation::legacy_verdict`, whose result is the same maximum:

      * `block` -> `BLOCK`                : rank 3
      * `review_required` -> `CONDITIONAL`: rank 2
      * `approve` + complete + quality    : rank 1 (`PASS`)
      * `approve` + degraded/quality fail : rank 2 (`CONDITIONAL`)

    `allow_merge` is `verdict == "PASS"`, so it never exceeds the verdict it
    sits beside; and `blocking_issues`/`policy_allow_merge` rank 3 because a
    blocking issue is pushed only for a check whose `PolicyConclusion` is
    `Blocked`, whose `merge_impact` is `Block` -- a stated blocker IS a stated
    `block` recommendation.

    DELIBERATE LIMIT: only the permissive direction is rejected. A verdict
    HARSHER than its other axes is legal -- a semgrep scan that passes with
    parse errors leaves `merge_recommendation: "approve"` beside
    `analysis_status: "degraded"`, which the contract turns into `CONDITIONAL`
    -- so "verdict equals the max of the OTHER axes" would reject a pack the
    emitter really writes. A harsher verdict also fools nobody: every reader
    publishes it as stated. It is the milder direction that certifies a
    permission the artifact never earned.

    Only axes whose own type and vocabulary already validated are ranked, so a
    malformed axis is reported once as a shape error rather than twice. An axis
    the pack does not state is not ranked either -- absence states nothing, and
    from 2.2 the caller has already required every axis this reads.
    """
    verdict = decision.get("verdict")
    verdict_rank = VERDICT_RANK.get(verdict) if isinstance(verdict, str) else None
    if verdict_rank is None:
        return []

    stated: list[tuple[str, int]] = [(f"verdict={verdict!r}", verdict_rank)]

    recommendation = decision.get("merge_recommendation")
    if isinstance(recommendation, str) and recommendation in MERGE_RECOMMENDATION_RANK:
        stated.append(
            (
                f"merge_recommendation={recommendation!r}",
                MERGE_RECOMMENDATION_RANK[recommendation],
            )
        )

    allow_merge = decision.get("allow_merge")
    if isinstance(allow_merge, bool):
        stated.append((f"allow_merge={allow_merge}", 1 if allow_merge else 2))

    # Only the FALSE of these two states a rank -- see the membership rule above.
    if decision.get("quality_pass") is False:
        stated.append(("quality_pass=False", 2))

    analysis_status = decision.get("analysis_status")
    if isinstance(analysis_status, str) and analysis_status in ANALYSIS_STATUS_RANK:
        stated.append(
            (
                f"analysis_status={analysis_status!r}",
                ANALYSIS_STATUS_RANK[analysis_status],
            )
        )

    # The blocker axis, stated twice by the emitter
    # (`policy_allow_merge = blocking_issues.is_empty()`). It is ONE axis: a pack
    # carrying both must not count it twice, and one carrying either is covered.
    # That the two halves actually AGREE is not this rule's job -- ranking only
    # asks how conservative the pack is -- it is enforced as an equivalence by
    # `check_blocker_flag_agrees_with_blocking_issues`.
    blocking_issues = decision.get("blocking_issues")
    if decision.get("policy_allow_merge") is False:
        stated.append(("policy_allow_merge=False", 3))
    elif isinstance(blocking_issues, list) and blocking_issues:
        stated.append((f"{len(blocking_issues)} blocking_issues", 3))

    # `stated` includes the verdict's own rank, so this is the milder-direction
    # test and nothing else: the maximum can only exceed the verdict when some
    # OTHER axis rules the verdict out.
    final_rank = max(rank for _, rank in stated)
    if verdict_rank >= final_rank:
        return []

    axes = ", ".join(name for name, _ in stated)
    return [
        f"decision.verdict is milder than the axes beside it: {axes}. The most "
        "conservative axis a pack states is the verdict it may publish (rank "
        f"{final_rank}), and every reader reconciles this decision to "
        f"{VERDICT_FROM_RANK[final_rank]!r}"
    ]


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


def inline_class_counts_coherent(
    status: str,
    effective_class: str,
    findings: int,
    introduced: int,
    preexisting: int,
) -> bool:
    """Whether the aggregate tuple is possible, without deriving its class."""
    if introduced + preexisting > findings:
        return False
    if status in {"passed", "notrun"}:
        return findings == 0 and effective_class == "PASS"
    if status == "warnings" and findings > 0:
        if effective_class == "INFO":
            return True
        if effective_class == "PASS":
            return introduced == 0 and findings == preexisting
        return False
    if status == "failed" and findings > 0:
        if effective_class == "FAIL":
            return True
        if effective_class == "INFO":
            return findings >= 2 and preexisting >= 1
        if effective_class == "PASS":
            return introduced == 0 and findings == preexisting
    return False


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
    elif data["schema_version"] not in ("1.0", "2.0", "2.1", "2.2", "2.3"):
        issues.append(
            "schema_version must be '1.0', '2.0', '2.1', '2.2', or '2.3'"
        )
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

    policy_mode = policy.get("mode") if isinstance(policy, dict) else None
    raw_decision = data.get("decision")
    raw_quality_details = (
        raw_decision.get("quality_failure_details")
        if isinstance(raw_decision, dict)
        else None
    )
    failure_details_by_name: dict[str, list[str]] = {}
    preexisting_warning_details: dict[str, int] = {}
    warning_details_by_name: dict[str, int] = {}
    for detail in raw_quality_details if isinstance(raw_quality_details, list) else []:
        if (
            isinstance(detail, dict)
            and isinstance(detail.get("name"), str)
            and isinstance(detail.get("classification"), str)
            and detail.get("origin") == "failure"
        ):
            failure_details_by_name.setdefault(detail["name"], []).append(
                detail["classification"]
            )
        elif (
            isinstance(detail, dict)
            and isinstance(detail.get("name"), str)
            and detail.get("classification") in VALID_QUALITY_FAILURE_CLASSES
            and detail.get("origin") == "warning"
        ):
            warning_details_by_name[detail["name"]] = (
                warning_details_by_name.get(detail["name"], 0) + 1
            )
            if detail.get("classification") == "pre-existing":
                preexisting_warning_details[detail["name"]] = (
                    preexisting_warning_details.get(detail["name"], 0) + 1
                )
    failed_check_counts: dict[str, int] = {}
    warning_check_counts: dict[str, int] = {}
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
            if check.get("status") not in VALID_CHECK_STATUSES:
                issues.append(
                    f"{ctx}.status must be one of {sorted(VALID_CHECK_STATUSES)}"
                )
            if check.get("class") not in VALID_CLASSES:
                issues.append(f"{ctx}.class must be one of {sorted(VALID_CLASSES)}")
            if check.get("severity") not in VALID_SEVERITIES:
                issues.append(f"{ctx}.severity must be one of {sorted(VALID_SEVERITIES)}")
            require_boolean(check.get("blocking"), f"{ctx}.blocking", issues)
            require_non_negative_number(check.get("duration_secs"), f"{ctx}.duration_secs", issues)
            require_non_empty_string(check.get("evidence"), f"{ctx}.evidence", issues)
            if schema_at_least(data.get("schema_version"), (2, 3)):
                issues.extend(
                    ensure_keys(
                        check,
                        [
                            "execution_state",
                            "outcome",
                            "policy_conclusion",
                            "confidence_impact",
                            "merge_impact",
                        ],
                        ctx,
                    )
                )
                for field, vocabulary in [
                    ("execution_state", VALID_EXECUTION_STATES),
                    ("outcome", VALID_TOOL_OUTCOMES),
                    ("policy_conclusion", VALID_POLICY_CONCLUSIONS),
                    ("confidence_impact", VALID_CONFIDENCE_IMPACTS),
                    ("merge_impact", VALID_MERGE_IMPACTS),
                ]:
                    if check.get(field) not in vocabulary:
                        issues.append(
                            f"{ctx}.{field} must be one of {sorted(vocabulary)} (schema 2.3)"
                        )
                status_outcomes = {
                    "passed": {"passed"},
                    "failed": {"findings_failed"},
                    "warnings": {"findings_warning"},
                    "error": {"system_error"},
                    "skipped": {"skipped", "unavailable", "unknown"},
                }
                execution_outcomes = {
                    "executed": {
                        "passed",
                        "findings_failed",
                        "findings_warning",
                        "system_error",
                    },
                    "skipped": {"skipped"},
                    "unavailable": {"unavailable"},
                    "unknown": {"unknown"},
                }
                if check.get("outcome") not in status_outcomes.get(
                    check.get("status"), set()
                ):
                    issues.append(
                        f"{ctx}.status and outcome are not an emitted pair"
                    )
                if check.get("outcome") not in execution_outcomes.get(
                    check.get("execution_state"), set()
                ):
                    issues.append(
                        f"{ctx}.execution_state and outcome are not an emitted pair"
                    )
                if check.get("outcome") == "system_error" and (
                    check.get("confidence_impact") != "incomplete"
                    or check.get("policy_conclusion") == "satisfied"
                ):
                    issues.append(
                        f"{ctx}.system_error requires incomplete confidence and a non-satisfied conclusion"
                    )
                if check.get("outcome") in {
                    "findings_failed",
                    "findings_warning",
                    "system_error",
                } and check.get("policy_conclusion") == "satisfied":
                    issues.append(
                        f"{ctx}.finding/error outcome cannot have policy_conclusion=satisfied"
                    )
                expected_class = {
                    "passed": "PASS",
                    "failed": "FAIL",
                    "warnings": "INFO",
                    "skipped": "SKIP",
                    "error": "FAIL",
                }.get(check.get("status"))
                if expected_class is not None and check.get("class") != expected_class:
                    issues.append(
                        f"{ctx}.class must agree with status ({expected_class})"
                    )
                typed_preexisting = (
                    check.get("status") in {"failed", "error"}
                    and failure_details_by_name.get(check.get("name"))
                    == ["pre-existing"]
                ) or (
                    check.get("status") == "warnings"
                    and preexisting_warning_details.get(check.get("name")) == 1
                )
                preexisting_downgrade = (
                    typed_preexisting
                    and check.get("policy_conclusion") == "advisory"
                    and check.get("merge_impact") == "approve"
                    and check.get("blocking") is False
                )
                if check.get("status") in {"failed", "error"}:
                    check_name = check.get("name")
                    if isinstance(check_name, str):
                        failed_check_counts[check_name] = (
                            failed_check_counts.get(check_name, 0) + 1
                        )
                    matched_details = failure_details_by_name.get(check_name, [])
                    if len(matched_details) != 1:
                        issues.append(
                            f"{ctx} requires exactly one same-name origin=failure quality_failure_details entry"
                        )
                    if check.get("merge_impact") == "approve" and not preexisting_downgrade:
                        issues.append(
                            f"{ctx} may approve a failed/error result only with typed pre-existing failure provenance"
                        )
                elif check.get("status") == "warnings" and isinstance(
                    check.get("name"), str
                ):
                    check_name = check["name"]
                    warning_check_counts[check_name] = (
                        warning_check_counts.get(check_name, 0) + 1
                    )
                conclusion_merge_coherent = (
                    check.get("policy_conclusion") == "blocked"
                    and check.get("merge_impact") == "block"
                ) or (
                    check.get("policy_conclusion") == "advisory"
                    and check.get("merge_impact") == "review_required"
                ) or (
                    check.get("policy_conclusion") == "satisfied"
                    and check.get("merge_impact") in {"approve", "review_required"}
                ) or preexisting_downgrade
                if not conclusion_merge_coherent:
                    issues.append(
                        f"{ctx}.policy_conclusion and merge_impact are not an emitted pair"
                    )
                if preexisting_downgrade:
                    expected_check_blocking = False
                elif check.get("status") != "skipped":
                    expected_check_blocking = {
                        "shadow": False,
                        "warn": check.get("class") == "FAIL"
                        and check.get("severity") == "block",
                        "block": check.get("class") == "FAIL"
                        and check.get("severity") in {"block", "warn"},
                    }.get(policy_mode)
                else:
                    expected_check_blocking = None
                if (
                    expected_check_blocking is not None
                    and check.get("blocking") is not expected_check_blocking
                ):
                    issues.append(
                        f"{ctx}.blocking contradicts policy.mode, severity, class, and typed provenance"
                    )
                if check.get("status") == "skipped" and check.get(
                    "execution_state"
                ) in {"unavailable", "unknown"}:
                    expected_skip_tuple = {
                        "warn": ("advisory", "degraded", "review_required"),
                        "ignore": ("satisfied", "complete", "approve"),
                        "block": ("blocked", "incomplete", "block"),
                    }.get(check.get("severity"))
                    actual_skip_tuple = (
                        check.get("policy_conclusion"),
                        check.get("confidence_impact"),
                        check.get("merge_impact"),
                    )
                    if expected_skip_tuple is not None and (
                        actual_skip_tuple != expected_skip_tuple
                    ):
                        issues.append(
                            f"{ctx}.unavailable/unknown skip policy tuple contradicts severity"
                        )
                block_tuple = (
                    check.get("blocking") is True,
                    check.get("policy_conclusion") == "blocked",
                    check.get("merge_impact") == "block",
                )
                if block_tuple not in {(False, False, False), (True, True, True)}:
                    issues.append(
                        f"{ctx} requires policy_conclusion=blocked iff merge_impact=block iff blocking=true"
                    )

        if schema_at_least(data.get("schema_version"), (2, 3)):
            for name, classifications in failure_details_by_name.items():
                if failed_check_counts.get(name, 0) != len(classifications):
                    issues.append(
                        "decision.quality_failure_details origin=failure rows must map one-to-one "
                        f"to failed/error checks for {name!r}"
                    )
            for name, detail_count in warning_details_by_name.items():
                if warning_check_counts.get(name, 0) != detail_count:
                    issues.append(
                        "decision.quality_failure_details origin=warning rows "
                        "must map one-to-one to warning checks "
                        f"for {name!r}"
                    )

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
        if schema_at_least(data.get("schema_version"), (2, 3)):
            issues.extend(
                ensure_keys(
                    inline,
                    [
                        "effective_class",
                        "enforcement_disposition",
                        "introduced_count",
                        "preexisting_count",
                    ],
                    "inline_findings",
                )
            )
            effective_class = inline.get("effective_class")
            inline_disposition = inline.get("enforcement_disposition")
            introduced_count = inline.get("introduced_count")
            preexisting_count = inline.get("preexisting_count")
            if effective_class not in {"PASS", "INFO", "FAIL"}:
                issues.append(
                    "inline_findings.effective_class must be one of PASS|INFO|FAIL (schema 2.3)"
                )
            if inline_disposition not in VALID_ENFORCEMENT_DISPOSITIONS:
                issues.append(
                    "inline_findings.enforcement_disposition must be one of "
                    f"{sorted(VALID_ENFORCEMENT_DISPOSITIONS)} (schema 2.3)"
                )
            require_non_negative_integer(
                introduced_count, "inline_findings.introduced_count", issues
            )
            require_non_negative_integer(
                preexisting_count, "inline_findings.preexisting_count", issues
            )
            normalized_status = normalize_inline_status(inline.get("status"))
            expected_inline_blocking = {
                "shadow": False,
                "warn": effective_class == "FAIL" and inline.get("severity") == "block",
                "block": effective_class == "FAIL"
                and inline.get("severity") in {"block", "warn"},
            }.get(policy_mode)
            if (
                expected_inline_blocking is not None
                and inline.get("blocking") is not expected_inline_blocking
            ):
                issues.append(
                    "inline_findings.blocking contradicts policy.mode, severity, and effective_class"
                )
            expected_inline_disposition = None
            if expected_inline_blocking is True:
                expected_inline_disposition = "block"
            elif effective_class == "FAIL":
                expected_inline_disposition = "review_required"
            elif effective_class == "INFO" or normalized_status == "warnings":
                expected_inline_disposition = "warnings_only"
            elif effective_class == "PASS":
                expected_inline_disposition = "clean"
            if (
                expected_inline_disposition is not None
                and inline_disposition != expected_inline_disposition
            ):
                issues.append(
                    "inline_findings effective_class/status/blocking contradict "
                    "inline_findings.enforcement_disposition"
                )
            if (
                isinstance(findings_count, int)
                and not isinstance(findings_count, bool)
                and isinstance(introduced_count, int)
                and not isinstance(introduced_count, bool)
                and isinstance(preexisting_count, int)
                and not isinstance(preexisting_count, bool)
            ):
                if introduced_count + preexisting_count > findings_count:
                    issues.append(
                        "inline_findings introduced_count + preexisting_count must not exceed findings_count"
                    )
                if not inline_class_counts_coherent(
                    normalized_status,
                    effective_class,
                    findings_count,
                    introduced_count,
                    preexisting_count,
                ):
                    issues.append(
                        "inline_findings status/counts cannot accompany effective_class"
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
        # From schema 2.2 the whole entry is part of the contract, not an extra.
        # `origin` is the only thing that explains an entry in
        # `introduced_quality_failures` sitting next to `quality_pass: true`;
        # `name` is what a consumer reports; `classification` is what decides
        # whether the entry gated this diff. Validating only one of the three let
        # `{"origin": "failure"}` -- an anonymous, unclassified failure -- pass
        # its own contract gate.
        if schema_at_least(data.get("schema_version"), (2, 2)):
            # `quality_pass` is a documented decision axis and the 2.2 writer
            # emits it unconditionally, as a boolean, from a single `json!`
            # literal -- so a 2.2 pack that omits it or states it as a string is
            # not an old pack, it is a broken one. Absence stays forgiven BELOW
            # 2.2, where readers derive the flag from the reconciled verdict; a
            # 2.2 pack gets no such benefit of the doubt. Type-checking here also
            # puts the validator back in step with the readers, which normalize a
            # present-but-unreadable signal to BLOCK: without this the contract
            # gate certified an artifact the CLI and MCP both refuse to trust.
            issues.extend(ensure_keys(decision, ["quality_pass"], "decision"))
            if "quality_pass" in decision:
                require_boolean(decision.get("quality_pass"), "decision.quality_pass", issues)
            # The remaining decision axes, on the same argument. All three come
            # out of the same 2.2 `json!` literal, unconditionally and from the
            # typed enums, so a 2.2 pack missing one is broken rather than old --
            # and the reconciliation below can only reject a verdict its axes
            # contradict if the axes are actually there to read. Absence stays
            # forgiven BELOW 2.2, where a reader derives what the pack omits.
            issues.extend(
                ensure_keys(
                    decision,
                    ["analysis_status", "merge_recommendation", "policy_allow_merge"],
                    "decision",
                )
            )
            if "analysis_status" in decision:
                if decision.get("analysis_status") not in VALID_ANALYSIS_STATUSES:
                    issues.append(
                        "decision.analysis_status must be one of "
                        f"{sorted(VALID_ANALYSIS_STATUSES)} (schema 2.2)"
                    )
            if "merge_recommendation" in decision:
                if decision.get("merge_recommendation") not in VALID_MERGE_RECOMMENDATIONS:
                    issues.append(
                        "decision.merge_recommendation must be one of "
                        f"{sorted(VALID_MERGE_RECOMMENDATIONS)} (schema 2.2)"
                    )
            if "policy_allow_merge" in decision:
                require_boolean(
                    decision.get("policy_allow_merge"), "decision.policy_allow_merge", issues
                )
            issues.extend(check_blocker_flag_agrees_with_blocking_issues(decision))
            issues.extend(check_decision_axes_agree_on_the_verdict(decision))
            details = decision.get("quality_failure_details")
            if not isinstance(details, list):
                issues.append("decision.quality_failure_details must be an array")
            else:
                for idx, detail in enumerate(details):
                    ctx = f"decision.quality_failure_details[{idx}]"
                    if not isinstance(detail, dict):
                        issues.append(f"{ctx} must be an object")
                        continue
                    require_non_empty_string(detail.get("name"), f"{ctx}.name", issues)
                    classification = detail.get("classification")
                    if classification not in VALID_QUALITY_FAILURE_CLASSES:
                        issues.append(
                            f"{ctx}.classification must be one of "
                            f"{sorted(VALID_QUALITY_FAILURE_CLASSES)} (schema 2.2)"
                        )
                    origin = detail.get("origin")
                    if origin not in VALID_QUALITY_FAILURE_ORIGINS:
                        issues.append(
                            f"{ctx}.origin must be one of "
                            f"{sorted(VALID_QUALITY_FAILURE_ORIGINS)} (schema 2.2)"
                        )
                issues.extend(check_quality_pass_agrees_with_details(decision, details))

        if schema_at_least(data.get("schema_version"), (2, 3)):
            typed_checks = data.get("checks") if isinstance(data.get("checks"), list) else []
            typed_inline = (
                data.get("inline_findings")
                if isinstance(data.get("inline_findings"), dict)
                else {}
            )
            explicit_warning = any(
                isinstance(check, dict)
                and check.get("status") == "warnings"
                and check.get("outcome") == "findings_warning"
                for check in typed_checks
            ) or typed_inline.get("enforcement_disposition") == "warnings_only"
            typed_confidence_review_source = any(
                isinstance(check, dict)
                and check.get("confidence_impact") in {"degraded", "incomplete"}
                for check in typed_checks
            )
            typed_merge_review_source = any(
                isinstance(check, dict)
                and check.get("outcome") != "findings_warning"
                and check.get("merge_impact") == "review_required"
                for check in typed_checks
            ) or typed_inline.get("enforcement_disposition") == "review_required"
            typed_review_source = (
                typed_confidence_review_source or typed_merge_review_source
            )
            typed_blocking_source = any(
                isinstance(check, dict)
                and (
                    check.get("blocking") is True
                    or check.get("policy_conclusion") == "blocked"
                    or check.get("merge_impact") == "block"
                )
                for check in typed_checks
            ) or (
                typed_inline.get("blocking") is True
                or typed_inline.get("enforcement_disposition") == "block"
            )
            issues.extend(
                ensure_keys(decision, ["enforcement_disposition"], "decision")
            )
            disposition = decision.get("enforcement_disposition")
            if disposition not in VALID_ENFORCEMENT_DISPOSITIONS:
                issues.append(
                    "decision.enforcement_disposition must be one of "
                    f"{sorted(VALID_ENFORCEMENT_DISPOSITIONS)} (schema 2.3)"
                )
            elif verdict in VALID_VERDICTS:
                expected_verdicts = {
                    "clean": {"PASS"},
                    "warnings_only": {"PASS", "CONDITIONAL"},
                    "review_required": {"CONDITIONAL"},
                    "block": {"BLOCK"},
                }
                if verdict not in expected_verdicts[disposition]:
                    issues.append(
                        "decision.enforcement_disposition contradicts decision.verdict: "
                        f"{disposition!r} cannot accompany {verdict!r}"
                    )
            if disposition == "warnings_only":
                if not explicit_warning:
                    issues.append(
                        "decision.enforcement_disposition warnings_only requires a typed warning fact"
                    )
                if decision.get("quality_pass") is not True:
                    issues.append(
                        "decision.enforcement_disposition warnings_only requires quality_pass=true"
                    )
                if decision.get("analysis_status") != "complete":
                    issues.append(
                        "decision.enforcement_disposition warnings_only requires analysis_status=complete"
                    )
                if decision.get("blocking_issues"):
                    issues.append(
                        "decision.enforcement_disposition warnings_only requires no blocking_issues"
                    )
                if typed_blocking_source:
                    issues.append(
                        "decision.enforcement_disposition warnings_only cannot accompany a typed blocking source"
                    )
            elif disposition == "clean":
                if explicit_warning:
                    issues.append(
                        "decision.enforcement_disposition clean cannot hide a typed warning fact"
                    )
            if typed_review_source and (
                verdict == "PASS"
                or decision.get("allow_merge") is not False
                or disposition not in {"review_required", "block"}
            ):
                issues.append(
                    "a typed degraded/incomplete or non-warning review check requires a "
                    "non-PASS decision and enforcement_disposition review_required or block"
                )
            if typed_merge_review_source and decision.get(
                "merge_recommendation"
            ) not in {"review_required", "block"}:
                issues.append(
                    "a typed check or inline merge review source requires "
                    "merge_recommendation review_required or block"
                )
            if typed_blocking_source and (
                verdict != "BLOCK"
                or decision.get("merge_recommendation") != "block"
                or decision.get("allow_merge") is not False
                or decision.get("policy_allow_merge") is not False
                or disposition != "block"
            ):
                issues.append(
                    "a typed blocking check or inline finding requires verdict=BLOCK, "
                    "merge_recommendation=block, allow_merge=false, "
                    "policy_allow_merge=false, and enforcement_disposition=block"
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
