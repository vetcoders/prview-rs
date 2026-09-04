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


if __name__ == "__main__":
    unittest.main()
