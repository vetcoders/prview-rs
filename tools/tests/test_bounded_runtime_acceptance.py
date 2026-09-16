from __future__ import annotations

import importlib.util
import json
import pathlib
import tempfile
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


class RequiredRunChecksTests(unittest.TestCase):
    def test_clippy_and_rustfmt_are_required_live_checks(self) -> None:
        self.assertEqual(MODULE.REQUIRED_RUN_CHECKS["clippy"], "Clippy")
        self.assertEqual(MODULE.REQUIRED_RUN_CHECKS["rustfmt"], "Rustfmt")
        self.assertIn("clippy", MODULE.REQUIRED_LIVE_CHECKS_ONLY)
        self.assertIn("rustfmt", MODULE.REQUIRED_LIVE_CHECKS_ONLY)

        run = {
            "checks": [
                {"name": "Clippy", "status": "passed", "cached": False},
                {"name": "Rustfmt", "status": "passed", "cached": False},
            ]
        }
        self.assertTrue(MODULE.has_successful_live_check(run, "Clippy"))
        self.assertTrue(MODULE.has_successful_live_check(run, "Rustfmt"))

    def test_rejects_a_missing_toolchain_component(self) -> None:
        run = {
            "checks": [
                {"name": "Clippy", "status": "failed", "cached": False},
                {"name": "Rustfmt", "status": "skipped", "cached": False},
            ]
        }
        self.assertFalse(MODULE.has_successful_live_check(run, "Clippy"))
        self.assertFalse(MODULE.has_successful_live_check(run, "Rustfmt"))


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


class CargoShimTests(unittest.TestCase):
    """The shim is the complete record; the census is only a sample of one."""

    def test_recognises_a_test_run_by_its_subcommand(self) -> None:
        census = {
            "cargo_invocations": [
                "test --all-targets --no-fail-fast -p core",
                "+nightly test --lib",
            ]
        }

        self.assertEqual(len(MODULE.shim_cargo_test_invocations(census)), 2)

    def test_ignores_other_subcommands_and_flags_that_say_test(self) -> None:
        census = {
            "cargo_invocations": [
                "clippy --all-targets --tests",
                "check --tests",
                "metadata --no-deps --frozen",
                "",
            ]
        }

        self.assertEqual(MODULE.shim_cargo_test_invocations(census), [])

    def test_the_shim_execs_the_real_cargo_and_logs_every_invocation(self) -> None:
        import os
        import subprocess

        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        work = pathlib.Path(temp.name)
        real_dir = work / "bin"
        real_dir.mkdir()
        real = real_dir / "cargo"
        real.write_text("#!/bin/sh\nprintf 'real cargo: %s\\n' \"$*\"\n", encoding="utf-8")
        real.chmod(0o755)
        env = {"PATH": str(real_dir)}

        log = MODULE.install_cargo_shim(work, env)

        self.assertIsNotNone(log)
        completed = subprocess.run(
            ["cargo", "test", "-p", "core"],
            env={**os.environ, "PATH": env["PATH"]},
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertIn("real cargo: test -p core", completed.stdout)
        census = {"cargo_invocations": MODULE.cargo_shim_invocations(log)}
        self.assertEqual(census["cargo_invocations"], ["test -p core"])
        self.assertEqual(
            MODULE.shim_cargo_test_invocations(census), ["test -p core"]
        )

    def test_a_missing_log_reads_as_no_evidence(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        missing = pathlib.Path(temp.name) / "cargo-invocations.log"

        self.assertEqual(MODULE.cargo_shim_invocations(missing), [])
        self.assertEqual(MODULE.cargo_shim_invocations(None), [])


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
        census = {
            "observed_commands": set(),
            "truncated": False,
            # The shim saw cargo run — just never `cargo test`. An empty log
            # would mean the shim was not on PATH, which proves nothing.
            "cargo_invocations": ["check --all-targets", "clippy --all-targets"],
        }
        return run, gate, census

    def pack_with_vitest_command(self, command: str) -> pathlib.Path:
        """A minimal pack carrying the commands the gates really spawned."""
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        pack = pathlib.Path(temp.name)
        quality = pack / "20_quality"
        quality.mkdir(parents=True, exist_ok=True)
        (quality / "tests.result.json").write_text(
            json.dumps({"command": command}), encoding="utf-8"
        )
        (quality / "cargo_test.result.json").write_text(
            json.dumps({"command": MODULE.NO_COMMAND_RECORDED}), encoding="utf-8"
        )
        return pack

    def narrowed_vitest_pack(self) -> pathlib.Path:
        return self.pack_with_vitest_command(
            "pnpm exec vitest related --run --maxWorkers 1 --passWithNoTests src/math.js "
            "--reporter=default --reporter=json --outputFile.json=/tmp/x/vitest-scope.json"
        )

    def test_accepts_a_genuinely_empty_selection(self) -> None:
        run, gate, census = self.passing_inputs()
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, self.narrowed_vitest_pack(), census)

        self.assertEqual(violations, [])

    def test_rejects_a_suite_relabelled_as_passed(self) -> None:
        run, gate, census = self.passing_inputs()
        run["checks"][1]["status"] = "passed"
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, self.narrowed_vitest_pack(), census)

        self.assertTrue(any("not skipped" in item for item in violations))

    def test_rejects_a_skip_that_still_spawned_the_suite(self) -> None:
        run, gate, census = self.passing_inputs()
        census["observed_commands"] = {"cargo test --all-targets --no-fail-fast"}
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, self.narrowed_vitest_pack(), census)

        self.assertTrue(any("cargo test process ran" in item for item in violations))

    def test_rejects_a_narrowed_vitest_run_without_its_reporter(self) -> None:
        # The anti-spoof guarantee is the JSON reporter. A narrowed command
        # without it is back to trusting whatever the tool printed.
        run, gate, census = self.passing_inputs()
        pack = self.pack_with_vitest_command(
            "pnpm exec vitest related --run --maxWorkers 1 --passWithNoTests src/math.js"
        )
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, pack, census)

        self.assertTrue(any("no JSON reporter" in item for item in violations))

    def test_rejects_a_cargo_test_run_the_shim_recorded(self) -> None:
        # The shim is the complete record: every cargo invocation passes through
        # it, so a `cargo test` line there refutes the skip outright.
        run, gate, census = self.passing_inputs()
        census["cargo_invocations"].append("test --all-targets --no-fail-fast")
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, self.narrowed_vitest_pack(), census)

        self.assertTrue(any("cargo test was invoked" in item for item in violations))

    def test_rejects_a_silent_shim(self) -> None:
        # No recorded invocation at all means the shim never reached PATH. Its
        # silence is then absence of evidence, and must not read as proof.
        run, gate, census = self.passing_inputs()
        census["cargo_invocations"] = []
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, self.narrowed_vitest_pack(), census)

        self.assertTrue(
            any("cannot witness anything" in item for item in violations)
        )

    def test_rejects_a_truncated_census(self) -> None:
        # The census stops recording at its cap. An absence read off a partial
        # set is not an absence, so the case fails instead of passing.
        run, gate, census = self.passing_inputs()
        census["truncated"] = True
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, self.narrowed_vitest_pack(), census)

        self.assertTrue(
            any("cannot witness an absence" in item for item in violations)
        )

    def test_rejects_a_pack_that_published_a_command_for_the_skip(self) -> None:
        # The pack has to tell the same story: a gate that ran nothing records
        # no command.
        run, gate, census = self.passing_inputs()
        pack = self.narrowed_vitest_pack()
        (pack / "20_quality" / "cargo_test.result.json").write_text(
            json.dumps({"command": "cargo test --all-targets"}), encoding="utf-8"
        )
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, pack, census)

        self.assertTrue(
            any("not '<no command recorded>'" in item for item in violations)
        )

    def test_rejects_a_blocking_gate_row(self) -> None:
        run, gate, census = self.passing_inputs()
        gate["checks"][0]["blocking"] = True
        gate["decision"]["verdict"] = "BLOCK"
        violations: list[str] = []

        MODULE.assert_js_only(violations, run, gate, self.narrowed_vitest_pack(), census)

        self.assertEqual(len(violations), 2)


if __name__ == "__main__":
    unittest.main()
