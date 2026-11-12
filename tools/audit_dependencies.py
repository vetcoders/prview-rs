#!/usr/bin/env python3
"""Audit with checked exceptions for rmcp's uncompiled OAuth/HTTP clients."""

import json
from pathlib import Path
import subprocess
import sys

ADVISORIES = ("RUSTSEC-2026-0457", "RUSTSEC-2026-0462")
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
EXPECTED_FEATURES = {
    "base64", "client", "default", "macros", "server",
    "transport-async-rw", "transport-child-process", "transport-io",
}


def verify_rmcp_scope(metadata, repo):
    """Refuse any graph outside the reviewed registry rmcp 2.0.0 feature set."""
    try:
        packages = metadata["packages"]
        nodes = metadata["resolve"]["nodes"]
        package_ids = [p["id"] for p in packages]
        node_ids = [n["id"] for n in nodes]
        if len(package_ids) != len(set(package_ids)) or len(node_ids) != len(set(node_ids)):
            raise ValueError("duplicate package or resolution node")
        by_id = {p["id"]: p for p in packages}
        if set(package_ids) != set(node_ids):
            raise ValueError("package and resolution node sets differ")
        root = metadata["resolve"]["root"]
        if root not in by_id or by_id[root]["name"] != "prview":
            raise ValueError("PrView resolution root is missing")
        if metadata["workspace_members"] != [root]:
            raise ValueError("reviewed single-package workspace changed")
        if Path(metadata["workspace_root"]).resolve() != repo.resolve():
            raise ValueError("metadata describes another workspace")
        if Path(by_id[root]["manifest_path"]).resolve() != (repo / "Cargo.toml").resolve():
            raise ValueError("metadata describes another PrView manifest")
        for node in nodes:
            dependencies = node["dependencies"]
            edges = node["deps"]
            if not isinstance(dependencies, list) or not isinstance(edges, list):
                raise ValueError("invalid dependency edges")
            if any(i not in by_id for i in dependencies):
                raise ValueError("dependency edge has no resolution node")
            if set(dependencies) != {edge["pkg"] for edge in edges}:
                raise ValueError("dependency edge representations differ")
        rmcp = [p for p in packages if p["name"] == "rmcp"]
        if len(rmcp) != 1 or rmcp[0]["version"] != "2.0.0" or rmcp[0]["source"] != REGISTRY:
            raise ValueError("rmcp source/version changed; review or remove the exception")
        matches = [n for n in nodes if n["id"] == rmcp[0]["id"]]
        if len(matches) != 1:
            raise ValueError("rmcp resolution is missing")
        features = matches[0]["features"]
        if not isinstance(features, list) or any(not isinstance(f, str) for f in features):
            raise ValueError("invalid resolved features")
        if set(features) != EXPECTED_FEATURES:
            raise ValueError("rmcp feature set changed; OAuth/HTTP exceptions are not established")
        if any(p["name"] in {"oauth2", "reqwest"} for p in packages):
            raise ValueError("OAuth/HTTP dependencies appeared; review the exception")
    except (KeyError, TypeError) as exc:
        raise ValueError("incomplete Cargo metadata") from exc


def main():
    """Prove the approved graph before passing two exceptions to cargo-audit."""
    repo = Path(__file__).resolve().parents[1]
    try:
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1"],
            cwd=repo, check=True, stdout=subprocess.PIPE, text=True,
        )
        verify_rmcp_scope(json.loads(result.stdout), repo)
    except (subprocess.SubprocessError, OSError, ValueError) as exc:
        print(f"Security exception refused: {exc}", file=sys.stderr)
        return 1
    print(
        f"{', '.join(ADVISORIES)}: reviewed rmcp 2.0.0 OAuth/HTTP client code "
        "is not compiled; applying CI-only exceptions.", flush=True,
    )
    command = ["cargo", "audit"]
    for advisory in ADVISORIES:
        command.extend(["--ignore", advisory])
    return subprocess.run(command, cwd=repo).returncode


if __name__ == "__main__":
    sys.exit(main())
