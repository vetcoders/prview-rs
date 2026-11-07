"""Schema-3.0 provenance contract tests for `tools/validate_merge_gate.py`.

Every fixture here is a REAL `MERGE_GATE.json` emitted by prview (schema 3.0),
mutated in exactly one way. `valid_base.json` is the unmutated control: if a
change to the validator makes it fail, the validator rejects artifacts the tool
actually writes.

The four negatives are the shapes an earlier validator certified as clean while
the contract in `docs/contracts/merge_gate.md` forbids them: a 3.0 gate with no
`provenance_contradictions` field at all, a contradiction with no review signal,
signals that count correctly but name the wrong rows, and a row attributed to a
check the gate never emitted.
"""

from __future__ import annotations

import copy
import json
import tempfile
import importlib.util
import pathlib
import unittest


TOOLS = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = TOOLS / "validate_merge_gate.py"
FIXTURES = TOOLS / "fixtures" / "merge-gate"
SPEC = importlib.util.spec_from_file_location("validate_merge_gate", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def validate(name: str) -> list[str]:
    return MODULE.validate(FIXTURES / f"{name}.json")


def validate_with_scope(scope: object, check_name: str = "Cargo test") -> list[str]:
    """Validate the real control gate with one `scope` object grafted on."""
    import json
    import tempfile

    gate = json.loads((FIXTURES / "valid_base.json").read_text(encoding="utf-8"))
    row = next(check for check in gate["checks"] if check["name"] == check_name)
    row["scope"] = scope
    with tempfile.TemporaryDirectory() as temp:
        path = pathlib.Path(temp) / "MERGE_GATE.json"
        path.write_text(json.dumps(gate), encoding="utf-8")
        return MODULE.validate(path)


class ProvenanceContradictionContractTests(unittest.TestCase):
    def test_real_gate_without_contradictions_validates(self) -> None:
        # The control. A validator that cannot pass this rejects live packs.
        self.assertEqual(validate("valid_base"), [])

    def test_missing_root_array_is_an_issue(self) -> None:
        # 3.0 requires the array even when empty: omitting it is how a
        # contradiction disappears without anything noticing.
        issues = validate("missing_root")
        self.assertTrue(
            any("provenance_contradictions" in issue for issue in issues),
            issues,
        )

    def test_contradiction_without_any_review_signal_is_an_issue(self) -> None:
        # `decision.review_caveats` absent while a row stands: the gate names
        # the disagreement in one field and hides it from the one a reviewer
        # reads.
        issues = validate("missing_signals")
        self.assertTrue(
            any("review_caveats" in issue for issue in issues),
            issues,
        )

    def test_duplicated_signal_does_not_cover_a_second_row(self) -> None:
        # Two rows, two signals, both announcing the FIRST row. Counting alone
        # called this correspondence; the second contradiction is unannounced
        # and the extra signal is unsupported.
        issues = validate("duplicate_signal")
        self.assertTrue(
            any("missing" in issue for issue in issues),
            issues,
        )
        self.assertTrue(
            any("no provenance_contradictions row supports" in issue for issue in issues),
            issues,
        )

    def test_row_attributed_to_an_unemitted_check_is_an_issue(self) -> None:
        # `check_id` is the same value as `checks[].id`; a row pointing outside
        # that set is evidence no reader can follow back to anything.
        issues = validate("unknown_check")
        self.assertTrue(
            any("check_id" in issue for issue in issues),
            issues,
        )


class ScopeObjectContractTests(unittest.TestCase):
    """`scope` describes a command that ran. It may not contradict itself."""

    def test_an_honest_narrowed_scope_validates(self) -> None:
        self.assertEqual(
            validate_with_scope(
                {
                    "mode": "change-scoped",
                    "reason": "change-scoped selection",
                    "inputs": 3,
                    "selected": 2,
                    "universe": 4,
                    "selector": "-p app -p core",
                }
            ),
            [],
        )

    def test_an_honest_full_scope_validates(self) -> None:
        self.assertEqual(
            validate_with_scope(
                {
                    "mode": "full",
                    "reason": "unsupported input: src/limits.yaml",
                    "inputs": 3,
                    "selected": None,
                    "universe": None,
                    "selector": None,
                }
            ),
            [],
        )

    def test_a_full_run_may_not_publish_a_selector(self) -> None:
        # The one shape that makes the pack contradict itself: the mode says
        # the whole suite ran, the selector says a fragment of it did.
        issues = validate_with_scope(
            {
                "mode": "full",
                "reason": "unsupported input: src/limits.yaml",
                "selector": "-p core",
            }
        )
        self.assertTrue(
            any("selector must be null when mode is 'full'" in issue for issue in issues),
            issues,
        )

    def test_a_full_run_may_not_publish_a_selection_count(self) -> None:
        issues = validate_with_scope(
            {"mode": "full", "reason": "why", "selected": 2}
        )
        self.assertTrue(
            any("selected must be null when mode is 'full'" in issue for issue in issues),
            issues,
        )

    def test_an_empty_selection_is_a_valid_narrowing(self) -> None:
        # Nothing to run is a real answer, and the row that carries it is a
        # `skipped` one. It must not need a selector to be valid.
        self.assertEqual(
            validate_with_scope(
                {
                    "mode": "change-scoped",
                    "reason": "change-scoped selection",
                    "inputs": 1,
                    "selected": 0,
                    "universe": 2,
                    "selector": None,
                }
            ),
            [],
        )


class EvidenceScopeContractTests(unittest.TestCase):
    """3.2 evidence cannot disappear or contradict its stated measurement kind."""

    def setUp(self) -> None:
        self.gate = json.loads((FIXTURES / "valid_base.json").read_text())
        self.gate.update({
            "schema_version": "3.2",
            "security_scope": {
                "requested": "extended", "source": "recorded_check_results",
                "checks": [{"id": "semgrep_scan", "name": "Semgrep scan",
                            "execution": "executed", "status": "passed", "reason": None}],
            },
            "coverage_evidence": {
                "kind": "source_test_change_match", "execution_coverage_measured": False,
                "measured_field_meaning": "heuristic_evaluated", "weak_match_count": 0,
            },
            "loctree_evidence": {
                "scope": "whole_repository", "comparison_kind": "not_available",
                "finding_identities_compared": False, "net_counts_unchanged": None,
                "count_deltas": None, "base_sha": None, "target_sha": None,
            },
        })

    def issues(self, gate: dict) -> list[str]:
        with tempfile.TemporaryDirectory() as temp:
            path = pathlib.Path(temp) / "MERGE_GATE.json"
            path.write_text(json.dumps(gate))
            return MODULE.validate(path)

    def test_valid_evidence_and_recorded_security_states(self) -> None:
        self.assertEqual(self.issues(self.gate), [])
        for execution, status in [("executed", "passed"), ("cached", "passed"),
                                  ("error", "error"), ("skipped_at_runtime", "skipped"),
                                  ("not_executed", "skipped")]:
            with self.subTest(execution=execution):
                row = self.gate["security_scope"]["checks"][0]
                row.update(execution=execution, status=status, reason="observed reason")
                self.assertEqual(self.issues(self.gate), [])
        self.gate["security_scope"].update(requested="disabled", checks=[])
        self.assertEqual(self.issues(self.gate), [])

    def test_all_three_objects_are_required_only_from_3_2(self) -> None:
        for name in ("security_scope", "coverage_evidence", "loctree_evidence"):
            for bad in ("missing", None, [], "scope", True):
                with self.subTest(name=name, bad=bad):
                    gate = copy.deepcopy(self.gate)
                    if bad == "missing":
                        del gate[name]
                    else:
                        gate[name] = bad
                    self.assertTrue(any(name in issue for issue in self.issues(gate)))
        for version in ("3.0", "3.1"):
            gate = copy.deepcopy(self.gate)
            gate["schema_version"] = version
            for name in ("security_scope", "coverage_evidence", "loctree_evidence"):
                del gate[name]
            self.assertEqual(self.issues(gate), [])

    def test_every_evidence_member_is_required_including_nullable_members(self) -> None:
        for name in ("security_scope", "coverage_evidence", "loctree_evidence"):
            for field in self.gate[name]:
                with self.subTest(name=name, field=field):
                    gate = copy.deepcopy(self.gate)
                    del gate[name][field]
                    self.assertTrue(any(f"{name}" in issue and field in issue
                                        for issue in self.issues(gate)))

    def test_security_metadata_and_rows_reject_malformed_values(self) -> None:
        for field, bad in [("requested", "complete"), ("requested", []),
                           ("source", "assumed"), ("checks", {}), ("checks", None),
                           ("checks", [None])]:
            with self.subTest(field=field, bad=bad):
                gate = copy.deepcopy(self.gate)
                gate["security_scope"][field] = bad
                self.assertTrue(self.issues(gate))
        for field, bad in [("id", ""), ("name", ""), ("execution", "assumed"),
                           ("execution", {}), ("status", "PASS"), ("status", []),
                           ("reason", 1)]:
            gate = copy.deepcopy(self.gate)
            gate["security_scope"]["checks"][0][field] = bad
            self.assertTrue(self.issues(gate), (field, bad))
        for field in self.gate["security_scope"]["checks"][0]:
            gate = copy.deepcopy(self.gate)
            del gate["security_scope"]["checks"][0][field]
            self.assertTrue(self.issues(gate), field)

    def test_security_execution_status_pairs_follow_recorded_results(self) -> None:
        for execution, statuses in [
            ("not_executed", ["passed", "failed", "warnings", "error"]),
            ("skipped_at_runtime", ["passed", "failed", "warnings", "error"]),
            ("error", ["passed", "failed", "warnings", "skipped"]),
            ("executed", ["skipped", "error"]),
        ]:
            for status in statuses:
                with self.subTest(execution=execution, status=status):
                    gate = copy.deepcopy(self.gate)
                    gate["security_scope"]["checks"][0].update(execution=execution, status=status)
                    self.assertTrue(self.issues(gate))
        # The producer records cache provenance before inspecting status. Do
        # not silently redefine a cached result as necessarily successful.
        for status in ["passed", "failed", "warnings", "skipped", "error"]:
            gate = copy.deepcopy(self.gate)
            gate["security_scope"]["checks"][0].update(execution="cached", status=status)
            self.assertEqual(self.issues(gate), [])

    def test_coverage_kind_and_integer_weak_count_are_explicit(self) -> None:
        for field, bad in [("kind", "line_coverage"), ("kind", {}),
                           ("execution_coverage_measured", True),
                           ("execution_coverage_measured", 0),
                           ("measured_field_meaning", "execution"),
                           *(('weak_match_count', value) for value in [True, -1, 0.5, "0", None])]:
            with self.subTest(field=field, bad=bad):
                gate = copy.deepcopy(self.gate)
                gate["coverage_evidence"][field] = bad
                self.assertTrue(self.issues(gate))

    def test_loctree_zero_and_nonzero_deltas_remain_count_evidence(self) -> None:
        evidence = self.gate["loctree_evidence"]
        evidence.update(comparison_kind="net_count_delta", base_sha="base", target_sha="target",
                        count_deltas={"dead_exports": 0, "cycles": 0, "unused_symbols": 0},
                        net_counts_unchanged=True)
        self.assertEqual(self.issues(self.gate), [])
        evidence["count_deltas"]["dead_exports"] = -1
        evidence["net_counts_unchanged"] = False
        self.assertEqual(self.issues(self.gate), [])
        evidence["net_counts_unchanged"] = True
        self.assertTrue(self.issues(self.gate))
        evidence["net_counts_unchanged"] = False
        for field in ("dead_exports", "cycles", "unused_symbols"):
            for bad in [None, True, 1.5, "1"]:
                gate = copy.deepcopy(self.gate)
                gate["loctree_evidence"]["count_deltas"][field] = bad
                self.assertTrue(self.issues(gate), (field, bad))
        for field in ("base_sha", "target_sha"):
            gate = copy.deepcopy(self.gate)
            gate["loctree_evidence"][field] = None
            self.assertTrue(self.issues(gate))
        del evidence["count_deltas"]["cycles"]
        self.assertTrue(self.issues(self.gate))

    def test_unavailable_loctree_may_not_claim_a_measured_delta(self) -> None:
        for field, bad in [("scope", "diff"), ("comparison_kind", "identical_findings"),
                           ("comparison_kind", []), ("finding_identities_compared", True),
                           ("finding_identities_compared", 0), ("net_counts_unchanged", True),
                           ("count_deltas", {}), ("base_sha", "base"), ("target_sha", 42)]:
            gate = copy.deepcopy(self.gate)
            gate["loctree_evidence"][field] = bad
            self.assertTrue(self.issues(gate), (field, bad))


if __name__ == "__main__":
    unittest.main()
