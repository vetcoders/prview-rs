from __future__ import annotations

import json
import os
import pathlib
import subprocess
import tempfile
import textwrap
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/gate.yml"


def workflow_step(name: str) -> str:
    content = WORKFLOW.read_text()
    marker = f"      - name: {name}\n"
    start = content.index(marker)
    end = content.find("\n      - ", start + len(marker))
    return content[start:] if end == -1 else content[start:end]


class GateShadowReceiptTests(unittest.TestCase):
    def run_gate_step(self, payload: bytes, status: int, matching_source: bool = True):
        with tempfile.TemporaryDirectory(prefix="prview gate receipt ") as temporary:
            root = pathlib.Path(temporary)
            subprocess.run(["git", "init", "-q", str(root)], check=True, timeout=10)
            subprocess.run(
                ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "test: fixture"],
                cwd=root, check=True, timeout=10,
            )
            checkout = subprocess.check_output(
                ["git", "rev-parse", "HEAD"], cwd=root, text=True, timeout=10,
            ).strip()
            source = checkout if matching_source else "f" * 40
            binary = root / "target/release/prview"
            binary.parent.mkdir(parents=True)
            binary.write_text(
                "#!/usr/bin/env python3\n"
                "import os, pathlib, sys\n"
                "if sys.argv[1:] == ['--build-source-sha']:\n"
                "    print(os.environ['FIXTURE_SOURCE_SHA'])\n"
                "elif sys.argv[1:] == ['gate', '--json']:\n"
                "    sys.stdout.buffer.write(pathlib.Path('payload').read_bytes())\n"
                "    sys.stderr.write('fixture diagnostics\\n')\n"
                "    sys.exit(int(os.environ['FIXTURE_EXIT_CODE']))\n"
                "else:\n"
                "    sys.exit(99)\n"
            )
            binary.chmod(0o755)
            (root / "payload").write_bytes(payload)
            runner = root / "runner temp"
            runner.mkdir()
            environment = dict(
                os.environ, RUNNER_TEMP=str(runner), GITHUB_OUTPUT=str(root / "outputs"),
                GITHUB_STEP_SUMMARY=str(root / "summary"), FIXTURE_SOURCE_SHA=source,
                FIXTURE_EXIT_CODE=str(status),
            )
            script = textwrap.dedent(workflow_step("Run prview gate").split("        run: |\n", 1)[1])
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script],
                cwd=root, env=environment, capture_output=True, text=True, timeout=15,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual((runner / "prview-gate.json").read_bytes(), payload)
            self.assertEqual((runner / "prview-gate.stderr.log").read_text(), "fixture diagnostics\n")
            receipt = json.loads((runner / "prview-gate-receipt.json").read_text())
            self.assertEqual(receipt["process_exit_code"], status)
            self.assertEqual(receipt["build_source_sha"], source)
            self.assertEqual(receipt["checkout_sha"], checkout)
            self.assertEqual(receipt["source_matches_checkout"], matching_source)
            return receipt, (root / "summary").read_text()

    def test_shadow_retains_exact_evidence_for_verdicts_and_nonzero_exits(self):
        for verdict, disposition, status in [
            ("PASS", "clean", 0), ("CONDITIONAL", "warnings_only", 0),
            ("CONDITIONAL", "review_required", 0), ("BLOCK", "block", 1),
            ("CONDITIONAL", "review_required", 2),
        ]:
            with self.subTest(verdict=verdict, disposition=disposition, status=status):
                payload = json.dumps({"verdict": verdict, "enforcement_disposition": disposition,
                                      "exit_code": status}, indent=2).encode() + b"\n"
                receipt, summary = self.run_gate_step(payload, status)
                self.assertIs(receipt["exit_matches_json"], True)
                self.assertIn(f"JSON verdict | `{verdict}`", summary)
                self.assertIn("Shadow success is not a PASS verdict", summary)

    def test_execution_errors_keep_raw_output_without_claiming_a_json_exit(self):
        for payload, status in [(b"", 3), (b"not JSON\n", 3), (b"", 130)]:
            with self.subTest(payload=payload, status=status):
                receipt, summary = self.run_gate_step(payload, status)
                self.assertIsNone(receipt["json_exit_code"])
                self.assertIsNone(receipt["exit_matches_json"])
                self.assertIn("unavailable", summary)

    def test_process_exit_and_source_mismatches_remain_visible(self):
        receipt, summary = self.run_gate_step(b'{"verdict":"BLOCK","exit_code":1}', 0, False)
        self.assertIs(receipt["exit_matches_json"], False)
        self.assertIn("Exit matches JSON | `false`", summary)
        self.assertIn("Source matches checkout | `false`", summary)

    def test_workflow_builds_exact_source_and_preserves_receipt_without_sarif(self):
        build = workflow_step("Build prview from source")
        self.assertIn('PRVIEW_SOURCE_SHA="$(git rev-parse HEAD)"', build)
        upload = workflow_step("Upload gate evidence")
        self.assertIn("always()", upload)
        self.assertNotIn("sarif", upload.lower())
        for name in ["prview-gate.json", "prview-gate.stderr.log", "prview-gate-receipt.json"]:
            self.assertIn(name, upload)
        self.assertIn("retention-days: 7", upload)
        self.assertIn("test_gate_shadow_receipt.py", workflow_step("Test gate evidence contract"))


if __name__ == "__main__":
    unittest.main()
