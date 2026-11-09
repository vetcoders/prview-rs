import copy
import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "audit_dependencies", Path(__file__).resolve().parents[1] / "audit_dependencies.py"
)
audit = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(audit)


class AuditScopeTests(unittest.TestCase):
    def setUp(self):
        self.graph = {
            "workspace_root": "/reviewed/prview",
            "workspace_members": ["prview"],
            "packages": [
                {"id": "prview", "name": "prview", "manifest_path": "/reviewed/prview/Cargo.toml"},
                {"id": "rmcp2", "name": "rmcp", "version": "2.0.0", "source": audit.REGISTRY},
            ],
            "resolve": {"root": "prview", "nodes": [
                {"id": "prview", "features": [], "dependencies": ["rmcp2"], "deps": [{"pkg": "rmcp2"}]},
                {"id": "rmcp2", "features": sorted(audit.EXPECTED_FEATURES), "dependencies": [], "deps": []},
            ]},
        }

    def test_reviewed_features_are_accepted(self):
        audit.verify_rmcp_scope(self.graph, Path("/reviewed/prview"))

    def test_auth_http_and_unknown_features_are_refused(self):
        for feature in (
            "auth", "auth-client-credentials-jwt",
            "transport-streamable-http-client",
            "transport-streamable-http-client-reqwest",
            "transport-streamable-http-client-unix-socket", "new-feature",
        ):
            with self.subTest(feature=feature):
                graph = copy.deepcopy(self.graph)
                graph["resolve"]["nodes"][1]["features"].append(feature)
                with self.assertRaises(ValueError):
                    audit.verify_rmcp_scope(graph, Path("/reviewed/prview"))

    def test_changed_version_or_source_is_refused(self):
        for key, value in (
            ("version", "2.0.1"), ("version", "3.6.0"), ("version", "3.7.0"),
            ("source", None), ("source", "git+https://example.test/rmcp"),
        ):
            with self.subTest(key=key, value=value):
                graph = copy.deepcopy(self.graph)
                graph["packages"][1][key] = value
                with self.assertRaises(ValueError):
                    audit.verify_rmcp_scope(graph, Path("/reviewed/prview"))

    def test_missing_duplicate_and_multiple_rmcp_nodes_are_refused(self):
        variants = []
        missing = copy.deepcopy(self.graph)
        missing["resolve"]["nodes"] = []
        variants.append(missing)
        duplicate = copy.deepcopy(self.graph)
        duplicate["resolve"]["nodes"] *= 2
        variants.append(duplicate)
        multiple = copy.deepcopy(self.graph)
        self.add_package(multiple, {"id": "rmcp3", "name": "rmcp", "version": "3.6.0", "source": audit.REGISTRY})
        variants.append(multiple)
        variants.extend(({}, {"packages": [], "resolve": None}))
        for graph in variants:
            with self.subTest(graph=graph), self.assertRaises(ValueError):
                audit.verify_rmcp_scope(graph, Path("/reviewed/prview"))

    def test_oauth_dependencies_are_refused(self):
        for name in ("oauth2", "reqwest"):
            graph = copy.deepcopy(self.graph)
            self.add_package(graph, {"id": name, "name": name})
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "OAuth/HTTP"):
                audit.verify_rmcp_scope(graph, Path("/reviewed/prview"))

    def test_failed_metadata_never_invokes_audit(self):
        with mock.patch.object(
            audit.subprocess, "run",
            side_effect=audit.subprocess.CalledProcessError(1, "cargo"),
        ) as run:
            self.assertEqual(audit.main(), 1)
            self.assertEqual(run.call_count, 1)

    def add_package(self, graph, package):
        """Add a resolved, reachable package without creating a malformed graph."""
        graph["packages"].append(package)
        graph["resolve"]["nodes"].append({"id": package["id"], "features": [], "dependencies": [], "deps": []})
        graph["resolve"]["nodes"][0]["dependencies"].append(package["id"])
        graph["resolve"]["nodes"][0]["deps"].append({"pkg": package["id"]})

    def main_graph(self):
        """Point a test graph at the runner's real repository boundary."""
        graph = copy.deepcopy(self.graph)
        repo = Path(audit.__file__).resolve().parents[1]
        graph["workspace_root"] = str(repo)
        graph["packages"][0]["manifest_path"] = str(repo / "Cargo.toml")
        return graph

    def test_auth_metadata_never_invokes_audit(self):
        graph = self.main_graph()
        graph["resolve"]["nodes"][1]["features"].append("auth")
        stderr = io.StringIO()
        with mock.patch.object(
            audit.subprocess, "run", return_value=mock.Mock(stdout=json.dumps(graph))
        ) as run, mock.patch.object(audit.sys, "stderr", stderr):
            self.assertEqual(audit.main(), 1)
            self.assertEqual(run.call_count, 1)
        self.assertIn("feature set changed", stderr.getvalue())

    def test_balanced_missing_dependency_is_refused(self):
        graph = copy.deepcopy(self.graph)
        self.add_package(graph, {"id": "removed", "name": "removed"})
        graph["packages"].pop()
        graph["resolve"]["nodes"].pop()
        with self.assertRaisesRegex(ValueError, "dependency edge has no resolution node"):
            audit.verify_rmcp_scope(graph, Path("/reviewed/prview"))

    def test_root_workspace_and_edge_metadata_are_required(self):
        variants = []
        for field in ("workspace_root", "workspace_members"):
            graph = copy.deepcopy(self.graph)
            del graph[field]
            variants.append(graph)
        wrong_root = copy.deepcopy(self.graph)
        wrong_root["resolve"]["root"] = "rmcp2"
        variants.append(wrong_root)
        wrong_manifest = copy.deepcopy(self.graph)
        wrong_manifest["packages"][0]["manifest_path"] = "/another/Cargo.toml"
        variants.append(wrong_manifest)
        missing_edges = copy.deepcopy(self.graph)
        del missing_edges["resolve"]["nodes"][0]["deps"]
        variants.append(missing_edges)
        for graph in variants:
            with self.subTest(graph=graph), self.assertRaises(ValueError):
                audit.verify_rmcp_scope(graph, Path("/reviewed/prview"))

    def test_success_preserves_audit_exit_and_scopes_exception_to_one_call(self):
        with mock.patch.object(
            audit.subprocess, "run",
            side_effect=[mock.Mock(stdout=json.dumps(self.main_graph())), mock.Mock(returncode=7)],
        ) as run:
            self.assertEqual(audit.main(), 7)
            self.assertEqual(run.call_count, 2)
            self.assertEqual(
                run.call_args_list[0].args[0],
                ["cargo", "metadata", "--locked", "--format-version", "1"],
            )
            self.assertEqual(
                run.call_args_list[1].args[0],
                ["cargo", "audit", "--ignore", "RUSTSEC-2026-0457", "--ignore", "RUSTSEC-2026-0462"],
            )


if __name__ == "__main__":
    unittest.main()
