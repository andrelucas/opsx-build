#!/usr/bin/env python3
"""Deterministic workflow fixture; no model or provider requests."""
import json
import os
from pathlib import Path
import subprocess
import sys

if "--version" in sys.argv:
    print("Claude fixture")
    raise SystemExit
if any(arg.startswith("/rename ") for arg in sys.argv):
    print(json.dumps({"type": "result", "subtype": "success", "result": "Renamed"}))
    raise SystemExit

root = Path(os.environ["PROPORTIONAL_ROOT"])
prompt = sys.stdin.readline() if "stream-json" in sys.argv else "\n".join(sys.argv)
if ("CONTRACT PREFLIGHT — READ ONLY, REPORT ONLY" in prompt or
        (os.environ.get("PROPORTIONAL_MODE") == "incomplete-preflight" and not Path(".git/opsx-build/last-run.json").exists())):
    with (root / "preflights.jsonl").open("a") as out:
        out.write(json.dumps({"prompt": prompt, "arguments": sys.argv}) + "\n")
    status = "BLOCKED" if "CONTRADICTORY" in Path("contract.md").read_text() else "READY"
    summary = ("contract.md#OWN-01 requires a successful UNAVAILABLE reply, but acceptance.sh:3 requires an unavailable error under the same reuse profile. The maintainer must reconcile the protected assertion before implementation."
               if status == "BLOCKED" else "Reviewed contract.md, inventory.json and acceptance.sh. Consistent repair inputs; a failing seed is expected.")
    mode = os.environ.get("PROPORTIONAL_MODE", "repair")
    if mode == "edit-preflight":
        Path("program.py").write_text("print('unauthorized correction')\n")
    if mode == "incomplete-preflight":
        status = "INCOMPLETE"
        summary = "Acceptance oracle review remains unfinished."
    print(json.dumps({"type": "result", "subtype": "success", "structured_output": {"opsx_status": status, "summary": summary}}))
    raise SystemExit
state = json.loads(Path(".git/opsx-build/last-run.json").read_text())
stage = state["stage"]
name = state["change"]
mode = os.environ.get("PROPORTIONAL_MODE", "repair")
calls = root / "calls.jsonl"
with calls.open("a") as out:
    out.write(json.dumps({"stage": stage, "change": name, "bootstrap": state["bootstrap"], "prompt": prompt}) + "\n")
result = {"opsx_status": "READY", "summary": f"Fixture completed {name} {stage}"}
change = Path("openspec/changes") / name

def git_commit():
    paths = [p for p in ("automation", "openspec", "program.py", "DELIVERY.md", "AGENTS.md", "CLAUDE.md") if Path(p).exists()]
    subprocess.run(["git", "add", *paths], check=True)
    dirty = subprocess.run(["git", "diff", "--cached", "--quiet"]).returncode
    if dirty:
        subprocess.run(["git", "commit", "-qm", f"Fixture {name} {stage}"], check=True)

def brief(path):
    path.write_text("# Example delivery\n\n## Objective\nRepair the running program while preserving its other behaviour.\n\n## Prerequisites\nPending render.\n\n## Supplied requirements\nPending render.\n\n## Acceptance Criteria\nThe assigned behaviour works.\n\n## Required Tests\nRun the assigned checks and deliver DELIVERY.md.\n")

def render():
    subprocess.run([os.environ["PROPORTIONAL_RUNNER"], "--no-config", "--no-campaign-config", "render-agenda"], check=True)

if stage == "explore":
    pass
elif state["bootstrap"] and stage in ("propose", "repair"):
    if "CONTRADICTORY" in Path("contract.md").read_text():
        result = {"opsx_status": "BLOCKED", "summary": "contract.md#OWN-01 contradicts the protected test. The maintainer must correct those inputs before implementation."}
    elif mode == "interrupt" and not (root / "interrupted").exists():
        (root / "interrupted").touch()
        result = {"opsx_status": "BLOCKED", "summary": "Fixture interruption before assignment completion"}
    else:
        record = json.loads((root / "record.json").read_text())
        if mode == "rebuild":
            record["slices"].append({"path": "0002-program.md", "requires": ["0001-delivery.md"]})
            record["obligations"][1]["accountable"] = "0002-program.md"
            record["acceptance"][1]["slices"].append("0002-program.md")
        if mode in ("missing", "repair-assignment") and stage == "propose":
            record["obligations"].pop(0)
        if mode == "cycle":
            record["slices"][0]["requires"] = ["0002-program.md"]
            record["slices"].append({"path": "0002-program.md", "requires": ["0001-delivery.md"]})
        Path("automation/slices").mkdir(parents=True, exist_ok=True)
        Path("automation/slices/assignments.json").write_text(json.dumps(record))
        for entry in record["slices"]:
            brief(Path("automation/slices") / entry["path"])
        brief(Path("automation/slices/9999-project-acceptance.md"))
        if mode not in ("missing", "cycle", "repair-assignment") or stage == "repair":
            render()
        if mode == "stale":
            p = Path("automation/slices/9999-project-acceptance.md")
            p.write_text(p.read_text().replace("sh acceptance.sh", "true"))
elif stage == "propose" and name == "9999-project-acceptance":
    record = json.loads(Path("automation/slices/assignments.json").read_text())
    path = "0003-maintainer-correction.md"
    record["slices"].append({"path": path, "requires": [record["slices"][-1]["path"]]})
    for allocation in record["obligations"]:
        allocation["contributors"].append(path)
    for route in record["acceptance"]:
        if route["id"] == "BUILDER-01":
            route["slices"].append(path)
    Path("automation/slices/assignments.json").write_text(json.dumps(record))
    brief(Path("automation/slices") / path)
    render()
    result["opsx_status"] = "REPLANNED"
elif stage == "propose":
    change.mkdir(parents=True, exist_ok=True)
    (change / ".openspec.yaml").write_text("schema: opsx-supplied-contracts\n")
    (change / "proposal.md").write_text("## Purpose\nRepair the program and deliver its report.\n\n## Supplied requirements\n- [OWN-01](../../../contract.md#OWN-01)\n\n## Planning references\nSee the existing assignment.\n")
    (change / "tasks.md").write_text("- [ ] 1.1 Repair the program, test it and write DELIVERY.md.\n")
elif stage in ("apply", "repair"):
    message = os.environ.get("EXPECTED_MESSAGE", "repaired")
    if mode == "final-failure" and stage == "apply":
        message = "incorrect"
    Path("program.py").write_text(f"print({message!r})\n")
    Path("DELIVERY.md").write_text(f"Delivered message: {message}\n")
    if mode == "rebuild" and name == "0001-delivery":
        observed = subprocess.check_output([sys.executable, "program.py"], text=True)
        assert observed.strip() == message
        Path(".git/early-startup").write_text(observed)
    if change.is_dir():
        (change / "tasks.md").write_text("- [x] 1.1 Repair the program, test it and write DELIVERY.md.\n")
elif stage == "verify":
    if not state["bootstrap"]:
        assert Path("DELIVERY.md").is_file(), "The required delivery report was not implemented"
    result["opsx_status"] = "VERIFIED"
    result["review"] = {"findings": [], "areas": [{"id": "complete-owned-scope", "sources": ["contract.md#OWN-01"], "paths": ["program.py", "DELIVERY.md"], "status": "reviewed", "limitations": "Deterministic orchestration fixture"}]}
    if mode == "unrelated-finding":
        result["opsx_status"] = "RETRY"
        result["review"]["findings"] = [{"id": "", "kind": "implementation", "origin": "new", "evidence_kind": "code_review", "status": "open", "sources": ["contract.md#OWN-02"], "scope": ["program.py"], "consequence": "An independent implementation defect remains", "evidence": "Fixture finding", "repair": "Fix the implementation without changing the inputs"}]
elif stage == "archive":
    target = Path("openspec/changes/archive") / ("2026-10-27-" + name)
    target.parent.mkdir(parents=True, exist_ok=True)
    change.rename(target)
elif stage in ("proposal-commit", "final-commit"):
    git_commit()
else:
    raise AssertionError((stage, name, prompt))

print(json.dumps({"type": "result", "subtype": "success", "session_id": "proportional-fixture", "structured_output": result, "result": result["summary"]}))
