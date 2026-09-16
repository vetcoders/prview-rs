from __future__ import annotations

import importlib.util
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "bounded_runtime_acceptance.py"
SPEC = importlib.util.spec_from_file_location("bounded_runtime_acceptance", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SuccessfulLiveCheckTests(unittest.TestCase):
    def test_accepts_only_exact_live_passed_row(self) -> None:
        run = {
            "checks": [
                {"name": "Cargo check", "status": "passed", "cached": False}
            ]
        }

        self.assertTrue(MODULE.has_successful_live_check(run, "Cargo check"))
        self.assertFalse(MODULE.has_successful_live_check(run, "Cargo"))
        self.assertFalse(MODULE.has_successful_live_check(run, "cargo check"))

    def test_rejects_failed_skipped_cached_and_malformed_rows(self) -> None:
        for row in [
            {"name": "Vitest", "status": "failed", "cached": False},
            {"name": "Vitest", "status": "skipped", "cached": False},
            {"name": "Vitest", "status": "passed", "cached": True},
            {"name": "Vitest", "status": "PASSED", "cached": False},
            {"name": "Vitest", "status": "passed"},
            "Vitest",
        ]:
            with self.subTest(row=row):
                self.assertFalse(
                    MODULE.has_successful_live_check({"checks": [row]}, "Vitest")
                )


class CaseCatalogueTests(unittest.TestCase):
    def test_every_case_has_a_mutation_and_an_assertion(self) -> None:
        self.assertEqual(
            sorted(MODULE.CASES), ["js-only", "mixed", "unknown-input"]
        )
        for name, case in MODULE.CASES.items():
            with self.subTest(case=name):
                self.assertTrue(callable(case["mutate"]))
                self.assertTrue(callable(case["assert_scope"]))


class CommandCensusTests(unittest.TestCase):
    """`cargo test` must be recognised wherever cargo is installed from."""

    def test_recognises_a_cargo_test_invocation(self) -> None:
        census = {
            "observed_commands": {
                "/Users/x/.cargo/bin/cargo test --all-targets --no-fail-fast",
                "cargo test --all-targets -p only",
            }
        }

        self.assertEqual(len(MODULE.observed_cargo_test_commands(census)), 2)

    def test_ignores_other_cargo_subcommands_and_test_binaries(self) -> None:
        census = {
            "observed_commands": {
                "cargo clippy --all-targets",
                "cargo check",
                "/tmp/target/debug/deps/fixture-1a2b3c test",
                "rustc --test src/lib.rs",
                "",
            }
        }

        self.assertEqual(MODULE.observed_cargo_test_commands(census), [])


class RowHelperTests(unittest.TestCase):
    def test_reads_the_named_row_and_its_scope(self) -> None:
        run = {
            "checks": [
                {"name": "Vitest", "status": "passed"},
                {
                    "name": "Cargo test",
                    "status": "skipped",
                    "scope": {"mode": "change-scoped", "selected": 0},
                },
            ]
        }

        self.assertEqual(MODULE.check_row(run, "Cargo test")["status"], "skipped")
        self.assertEqual(MODULE.scope_of(run, "Cargo test")["selected"], 0)
        # A row with no scope, an absent row and a malformed document all read
        # as "nothing claimed", never as a passing assertion.
        self.assertEqual(MODULE.scope_of(run, "Vitest"), {})
        self.assertEqual(MODULE.check_row(run, "Stylelint"), {})
        self.assertEqual(MODULE.scope_of(None, "Cargo test"), {})


class EmptySelectionAssertionTests(unittest.TestCase):
    """The js-only case is the one that can go falsely green; fence it in."""

    def passing_inputs(self) -> tuple[dict, dict, dict]:
        run = {
            "checks": [
                {"name": "Vitest", "status": "passed", "cached": False,
                 "scope": {"mode": "change-scoped", "selected": 1}},
                {"name": "Cargo test", "status": "skipped", "cached": False,
                 "scope": {"mode": "change-scoped", "selected": 0, "selector": None}},
            ]
        }
        gate = {
            "checks": [
                {
                    "name": "Cargo test",
                    "outcome": "skipped",
                    "blocking": False,
                    "reason": "no tests related to the change",
                }
            ],
            "decision": {
                "verdict": "PASS",
                "review_caveats": [
                    "Cargo test skipped: no tests related to the change"
                ],
            },
        }
        return run, gate, {"observed_commands": set()}

    def test_accepts_a_genuinely_empty_selection(self) -> None:
        run, gate, census = self.passing_inputs()
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, pathlib.Path("/nonexistent"), census)

        self.assertEqual(violations, [])

    def test_rejects_a_suite_relabelled_as_passed(self) -> None:
        run, gate, census = self.passing_inputs()
        run["checks"][1]["status"] = "passed"
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, pathlib.Path("/nonexistent"), census)

        self.assertTrue(any("not skipped" in item for item in violations))

    def test_rejects_a_skip_that_still_spawned_the_suite(self) -> None:
        run, gate, census = self.passing_inputs()
        census["observed_commands"] = {"cargo test --all-targets --no-fail-fast"}
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, pathlib.Path("/nonexistent"), census)

        self.assertTrue(any("cargo test process ran" in item for item in violations))

    def test_rejects_a_blocking_gate_row(self) -> None:
        run, gate, census = self.passing_inputs()
        gate["checks"][0]["blocking"] = True
        gate["decision"]["verdict"] = "BLOCK"
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, pathlib.Path("/nonexistent"), census)

        self.assertEqual(len(violations), 2)


if __name__ == "__main__":
    unittest.main()
