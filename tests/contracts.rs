//! Exercise the Propose boundary without a real model or provider.
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("opsx-contract-boundary-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("repo/openspec")).unwrap();
        fs::create_dir_all(root.join("repo/automation/slices")).unwrap();
        fs::write(
            root.join("repo/contract.md"),
            "# Contract\nREQ-01: Original required behaviour.\n",
        )
        .unwrap();
        fs::write(
            root.join("repo/openspec/config.yaml"),
            "schema: spec-driven\n",
        )
        .unwrap();
        fs::write(root.join("repo/automation/slices/0001-delivery.md"), "# Delivery\n\n## Supplied requirements\n- [REQ-01](../../contract.md#REQ-01) — implement this slice.\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "user.name", "Fixture"],
            vec!["add", "."],
            vec!["commit", "-qm", "Supplied inputs"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(root.join("repo"))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        fs::write(
            root.join("bin/openspec"),
            r#"#!/bin/sh
case "$1" in
 --version) echo '1.13.2';;
 list)
  if [ -f openspec/changes/0001-delivery/proposal.md ]; then
   echo '{"changes":[{"name":"0001-delivery","lastModified":"written"}]}'
  else echo '{"changes":[]}' ; fi;;
 status)
  schema=opsx-supplied-contracts
  if [ "$CONTRACT_TEST_MODE" = ordinary ]; then schema=spec-driven; fi
  if [ -f openspec/changes/0001-delivery/wrong-schema ]; then schema=spec-driven; fi
  printf '{"isPlanningComplete":true,"schemaName":"%s","nextSteps":[]}\n' "$schema";;
 *) exit 80;;
esac
"#,
        )
        .unwrap();
        fs::write(root.join("bin/claude"), r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'Claude fixture'; exit 0; fi
for arg in "$@"; do
 case "$arg" in /rename\ *) echo '{"type":"result","subtype":"success","result":"Renamed"}'; exit 0;; esac
 case "$arg" in stream-json) streamed=true;; esac
done
if [ "$streamed" = true ]; then IFS= read -r prompt; fi
printf 'call\n' >> "$CONTRACT_TEST_ROOT/calls"
count=$(wc -l < "$CONTRACT_TEST_ROOT/calls" | tr -d ' ')
printf '%s\n' "$@" > "$CONTRACT_TEST_ROOT/args-$count"
printf '%s\n' "$prompt" >> "$CONTRACT_TEST_ROOT/args-$count"
change=openspec/changes/0001-delivery
mkdir -p "$change"
status=READY
summary='Fixture proposal'
case "$CONTRACT_TEST_MODE:$count" in
 ordinary:1)
  mkdir -p "$change/specs/greeting"
  printf '# Greeting\nReturn the requested greeting.\n' > "$change/proposal.md"
  printf '# Design\nUse the existing entry point.\n' > "$change/design.md"
  printf '%s\n' '- [ ] Implement and test the greeting.' > "$change/tasks.md"
  printf '## ADDED Requirements\n### Requirement: Greeting\nReturn the requested greeting.\n' > "$change/specs/greeting/spec.md";;
 ordinary:2)
  git add "$change"; git commit -qm 'openspec: propose 0001-delivery' || exit 90;;
 ordinary:3) status=BLOCKED; summary='Fixture reached implementation Apply';;
 edit:1) printf 'Changed expectation\n' > contract.md;;
 repair:1|reject:*|wrong-schema:1)
  printf '## Supplied requirements\n- [Invented](../../../contract.md#REQ-99)\n' > "$change/proposal.md"
  if [ "$CONTRACT_TEST_MODE" = wrong-schema ]; then touch "$change/wrong-schema"; fi;;
 repair:2|wrong-schema:2)
  rm -f "$change/wrong-schema"
  printf '## Supplied requirements\n- [REQ-01](../../../contract.md#REQ-01) — implement task 1.1\n' > "$change/proposal.md";;
 *) status=BLOCKED; summary='Fixture reached proposal commit';;
esac
printf '{"type":"result","subtype":"success","session_id":"fixture-session","result":"%s","structured_output":{"opsx_status":"%s","summary":"%s"}}\n' "$summary" "$status" "$summary"
"#).unwrap();
        for name in ["openspec", "claude"] {
            fs::set_permissions(
                root.join("bin").join(name),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        Self { root }
    }
    fn run(&self, mode: &str, resume: bool) -> Output {
        self.run_with(mode, resume, &[])
    }
    fn run_with(&self, mode: &str, resume: bool, extra: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_opsx-build"));
        command
            .current_dir(self.root.join("repo"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("CONTRACT_TEST_ROOT", &self.root)
            .env("CONTRACT_TEST_MODE", mode)
            .args([
                "--no-config",
                "--no-campaign-config",
                "--worker-command",
                "claude",
                "--worker-backend",
                "claude",
                "--apply-command",
                "noop",
                "--verify-command",
                "noop",
                "--archive-command",
                "noop",
            ]);
        if resume {
            command.arg("--resume");
        } else if mode == "ordinary" {
            command.arg("advance");
        } else {
            command.args(["--supplied-contracts", "--contract=contract.md", "advance"]);
        }
        command.args(extra).output().unwrap()
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.root.join("calls"))
            .unwrap()
            .lines()
            .count()
    }
    fn state(&self) -> serde_json::Value {
        serde_json::from_str(
            &fs::read_to_string(self.root.join("repo/.git/opsx-build/last-run.json")).unwrap(),
        )
        .unwrap()
    }
    fn save_state(&self, state: &serde_json::Value) {
        fs::write(
            self.root.join("repo/.git/opsx-build/last-run.json"),
            serde_json::to_vec(state).unwrap(),
        )
        .unwrap();
    }

    fn acceptance(&self, script: &str) {
        fs::write(self.root.join("repo/contract_test.sh"), script).unwrap();
        let first = self.run_with(
            "repair",
            false,
            &[
                "--acceptance-file=contract_test.sh",
                "--acceptance-command=sh contract_test.sh",
                "--acceptance-timeout-seconds=2",
            ],
        );
        assert!(String::from_utf8_lossy(&first.stderr).contains("Fixture reached proposal commit"));
        let mut state = self.state();
        state["stage"] = "archive".into();
        state["agenda"] = serde_json::Value::Null;
        self.save_state(&state);
        fs::remove_file(self.root.join("calls")).unwrap();
        fs::write(self.root.join("bin/claude"), r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'Claude fixture'; exit 0; fi
for arg in "$@"; do
 case "$arg" in /rename\ *) echo '{"type":"result","subtype":"success","result":"Renamed"}'; exit 0;; esac
 case "$arg" in stream-json) streamed=true;; esac
done
if [ "$streamed" = true ]; then IFS= read -r prompt; fi
printf 'call\n' >> "$CONTRACT_TEST_ROOT/calls"
count=$(wc -l < "$CONTRACT_TEST_ROOT/calls" | tr -d ' ')
printf '%s\n' "$@" "$prompt" > "$CONTRACT_TEST_ROOT/args-$count"
status=READY
action=blocked
case "$CONTRACT_TEST_MODE:$count" in
 pass:1|late:1|mutate-test:1|repair-gate:3) action=archive;;
 pass:2|late:2|mutate-test:2|repair-gate:4|late:4) action=commit;;
 repair-gate:1|late:3) action=repair;;
 repair-gate:2) status=VERIFIED; action=verify;;
esac
case "$action" in
 archive) mkdir -p openspec/changes/archive; mv openspec/changes/0001-delivery openspec/changes/archive/2026-09-28-0001-delivery;;
 commit)
  if [ "$CONTRACT_TEST_MODE:$count" = late:2 ]; then rm implementation.ready; fi
  if [ "$CONTRACT_TEST_MODE" = mutate-test ]; then printf 'exit 0\n' > contract_test.sh; fi
  git add .; git commit -qm 'Fixture completion' || exit 90;;
 repair) touch implementation.ready;;
 blocked) status=BLOCKED;;
esac
printf '{"type":"result","subtype":"success","session_id":"fixture-session","result":"%s","structured_output":{"opsx_status":"%s","summary":"%s"}}\n' "$action" "$status" "$action"
"#).unwrap();
    }

    fn acceptance_reports(&self) -> Vec<serde_json::Value> {
        fs::read_dir(self.root.join("repo/.git/opsx-build/acceptance"))
            .unwrap()
            .map(|entry| {
                serde_json::from_slice(
                    &fs::read(entry.unwrap().path().join("result.json")).unwrap(),
                )
                .unwrap()
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn incomplete_apply_preserves_checkpoint_and_partial_work_for_resume_in_both_workflows() {
    for mode in ["ordinary", "repair"] {
        let fixture = Fixture::new();
        assert!(!fixture.run(mode, false).status.success());
        let mut state = fixture.state();
        state["stage"] = "apply".into();
        fixture.save_state(&state);
        let contract = fs::read(fixture.root.join("repo/contract.md")).unwrap();
        fs::remove_file(fixture.root.join("calls")).unwrap();
        fs::write(fixture.root.join("bin/claude"), r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'Claude fixture'; exit 0; fi
for arg in "$@"; do
 case "$arg" in /rename\ *) echo '{"type":"result","subtype":"success","result":"Renamed"}'; exit 0;; esac
 case "$arg" in stream-json) streamed=true;; esac
done
if [ "$streamed" = true ]; then IFS= read -r prompt; fi
printf 'call\n' >> "$CONTRACT_TEST_ROOT/calls"
count=$(wc -l < "$CONTRACT_TEST_ROOT/calls" | tr -d ' ')
case "$count" in
 1) printf 'audit result\n' > partial-work; status=INCOMPLETE;;
 2) status=INCOMPLETE;;
 3) test -f partial-work || exit 90; status=READY;;
 *) status=BLOCKED;;
esac
printf '{"type":"result","subtype":"success","structured_output":{"opsx_status":"%s","summary":"Fixture stage result"}}\n' "$status"
"#).unwrap();
        let first = fixture.run(mode, true);
        assert!(!first.status.success());
        assert!(
            String::from_utf8_lossy(&first.stderr)
                .contains("remains INCOMPLETE after one continuation")
        );
        assert_eq!(fixture.calls(), 2);
        assert_eq!(fixture.state()["stage"], "apply");
        assert_eq!(fixture.state()["change"], state["change"]);
        let resumed = fixture.run(mode, true);
        assert!(!resumed.status.success()); // A genuine Verify blocker stops immediately.
        assert_eq!(fixture.calls(), 4);
        assert_eq!(fixture.state()["stage"], "verify");
        assert_eq!(
            fs::read_to_string(fixture.root.join("repo/partial-work")).unwrap(),
            "audit result\n"
        );
        assert_eq!(
            fs::read(fixture.root.join("repo/contract.md")).unwrap(),
            contract
        );
    }
}

#[test]
fn invalid_references_are_repaired_in_propose_before_the_commit_stage() {
    for mode in ["repair", "wrong-schema"] {
        let fixture = Fixture::new();
        let result = fixture.run(mode, false);
        let output = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success()); // The fixture deliberately stops at commit.
        assert!(
            output.contains("Fixture reached proposal commit"),
            "{output}"
        );
        assert_eq!(fixture.calls(), 3);
        let correction = fs::read_to_string(fixture.root.join("args-2")).unwrap();
        assert!(correction.contains("POSTCONDITION REPAIR"));
        assert!(correction.contains(if mode == "repair" {
            "REQ-99"
        } else {
            "must use schema"
        }));
        assert_eq!(fixture.state()["stage"], "proposal-commit");
        assert!(fixture.state()["contracts"].is_object());
        let commit = fs::read_to_string(fixture.root.join("args-3")).unwrap();
        assert!(commit.contains("This stage packages completed planning"));
        assert!(commit.contains("Reuse prior validation for unchanged artifacts"));
        assert!(commit.contains("Preserve all supplied contracts and acceptance inputs unchanged"));
        assert!(commit.contains("openspec/schemas/opsx-supplied-contracts/"));
        assert!(!commit.contains("Read the relevant original inputs listed below before planning"));
        assert!(!commit.contains("run required artifact validation"));
    }
}

#[test]
fn ordinary_planning_still_delivers_specs_design_and_tasks_to_implementation_apply() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("repo/automation/slices/0001-delivery.md"),
        "# Greeting\n\n## Objective\nReturn the requested greeting and test it.\n",
    )
    .unwrap();
    let result = fixture.run("ordinary", false);
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("Fixture reached implementation Apply")
    );
    assert_eq!(fixture.calls(), 3);
    assert_eq!(fixture.state()["stage"], "apply");
    assert!(fixture.state()["contracts"].is_null());
    for path in [
        "proposal.md",
        "design.md",
        "tasks.md",
        "specs/greeting/spec.md",
    ] {
        assert!(
            fixture
                .root
                .join("repo/openspec/changes/0001-delivery")
                .join(path)
                .is_file()
        );
    }
    let commit = fs::read_to_string(fixture.root.join("args-2")).unwrap();
    assert!(commit.contains("This stage packages completed planning"));
    assert!(!commit.contains("SUPPLIED-CONTRACT WORKFLOW"));
    let apply = fs::read_to_string(fixture.root.join("args-3")).unwrap();
    assert!(apply.contains("Implement or continue implementing this OpenSpec change completely"));
    assert!(apply.contains("run appropriate project checks"));
    assert!(!apply.contains("planning-only bootstrap"));
    assert!(!apply.contains("Do not draft complete future slice documents"));
}

#[test]
fn archive_and_completion_reuse_evidence_across_resume_in_both_workflows() {
    for mode in ["ordinary", "repair"] {
        let fixture = Fixture::new();
        fixture.run(mode, false);
        let mut state = fixture.state();
        state["stage"] = "verify".into();
        state["agenda"] = serde_json::Value::Null;
        fixture.save_state(&state);
        fs::remove_file(fixture.root.join("calls")).unwrap();
        fs::write(
            fixture.root.join("repo/implementation.txt"),
            "verified implementation\n",
        )
        .unwrap();
        fs::write(fixture.root.join("repo/unrelated.txt"), "preserve me\n").unwrap();
        fs::write(fixture.root.join("bin/claude"), r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'Claude fixture'; exit 0; fi
for arg in "$@"; do
 case "$arg" in /rename\ *) echo '{"type":"result","subtype":"success","result":"Renamed"}'; exit 0;; esac
 case "$arg" in stream-json) streamed=true;; esac
done
if [ "$streamed" = true ]; then IFS= read -r prompt; fi
printf 'call\n' >> "$CONTRACT_TEST_ROOT/calls"
count=$(wc -l < "$CONTRACT_TEST_ROOT/calls" | tr -d ' ')
printf '%s\n' "$@" "$prompt" > "$CONTRACT_TEST_ROOT/args-$count"
status=READY
summary='Committed verified work'
case "$count" in
 1) status=VERIFIED; summary='Implementation checks and requirement coverage passed in campaign root; no edits';;
 2)
  if [ "$CONTRACT_TEST_MODE" = ordinary ]; then
   mkdir -p openspec/specs/greeting
   cp openspec/changes/0001-delivery/specs/greeting/spec.md openspec/specs/greeting/spec.md
  fi
  mkdir -p openspec/changes/archive
  mv openspec/changes/0001-delivery openspec/changes/archive/2026-10-01-0001-delivery
  summary='Archived at openspec/changes/archive/2026-10-01-0001-delivery; affected links checked; product unchanged';;
 3) status=BLOCKED; summary='Fixture pauses before completion commit';;
 4) git add openspec implementation.txt; git commit -qm 'openspec: complete 0001-delivery' || exit 90;;
 *) exit 90;;
esac
printf '{"type":"result","subtype":"success","session_id":"fixture-session","result":"%s","structured_output":{"opsx_status":"%s","summary":"%s"}}\n' "$summary" "$status" "$summary"
"#).unwrap();
        let result = fixture.run(mode, true);
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("Fixture pauses before completion commit")
        );
        let state = fixture.state();
        assert_eq!(state["stage"], "final-commit");
        assert_eq!(state["archive"]["change"], "0001-delivery");
        let archive = fs::read_to_string(fixture.root.join("args-2")).unwrap();
        assert!(archive.contains("Prior successful Verify result"));
        assert!(archive.contains("Implementation checks and requirement coverage passed"));
        assert!(archive.contains("Do not repeat planning, semantic review"));
        assert!(
            !archive.contains("Read the relevant original inputs listed below before planning")
        );
        if mode == "ordinary" {
            assert!(archive.contains("choose `Sync now (recommended)`"));
            assert!(
                fixture
                    .root
                    .join("repo/openspec/specs/greeting/spec.md")
                    .is_file()
            );
        } else {
            assert!(archive.contains("Archive them without synchronizing specifications"));
            assert!(!archive.contains("choose `Sync now (recommended)`"));
            assert!(
                !fixture
                    .root
                    .join("repo/openspec/specs/greeting/spec.md")
                    .exists()
            );
        }
        let completion = fs::read_to_string(fixture.root.join("args-3")).unwrap();
        assert!(completion.contains("Prior successful Verify result"));
        assert!(completion.contains("Prior successful Archive result"));
        assert!(completion.contains("affected links checked; product unchanged"));
        assert!(completion.contains("Files changed since Archive"));
        assert!(completion.contains("None among tracked and non-ignored untracked files"));
        assert!(completion.contains("it does not re-verify their design or requirement coverage"));
        assert!(
            !completion.contains("Read the relevant original inputs listed below before planning")
        );
        assert!(!completion.contains("MAINTAINER ACCEPTANCE"));

        // A resumed commit still receives the archive evidence, but cannot claim
        // that a subsequently edited implementation is unchanged.
        fs::write(
            fixture.root.join("repo/implementation.txt"),
            "edited after archive\n",
        )
        .unwrap();
        let result = fixture.run(mode, true);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let completion = fs::read_to_string(fixture.root.join("args-4")).unwrap();
        assert!(completion.contains("Prior successful Archive result"));
        assert!(completion.contains("implementation.txt"));
        assert!(!completion.contains("None among tracked and non-ignored untracked files"));
        assert_eq!(fixture.state()["stage"], "complete");
        assert_eq!(
            fs::read_to_string(fixture.root.join("repo/unrelated.txt")).unwrap(),
            "preserve me\n"
        );
        let status = Command::new("git")
            .args([
                "status",
                "--porcelain",
                "--",
                "unrelated.txt",
                "implementation.txt",
                "openspec",
            ])
            .current_dir(fixture.root.join("repo"))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(status.stdout).unwrap(),
            "?? unrelated.txt\n"
        );
    }
}

#[test]
fn unrepaired_proposal_never_reaches_commit() {
    let fixture = Fixture::new();
    let result = fixture.run("reject", false);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("after one corrective turn"));
    assert_eq!(fixture.calls(), 2);
    assert_eq!(fixture.state()["stage"], "propose");
}

#[test]
fn input_mutation_stops_propose_and_plain_resume_cannot_recapture_it() {
    let fixture = Fixture::new();
    for resume in [false, true] {
        let result = fixture.run("edit", resume);
        let output = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(
            output.contains("protected supplied input changed"),
            "{output}"
        );
        assert_eq!(fixture.calls(), 1);
        assert_eq!(fixture.state()["stage"], "propose");
    }
}

#[test]
fn completed_bootstrap_handoff_keeps_the_original_input_versions() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("repo/public-check.sh"), "exit 99\n").unwrap();
    fixture.run_with(
        "repair",
        false,
        &[
            "--acceptance-file=public-check.sh",
            "--acceptance-command=sh public-check.sh",
        ],
    );
    let mut state = fixture.state();
    let recorded = state["contracts"].clone();
    state["stage"] = "complete".into();
    state["bootstrap"] = true.into();
    state["change"] = "bootstrap-implementation-slices".into();
    state["agenda"] = serde_json::Value::Null;
    state["campaign"] =
        serde_json::json!({"iteration": 1, "max_iterations": null, "completed": []});
    fs::write(
        fixture.root.join("repo/.git/opsx-build/last-run.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    fs::remove_dir_all(fixture.root.join("repo/openspec/changes/0001-delivery")).unwrap();
    fs::remove_file(fixture.root.join("calls")).unwrap();
    let result = fixture.run("edit", true);
    let output = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success());
    assert!(
        output.contains("protected supplied input changed"),
        "{output}"
    );
    let state = fixture.state();
    assert_eq!(state["contracts"], recorded);
    assert_eq!(state["bootstrap"], false);
    assert_eq!(state["change"], "0001-delivery");
    assert_eq!(state["campaign"]["iteration"], 1);
    assert!(
        !fixture
            .root
            .join("repo/.git/opsx-build/acceptance")
            .exists()
    );
}

#[test]
fn supplied_acceptance_failure_repairs_then_reruns_before_archive() {
    let fixture = Fixture::new();
    fixture.acceptance("echo 'fixed public expectation'\ntest -f implementation.ready\n");
    let result = fixture.run("repair-gate", true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fixture.calls(), 4);
    assert_eq!(fixture.state()["stage"], "complete");
    let reports = fixture.acceptance_reports();
    assert_eq!(reports.len(), 2); // Archive and commit alone do not rerun checks.
    assert!(
        reports
            .iter()
            .any(|r| r["outcome"] == "failed" && r["results"][0]["exit_code"] == 1)
    );
    assert!(reports.iter().any(|r| r["outcome"] == "passed"));
    let repair = fs::read_to_string(fixture.root.join("args-1")).unwrap();
    assert!(repair.contains("fixed public expectation"));
    assert!(repair.contains("Keep maintainer-owned acceptance inputs unchanged"));
}

#[test]
fn completion_mutation_invalidates_acceptance_and_gets_repaired() {
    let fixture = Fixture::new();
    fixture.acceptance("test -f implementation.ready\n");
    fs::write(fixture.root.join("repo/implementation.ready"), "").unwrap();
    let result = fixture.run("late", true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fixture.acceptance_reports().len(), 3);
    assert_eq!(fixture.calls(), 4);
    assert!(fixture.root.join("repo/implementation.ready").is_file());
    assert_eq!(fixture.state()["stage"], "complete");
    assert!(
        fs::read_to_string(fixture.root.join("args-3"))
            .unwrap()
            .contains("after change `0001-delivery` was archived")
    );
}

#[test]
fn resumed_gate_cannot_be_replaced_or_waived_and_test_mutation_stops_completion() {
    let fixture = Fixture::new();
    fixture.acceptance("exit 0\n# Maintainer test\n");
    let result = fixture.run_with("pass", true, &["--acceptance-command=true"]);
    assert!(String::from_utf8_lossy(&result.stderr).contains("differ from the checkpoint"));
    let result = fixture.run_with("mutate-test", true, &["--no-acceptance-checks"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("protected supplied input changed"));
    assert_eq!(fixture.acceptance_reports().len(), 1);
    let result = fixture.run("pass", true);
    assert!(String::from_utf8_lossy(&result.stderr).contains("protected supplied input changed"));
}

#[test]
fn done_does_not_bypass_checks_and_timeout_is_retained() {
    let fixture = Fixture::new();
    fixture.acceptance("echo 'waiting for public check' >&2\nsleep 30\n");
    let mut state = fixture.state();
    state["stage"] = "done".into();
    state["change"] = serde_json::Value::Null;
    fixture.save_state(&state);
    let started = std::time::Instant::now();
    let result = fixture.run("pass", true);
    assert!(!result.status.success());
    assert!(started.elapsed().as_secs() < 10);
    assert_eq!(fixture.state()["stage"], "acceptance");
    let reports = fixture.acceptance_reports();
    assert_eq!(reports[0]["results"][0]["outcome"], "timed out");
    assert!(String::from_utf8_lossy(&result.stderr).contains("waiting for public check"));
    assert!(!fixture.root.join("calls").exists());
}

#[test]
fn intermediate_slice_does_not_run_final_acceptance() {
    let fixture = Fixture::new();
    fixture.acceptance("exit 9\n");
    let mut state = fixture.state();
    state["agenda"] = serde_json::json!({"path":"automation/slices/0001-delivery.md", "change":"0001-delivery", "title":"Delivery", "content":"# Delivery", "ordinal":[1]});
    fixture.save_state(&state);
    let result = fixture.run("pass", true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !fixture
            .root
            .join("repo/.git/opsx-build/acceptance")
            .exists()
    );
}

#[test]
fn bootstrap_reference_preflight_repairs_before_model_verify_and_preserves_exhaustion() {
    for budget in [0, 3] {
        let fixture = Fixture::new();
        fixture.run("repair", false);
        let baseline_calls = fixture.calls();
        let mut state = fixture.state();
        state["stage"] = "verify".into();
        state["bootstrap"] = true.into();
        state["change"] = "bootstrap-implementation-slices".into();
        state["agenda"] = serde_json::Value::Null;
        fixture.save_state(&state);
        let active_change = fixture
            .root
            .join("repo/openspec/changes/bootstrap-implementation-slices");
        fs::create_dir_all(&active_change).unwrap();
        fs::write(active_change.join("proposal.md"), "Bootstrap planning").unwrap();
        let agenda = fixture.root.join("repo/automation/slices");
        let original = "## Supplied requirements\n- [REQ-01](../../contract.md#REQ-01)\n";
        fs::write(agenda.join("README.md"), original).unwrap();
        let headings = "# Slice\n## Objective\nImplement.\n## Prerequisites\nNone.\n## Acceptance Criteria\nSource requirements.\n## Required Tests\nSource tests.\n";
        fs::write(
            agenda.join("0001-delivery.md"),
            format!("{headings}{original}"),
        )
        .unwrap();
        let terminal = agenda.join("9999-project-acceptance.md");
        let navigation = "- [Proposal](../../openspec/changes/0001-delivery/proposal.md)\n- [Coverage](README.md)\n- [Coverage again](README.md)\n";
        fs::write(&terminal, format!("{headings}{original}{navigation}\n## Project Goal Coverage\nAll source requirements.\n")).unwrap();
        let budget_arg = format!("--max-verify-retries={budget}");
        let result = fixture.run_with("repair", true, &[&budget_arg]);
        assert!(!result.status.success());
        let state = fixture.state();
        assert_eq!(state["verify_retries"], 1);
        assert!(
            state["pending_repair"]
                .as_str()
                .unwrap()
                .contains("3 issue(s)")
        );
        assert!(
            state["pending_repair"]
                .as_str()
                .unwrap()
                .contains("## Planning references")
        );
        if budget == 0 {
            assert_eq!(fixture.calls(), baseline_calls); // No expensive Verify or Repair turn.
            assert_eq!(state["stage"], "verify");
            assert!(String::from_utf8_lossy(&result.stderr).contains("Latest findings are saved"));
            // A maintainer correction can resume semantic verification without resetting.
            fs::write(&terminal, format!("{headings}{original}\n## Planning references\n{navigation}\n## Project Goal Coverage\nAll source requirements.\n")).unwrap();
            fixture.run_with("repair", true, &[&budget_arg]);
            assert_eq!(fixture.calls(), baseline_calls + 1);
            let prompt =
                fs::read_to_string(fixture.root.join(format!("args-{}", baseline_calls + 1)))
                    .unwrap();
            assert!(prompt.contains("VERIFIED"));
            assert!(!prompt.contains("Repair the planning-only implementation agenda"));
            // Premature archival must also stop before a model Verify turn.
            let archive = fixture.root.join("repo/openspec/changes/archive");
            fs::create_dir_all(&archive).unwrap();
            fs::rename(&active_change, archive.join("2026-09-28-bootstrap")).unwrap();
            let result = fixture.run_with("repair", true, &[&budget_arg]);
            assert!(!result.status.success());
            assert_eq!(fixture.calls(), baseline_calls + 1);
            assert!(
                fixture.state()["pending_repair"]
                    .as_str()
                    .unwrap()
                    .contains("restore the generated bootstrap artifacts")
            );
        } else {
            assert_eq!(fixture.calls(), baseline_calls + 1); // Goes directly to Repair.
            assert_eq!(state["stage"], "repair");
            let prompt =
                fs::read_to_string(fixture.root.join(format!("args-{}", baseline_calls + 1)))
                    .unwrap();
            assert!(prompt.contains(
                "Repair only the runner-reported structural or source-reference defects"
            ));
            assert!(prompt.contains("The subsequent Verify stage owns semantic review"));
            assert!(prompt.contains("Do not repeat the complete Apply workflow"));
            assert!(!prompt.contains("noop bootstrap-implementation-slices"));
            assert!(!prompt.contains("SUPPLIED-CONTRACT WORKFLOW"));
            assert!(!prompt.contains("Update the coverage map and required tests coherently"));
            assert!(prompt.contains("Do not archive or commit it"));
            assert!(prompt.contains("3 issue(s)"));
            // Actual semantic findings still use normal Repair in both workflows.
            for supplied_contracts in [true, false] {
                let mut semantic = state.clone();
                semantic["pending_repair"] =
                    "An acceptance criterion contradicts the required outcome.".into();
                if !supplied_contracts {
                    semantic["contracts"] = serde_json::Value::Null;
                }
                fixture.save_state(&semantic);
                fixture.run("repair", true);
                let prompt =
                    fs::read_to_string(fixture.root.join(format!("args-{}", fixture.calls())))
                        .unwrap();
                assert!(prompt.contains("noop bootstrap-implementation-slices"));
                assert!(!prompt.contains("Repair only the runner-reported structural"));
                assert!(prompt.contains(if supplied_contracts {
                    "Update the coverage map and required tests coherently"
                } else {
                    "Repair the material blocker in this ordinary planning-only bootstrap"
                }));
            }
        }
    }
}
