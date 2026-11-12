"""Static security and consent contract for the repository Gate Shadow workflow."""

from __future__ import annotations

import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "gate.yml"


class GateWorkflowContractTests(unittest.TestCase):
    def test_gate_shadow_explicitly_opts_in_at_its_gate_invocation(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        gate_invocations = [
            line.strip()
            for line in workflow.splitlines()
            if line.strip().startswith("./target/release/prview gate ")
        ]
        self.assertEqual(
            gate_invocations,
            [
                './target/release/prview gate "${gate_args[@]}" '
                '--allow-project-execution --json > "$json_path"'
            ],
        )
        self.assertIn("permissions:\n  contents: read", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("  pull_request:\n", workflow)
        self.assertNotIn("pull_request_target:", workflow)
        self.assertNotIn("secrets.", workflow)


if __name__ == "__main__":
    unittest.main()
