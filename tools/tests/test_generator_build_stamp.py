"""Exercise build metadata under the actual release workflow environments."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SOURCE_SHA = "542f2764617a78a858420af2f2b16c7e1f77e09d"


class GeneratorBuildStampTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.directory.cleanup)
        cls.probe = Path(cls.directory.name) / "build-stamp-probe"
        subprocess.run(
            ["rustc", "--edition=2024", str(ROOT / "build.rs"), "-o", str(cls.probe)],
            check=True,
            capture_output=True,
            timeout=30,
        )

    def run_probe(self, **metadata):
        environment = os.environ.copy()
        for key in ("PRVIEW_SOURCE_SHA", "PRVIEW_GENERATOR_RELEASE_TAG", "PRVIEW_RELEASE_TAG"):
            environment.pop(key, None)
        environment.update(CARGO_PKG_VERSION="0.8.0", **metadata)
        return subprocess.run(
            [str(self.probe)], env=environment, text=True, capture_output=True, timeout=10
        )

    def assert_unstamped_tag(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cargo:rustc-env=PRVIEW_BUILD_RELEASE_TAG=\n", result.stdout)

    def test_unstamped_build_is_explicitly_unknown(self):
        result = self.run_probe()
        self.assert_unstamped_tag(result)
        self.assertIn("PRVIEW_BUILD_SOURCE_SHA=unknown\n", result.stdout)

    def test_branch_dry_run_does_not_claim_a_release_tag(self):
        result = self.run_probe(
            PRVIEW_SOURCE_SHA=SOURCE_SHA,
            PRVIEW_RELEASE_TAG="main",
            PRVIEW_GENERATOR_RELEASE_TAG="",
        )
        self.assert_unstamped_tag(result)
        self.assertIn(f"PRVIEW_BUILD_SOURCE_SHA={SOURCE_SHA}\n", result.stdout)

    def test_package_verification_ignores_the_workflow_global_tag(self):
        result = self.run_probe(PRVIEW_RELEASE_TAG="v0.8.0")
        self.assert_unstamped_tag(result)
        self.assertIn("PRVIEW_BUILD_SOURCE_SHA=unknown\n", result.stdout)

    def test_validated_build_can_supply_a_generator_tag(self):
        result = self.run_probe(
            PRVIEW_SOURCE_SHA=SOURCE_SHA, PRVIEW_GENERATOR_RELEASE_TAG="v0.8.0"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PRVIEW_BUILD_RELEASE_TAG=v0.8.0\n", result.stdout)

    def test_source_stamp_is_normalized(self):
        result = self.run_probe(PRVIEW_SOURCE_SHA=SOURCE_SHA.upper())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"PRVIEW_BUILD_SOURCE_SHA={SOURCE_SHA}\n", result.stdout)

    def test_malformed_source_stamp_remains_an_error(self):
        result = self.run_probe(PRVIEW_SOURCE_SHA="not-a-commit")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exact 40-character", result.stderr)

    def test_generator_tag_with_wrong_version_is_rejected(self):
        result = self.run_probe(
            PRVIEW_SOURCE_SHA=SOURCE_SHA, PRVIEW_GENERATOR_RELEASE_TAG="v0.8.1"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must match the Cargo package version", result.stderr)

    def test_generator_tag_without_source_stamp_is_rejected(self):
        result = self.run_probe(PRVIEW_GENERATOR_RELEASE_TAG="v0.8.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires an explicit PRVIEW_SOURCE_SHA", result.stderr)


if __name__ == "__main__":
    unittest.main()
