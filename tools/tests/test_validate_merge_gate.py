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


if __name__ == "__main__":
    unittest.main()
