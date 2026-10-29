#!/usr/bin/env python3
"""Deterministic stage responses; no provider or model requests."""
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys

if "--version" in sys.argv:
    print("Claude fixture")
    raise SystemExit
if any(arg.startswith("/rename ") for arg in sys.argv):
    print(json.dumps({"type": "result", "subtype": "success", "result": "Renamed"}))
    raise SystemExit

root = Path(os.environ["CONTRACT_TEST_ROOT"])
prompt = sys.stdin.readline() if "stream-json" in sys.argv else "\n".join(sys.argv)
if "CONTRACT PREFLIGHT — READ ONLY, REPORT ONLY" in prompt:
    print(json.dumps({'type': 'result', 'subtype': 'success', 'structured_output': {'opsx_status': 'READY', 'summary': 'Fixture contract preflight found consistent inputs'}}))
    raise SystemExit
with (root / "calls").open("a") as out:
    out.write("call\n")
count = len((root / "calls").read_text().splitlines())
(root / f"args-{count}").write_text("\n".join(sys.argv) + "\n" + prompt)
mode = os.environ["CONTRACT_TEST_MODE"]
convergence = mode in ("review-stall", "review-productive")
status = "READY"
summary = "Apply checked the required outcome; remaining claims need independent review"
report = None
if count == 1:
    Path("candidate.txt").write_text("implemented\n")
elif count in (2, 4) or (mode in ("review-partial", "review-no-report") and count >= 2) or (mode == "review-continue" and count in (3, 6)) or (convergence and count % 2 == 0):
    partial = mode == "review-partial" or (mode == "review-continue" and count <= 3)
    finding = {
        "id": "" if count == 2 or (partial and count == 3) else "F0001",
        "kind": "assertion",
        "origin": "previously_missed",
        "evidence_kind": "reproduced",
        "status": "open" if count == 2 or partial or mode == "review-stall" or (mode == "review-continue" and count == 4) else "closed",
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
                   "limitations": "Rerun clean unit and vet checks after removing the temporary diagnostic" if partial else "No native runtime qualification claimed"}],
    }
    if mode == "review-continue":
        report["areas"].append({"id": "completed-audit", "sources": ["contract.md#REQ-01"],
                                "paths": ["candidate.txt"], "status": "reviewed", "limitations": ""})
    status = "RETRY" if count == 2 and not partial else "VERIFIED"
    if mode == "review-continue" and count <= 4:
        status = "RETRY"
    if mode == "review-blocked":
        status = "BLOCKED"
        report["areas"][0]["status"] = "partial"
    if mode == "review-stall":
        status = "RETRY"
    if mode == "review-productive" and count >= 4:
        finding["id"] = f"F{count // 2 - 1:04d}"
        if count < 6:
            report["findings"].append(dict(finding, id="", status="open", consequence="A different genuine required assertion remains missing"))
            status = "RETRY"
    summary = "Required assertion missing" if count == 2 else "Independently checked the repaired rule and alternatives"
elif count == 3 or (mode == "review-continue" and count == 5) or (convergence and count >= 3 and count % 2 == 1 and (mode == "review-stall" or count < 6)):
    Path("candidate.txt").write_text("fixed\n")
    summary = "Repair claims the rule and relevant sibling paths are corrected"
else:
    status = "BLOCKED"
    summary = "Fixture stops at Archive after independent final acceptance"

if mode == "review-checks" and count <= 4:
    helper = shlex.split(re.search(r"For checks you need to run, prefer (.+?) with one JSON object", prompt)[1])
    final = count >= 3 and Path("final-check.sh").exists()
    command = "sh final-check.sh" if final else "printf 'unit check\\n' >> " + shlex.quote(str(root / "unit-check-runs"))
    check = {"key": "final-check" if final else "unit-check", "command": command,
             "cwd": str(Path.cwd()), "tools": ["sh", "cat"] if final else ["sh"], "additional_inputs": [],
             "review_only": [], "runtime": "stable",
             "runtime_reason": "Deterministic fixture without external or clock dependencies",
             "timeout_seconds": 2}
    checked = subprocess.run(helper, input=json.dumps(check), text=True, capture_output=True)
    if checked.returncode:
        raise RuntimeError(checked.stdout + checked.stderr)
    (root / f"check-{count}.json").write_text(checked.stdout)

output = {"opsx_status": status, "summary": summary}
if mode == "review-incomplete-loop":
    output = {"opsx_status": "INCOMPLETE", "summary": "Unfinished owned review work; continuing without a durable result"}
if mode == "review-no-report" and count >= 2:
    output = {"opsx_status": "INCOMPLETE", "summary": "Collect the pending delegated audit before finalizing coverage"}
    report = None
if mode == "review-timeout":
    child = subprocess.Popen(["sleep", "30"])
    (root / "owned-child.pid").write_text(str(child.pid))
    try:
        child.wait()
    finally:
        child.wait()
if report is not None:
    output["review"] = report
print(json.dumps({"type": "result", "subtype": "success", "session_id": "review-fixture",
                  "structured_output": output, "result": summary}))
