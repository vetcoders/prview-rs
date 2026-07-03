#!/usr/bin/env python3
"""
Compare two prview report.json files and display the delta in checks, findings, and metrics.
Usage: compare_runs.py <old_report.json> <new_report.json>
"""

import sys
import json
import os

def load_json(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)

def main():
    if len(sys.argv) != 3:
        print(f"Usage: {os.path.basename(sys.argv[0])} <old_report.json> <new_report.json>")
        sys.exit(1)

    old_path, new_path = sys.argv[1], sys.argv[2]
    old = load_json(old_path)
    new = load_json(new_path)

    print(f"=== Comparing Runs ===")
    print(f"Old: {old.get('timestamp', 'unknown')}")
    print(f"New: {new.get('timestamp', 'unknown')}\n")

    # Gate Decision
    old_gate = old.get("gate", {}).get("allow_merge", False)
    new_gate = new.get("gate", {}).get("allow_merge", False)
    
    if old_gate != new_gate:
        status = "🟢 FIXED" if new_gate else "🔴 REGRESSED"
        print(f"Merge Gate: {old_gate} -> {new_gate} ({status})\n")

    # Checks
    old_checks = {c["name"]: c["status"] for c in old.get("checks", [])}
    new_checks = {c["name"]: c["status"] for c in new.get("checks", [])}
    
    all_checks = set(old_checks.keys()) | set(new_checks.keys())
    for name in sorted(all_checks):
        o_stat = old_checks.get(name, "missing")
        n_stat = new_checks.get(name, "missing")
        if o_stat != n_stat:
            if o_stat == "failure" and n_stat == "passed":
                print(f"🟢 CHECK FIXED: {name}")
            elif o_stat == "passed" and n_stat == "failure":
                print(f"🔴 CHECK FAILED: {name}")
            else:
                print(f"🟡 CHECK CHANGED: {name} ({o_stat} -> {n_stat})")

    # Findings Count
    old_findings = sum(1 for c in old.get("checks", []) for f in c.get("findings", []))
    new_findings = sum(1 for c in new.get("checks", []) for f in c.get("findings", []))
    
    diff = new_findings - old_findings
    if diff > 0:
        print(f"\n🔴 Total Findings: {old_findings} -> {new_findings} (+{diff})")
    elif diff < 0:
        print(f"\n🟢 Total Findings: {old_findings} -> {new_findings} ({diff})")
    else:
        print(f"\n⚪ Total Findings: unchanged ({new_findings})")

if __name__ == "__main__":
    main()
