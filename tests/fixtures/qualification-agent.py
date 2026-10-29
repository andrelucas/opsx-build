#!/usr/bin/env python3
"""Scripted qualification cases; checks prompt delivery, not model reasoning."""
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
if "CONTRACT PREFLIGHT — READ ONLY, REPORT ONLY" in prompt:
    print(json.dumps({'type': 'result', 'subtype': 'success', 'structured_output': {'opsx_status': 'READY', 'summary': 'Fixture contract preflight found consistent inputs'}}))
    raise SystemExit
with (root / "calls").open("a") as out:
    out.write("call\n")
count = len((root / "calls").read_text().splitlines())
(root / f"args-{count}").write_text(prompt)
if count <= 2:
    for instruction in (
        "Generated plans must not promote later qualification",
        "correct derived tasks that contradict that boundary",
        "cite the original source path and requirement/section",
        "missing prerequisite still blocks even when a narrower component check passes",
    ):
        assert instruction in prompt, instruction

result = {"opsx_status": "BLOCKED", "summary": "Fixture stops at Archive"}
if count == 1:
    Path("candidate.txt").write_text("owned harness and assertions\n")
    if "Execute fresh-peer qualification before component delivery" in Path("contract.md").read_text():
        result["summary"] = (
            "contract.md#QUAL-01 requires fresh-peer qualification during Apply; "
            "the required observation cannot execute because fresh-peer is absent. "
            "Supply the peer executable and its configuration to continue."
        )
    else:
        Path("openspec/changes/0001-delivery/tasks.md").write_text(
            "- [x] Deliver and check owned harness and assertions.\n"
            "Fresh-peer qualification remains pending after assembly under contract.md#QUAL-01; "
            "the assembly owner must supply fresh-peer and its configuration, then run the native observation.\n"
        )
        result = {"opsx_status": "READY", "summary": "Owned work delivered; later fresh-peer qualification explicitly pending"}
elif count == 2:
    result = {"opsx_status": "VERIFIED", "summary": "Independently checked owned harness; fresh-peer qualification remains pending",
              "review": {"findings": [], "areas": [{"id": "owned-harness", "sources": ["contract.md#QUAL-01"],
                         "paths": ["candidate.txt"], "status": "reviewed", "limitations": "Fresh-peer qualification pending after assembly"}]}}

print(json.dumps({"type": "result", "subtype": "success", "session_id": "qualification-fixture",
                  "structured_output": result, "result": result["summary"]}))
