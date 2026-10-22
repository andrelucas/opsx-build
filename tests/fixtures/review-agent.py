#!/usr/bin/env python3
"""Deterministic stage responses; no provider or model requests."""
import json
import os
from pathlib import Path
import sys

if "--version" in sys.argv:
    print("Claude fixture")
    raise SystemExit
if any(arg.startswith("/rename ") for arg in sys.argv):
    print(json.dumps({"type": "result", "subtype": "success", "result": "Renamed"}))
    raise SystemExit

root = Path(os.environ["CONTRACT_TEST_ROOT"])
prompt = sys.stdin.readline() if "stream-json" in sys.argv else "\n".join(sys.argv)
with (root / "calls").open("a") as out:
    out.write("call\n")
count = len((root / "calls").read_text().splitlines())
(root / f"args-{count}").write_text("\n".join(sys.argv) + "\n" + prompt)
mode = os.environ["CONTRACT_TEST_MODE"]
status = "READY"
summary = "Apply checked the required outcome; remaining claims need independent review"
report = None
if count == 1:
    Path("candidate.txt").write_text("implemented\n")
elif count in (2, 4) or (mode == "review-partial" and count >= 2):
    partial = mode == "review-partial"
    finding = {
        "id": "" if count == 2 or partial else "F0001",
        "kind": "assertion",
        "origin": "previously_missed",
        "evidence_kind": "reproduced",
        "status": "open" if count == 2 or partial else "closed",
        "sources": ["contract.md#REQ-01"],
        "scope": ["required outcome and sibling refusal paths"],
        "consequence": "The required refusal could be omitted without failing a test",
        "evidence": "A focused counterexample demonstrates the missing assertion",
        "repair": "Cover the governing rule and related alternatives",
    }
    report = {
        "findings": [] if count == 4 and mode == "review-omit" else [finding],
        "areas": [{"id": "required-outcome", "sources": ["contract.md#REQ-01"],
                   "paths": ["candidate.txt"], "status": "partial" if partial else "reviewed",
                   "limitations": "No native runtime qualification claimed"}],
    }
    status = "RETRY" if count == 2 and not partial else "VERIFIED"
    summary = "Required assertion missing" if count == 2 else "Independently checked the repaired rule and alternatives"
elif count == 3:
    Path("candidate.txt").write_text("fixed\n")
    summary = "Repair claims the rule and relevant sibling paths are corrected"
else:
    status = "BLOCKED"
    summary = "Fixture stops at Archive after independent final acceptance"

output = {"opsx_status": status, "summary": summary}
if report is not None:
    output["review"] = report
print(json.dumps({"type": "result", "subtype": "success", "session_id": "review-fixture",
                  "structured_output": output, "result": summary}))
