#!/usr/bin/env python3
import sys
import json
import os

def generate_comment(artifacts_dir):
    review_path = os.path.join(artifacts_dir, "PR_REVIEW.md")
    gate_path = os.path.join(artifacts_dir, "00_summary", "MERGE_GATE.json")
    
    if not os.path.exists(review_path) or not os.path.exists(gate_path):
        print("Error: Missing PR_REVIEW.md or MERGE_GATE.json in artifacts directory.", file=sys.stderr)
        sys.exit(1)

    with open(gate_path, "r") as f:
        gate = json.load(f)

    with open(review_path, "r") as f:
        review_lines = f.readlines()

    status_icon = "✅" if gate.get("allow_merge") else "🛑"
    verdict = gate.get("verdict", "Unknown")
    reason = gate.get("reason", "")

    print(f"### {status_icon} prview: {verdict}")
    if reason:
        print(f"**Blocked because:** {reason}")
    print("")

    print("<details><summary><b>Review Summary</b></summary>\n")
    # Print the first few lines of the review summary
    for line in review_lines[:20]:
        print(line, end="")
    if len(review_lines) > 20:
        print("\n...")
    print("</details>")

if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <artifacts_dir>")
        sys.exit(1)
    
    generate_comment(sys.argv[1])
