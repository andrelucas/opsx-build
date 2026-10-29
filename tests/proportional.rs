use serde_json::{Value, json};
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
        let root = std::env::temp_dir().join(format!("opsx-proportional-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("repo")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        for (file, contents) in [
            (
                "contract.md",
                include_str!("fixtures/assignment-workflow/contract.md"),
            ),
            (
                "inventory.json",
                include_str!("fixtures/assignment-workflow/inventory.json"),
            ),
            (
                "context.md",
                "# Example repair\nRepair a seeded runnable program and preserve its complete contract.\n",
            ),
            ("program.py", "print('seed')\n"),
            (
                "acceptance.sh",
                "#!/bin/sh\nset -eu\nprintf 'gate\\n' >> .git/gate-calls\ntest \"$(python3 program.py)\" = repaired\ntest -s DELIVERY.md\n",
            ),
        ] {
            fs::write(root.join("repo").join(file), contents).unwrap();
        }
        fs::write(
            root.join("record.json"),
            include_str!("fixtures/assignment-workflow/assignments.json"),
        )
        .unwrap();
        fs::write(
            root.join("bin/claude"),
            include_str!("fixtures/proportional-agent.py"),
        )
        .unwrap();
        fs::write(root.join("bin/openspec"), r#"#!/usr/bin/env python3
import json, sys
from pathlib import Path
if sys.argv[1] == '--version': print('1.13.2')
elif sys.argv[1] == 'init': Path('openspec').mkdir(exist_ok=True)
elif sys.argv[1] == 'list': print(json.dumps({'changes': [{'name': p.name, 'lastModified': str(p.stat().st_mtime_ns)} for p in Path('openspec/changes').glob('*') if p.is_dir() and p.name != 'archive']}))
elif sys.argv[1] == 'status': print(json.dumps({'isPlanningComplete': True, 'schemaName': 'opsx-supplied-contracts', 'nextSteps': []}))
else: raise AssertionError(sys.argv)
"#).unwrap();
        for name in ["claude", "openspec"] {
            fs::set_permissions(
                root.join("bin").join(name),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let fixture = Self { root };
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "user.name", "Maintainer"],
            vec!["add", "."],
            vec!["commit", "-qm", "Prepared repair"],
        ] {
            fixture.git(&args);
        }
        fixture
    }
    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(self.root.join("repo"))
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
    fn command(&self) -> Command {
        self.command_with_config(false)
    }
    fn command_with_config(&self, configured: bool) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_opsx-build"));
        c.current_dir(self.root.join("repo"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.root.join("bin").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("PROPORTIONAL_ROOT", &self.root)
            .env("PROPORTIONAL_RUNNER", env!("CARGO_BIN_EXE_opsx-build"))
            .args([
                "--no-config",
                "--worker-command=claude",
                "--worker-backend=claude",
                "--frontier-command=claude",
                "--frontier-backend=claude",
                "--apply-command=noop",
                "--verify-command=noop",
                "--archive-command=noop",
            ]);
        if !configured {
            c.arg("--no-campaign-config");
        }
        c
    }
    fn execute(&self, mode: &str) -> Output {
        self.command()
            .env("PROPORTIONAL_MODE", mode)
            .args([
                "--supplied-contracts",
                "--contract=contract.md",
                "--obligation-inventory=inventory.json",
                "--acceptance-file=acceptance.sh",
                "--acceptance-command=sh acceptance.sh",
                "--acceptance-timeout-seconds=37",
                "execute",
                "--context=context.md",
            ])
            .output()
            .unwrap()
    }
    fn preflight(&self) -> Output {
        self.command()
            .args([
                "--supplied-contracts",
                "--contract=contract.md",
                "--obligation-inventory=inventory.json",
                "--acceptance-file=acceptance.sh",
                "--acceptance-command=sh acceptance.sh",
                "--acceptance-timeout-seconds=37",
                "--frontier-model=frontier-preflight",
                "preflight",
            ])
            .output()
            .unwrap()
    }
    fn preflights(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("preflights.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn state(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.root.join("repo/.git/opsx-build/last-run.json")).unwrap(),
        )
        .unwrap()
    }
    fn calls(&self) -> Vec<Value> {
        fs::read_to_string(self.root.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn success(output: &Output) {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn seeded_repair_has_one_delivery_review_report_and_independent_final_gate() {
    let f = Fixture::new();
    let output = f.execute("repair");
    Fixture::success(&output);
    let progress = String::from_utf8_lossy(&output.stderr);
    let bootstrap = progress.split("Bootstrap complete —").next().unwrap();
    assert!(bootstrap.contains("[1/2] Propose"), "{bootstrap}");
    assert!(bootstrap.contains("[2/2] Completion commit"), "{bootstrap}");
    assert!(!bootstrap.contains("[6/6]"), "{bootstrap}");
    assert!(
        progress.contains("[1/2] Maintainer acceptance"),
        "{progress}"
    );
    let calls = f.calls();
    assert_eq!(f.preflights().len(), 1);
    assert_eq!(calls.iter().filter(|c| c["stage"] == "verify").count(), 1);
    assert!(
        calls
            .iter()
            .filter(|c| c["bootstrap"] == true)
            .all(|c| c["stage"] == "propose" || c["stage"] == "final-commit")
    );
    assert!(
        !calls
            .iter()
            .any(|c| c["change"] == "9999-project-acceptance"
                && (c["stage"] == "propose" || c["stage"] == "apply"))
    );
    assert!(f.root.join("repo/DELIVERY.md").is_file());
    assert!(
        !f.root
            .join("repo/openspec/changes/bootstrap-implementation-slices")
            .exists()
    );
    assert!(
        !f.root
            .join("repo/openspec/changes/9999-project-acceptance")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(f.root.join("repo/.git/gate-calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(f.state()["stage"], "done");
}

#[test]
fn maintainer_preflight_reports_conflict_and_bootstrap_reuses_the_stop_before_planning() {
    let f = Fixture::new();
    let repo = f.root.join("repo");
    let contract = fs::read_to_string(repo.join("contract.md")).unwrap();
    fs::write(
        repo.join("contract.md"),
        format!("{contract}\nCONTRADICTORY: resolve-delete must succeed with UNAVAILABLE.\n"),
    )
    .unwrap();
    fs::write(repo.join("acceptance.sh"), "#!/bin/sh\n# Same reuse profile, after retirement.\n# resolve-delete must fail with OUTCOME_UNAVAILABLE.\nexit 1\n").unwrap();
    let before = fs::read(repo.join("acceptance.sh")).unwrap();
    let result = f.preflight();
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("BLOCKED during contract preflight"),
        "{error}"
    );
    assert!(
        error.contains("contract.md#OWN-01") && error.contains("acceptance.sh:3"),
        "{error}"
    );
    assert_eq!(f.preflights().len(), 1);
    assert!(
        f.preflights()[0]["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|arg| arg == "frontier-preflight")
    );
    let prompt = f.preflights()[0]["prompt"].as_str().unwrap().to_owned();
    assert!(prompt.contains("negative controls") && prompt.contains("unchanged `retirement`"));
    assert!(prompt.contains("Do not edit, generate, format, stage, commit, repair or correct"));
    assert!(!repo.join(".git/opsx-build/last-run.json").exists());
    assert!(!repo.join("automation").exists());

    let bootstrap = f.execute("repair");
    assert!(!bootstrap.status.success());
    assert!(
        String::from_utf8_lossy(&bootstrap.stderr).contains("BLOCKED during contract preflight")
    );
    assert_eq!(f.preflights().len(), 1);
    assert!(f.calls().is_empty());
    assert!(!repo.join("automation/slices/assignments.json").exists());
    assert_eq!(fs::read(repo.join("acceptance.sh")).unwrap(), before);
    assert_eq!(
        fs::read_to_string(repo.join("program.py")).unwrap(),
        "print('seed')\n"
    );
}

#[test]
fn successful_preflight_is_reused_until_inputs_or_commands_change() {
    let f = Fixture::new();
    Fixture::success(&f.preflight());
    Fixture::success(&f.execute("repair"));
    assert_eq!(f.preflights().len(), 1);
    let checkpoint = fs::read(f.root.join("repo/.git/opsx-build/last-run.json")).unwrap();
    fs::write(
        f.root.join("repo/acceptance.sh"),
        "#!/bin/sh\n# maintainer correction\nexit 0\n",
    )
    .unwrap();
    Fixture::success(&f.command().arg("preflight").output().unwrap());
    assert_eq!(f.preflights().len(), 2);
    assert_eq!(
        fs::read(f.root.join("repo/.git/opsx-build/last-run.json")).unwrap(),
        checkpoint
    );
    let result = f.command().arg("--resume").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("accept-changes"));
    Fixture::success(&f.command().arg("accept-changes").output().unwrap());
    Fixture::success(&f.command().arg("preflight").output().unwrap());
    assert_eq!(f.preflights().len(), 2);
    Fixture::success(
        &f.command()
            .args([
                "--supplied-contracts",
                "--contract=contract.md",
                "--obligation-inventory=inventory.json",
                "--acceptance-file=acceptance.sh",
                "--acceptance-command=true",
                "preflight",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(f.preflights().len(), 3);
}

#[test]
fn preflight_rejects_reviewer_edits_and_unfinished_review() {
    for mode in ["edit-preflight", "incomplete-preflight"] {
        let f = Fixture::new();
        let output = f
            .command()
            .env("PROPORTIONAL_MODE", mode)
            .args([
                "--supplied-contracts",
                "--contract=contract.md",
                "--acceptance-file=acceptance.sh",
                "preflight",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if mode == "edit-preflight" {
                "changed repository files"
            } else {
                "unfinished"
            }),
            "{error}"
        );
        assert!(
            !f.root
                .join("repo/.git/opsx-build/contract-preflight.json")
                .exists()
        );
        assert!(f.calls().is_empty());
    }
}

#[test]
fn preflight_missing_inputs_and_dry_run_never_dispatch_a_model() {
    let f = Fixture::new();
    Fixture::success(
        &f.command()
            .args([
                "--supplied-contracts",
                "--contract=contract.md",
                "--acceptance-file=acceptance.sh",
                "--dry-run",
                "preflight",
            ])
            .output()
            .unwrap(),
    );
    assert!(!f.root.join("repo/.git/opsx-build").exists());
    fs::remove_file(f.root.join("repo/acceptance.sh")).unwrap();
    let result = f.preflight();
    assert!(!result.status.success());
    assert!(f.preflights().is_empty());
    assert!(f.calls().is_empty());
}

#[test]
fn advance_checks_changed_contracts_before_dispatching_delivery() {
    let f = Fixture::new();
    let settings = [
        "--supplied-contracts",
        "--contract=contract.md",
        "--obligation-inventory=inventory.json",
        "--acceptance-file=acceptance.sh",
        "--acceptance-command=sh acceptance.sh",
    ];
    Fixture::success(
        &f.command()
            .args(settings)
            .arg("bootstrap")
            .output()
            .unwrap(),
    );
    let calls = f.calls().len();
    let path = f.root.join("repo/contract.md");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{text}\nCONTRADICTORY\n")).unwrap();
    let result = f.command().args(settings).arg("advance").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("BLOCKED during contract preflight"));
    assert_eq!(f.calls().len(), calls);
    assert_eq!(f.preflights().len(), 2);
    assert!(!f.root.join("repo/openspec/changes/0001-delivery").exists());
    assert_eq!(
        fs::read_to_string(f.root.join("repo/program.py")).unwrap(),
        "print('seed')\n"
    );
}

#[test]
fn rebuild_keeps_multiple_slices_planning_review_and_final_integration_review() {
    let f = Fixture::new();
    let output = f.execute("rebuild");
    Fixture::success(&output);
    let progress = String::from_utf8_lossy(&output.stderr);
    assert!(progress.contains("[1/2] Verify"), "{progress}");
    assert!(progress.contains("[1/3] Verify"), "{progress}");
    assert!(
        progress.contains("[2/3] Maintainer acceptance"),
        "{progress}"
    );
    assert!(progress.contains("[3/3] Completion commit"), "{progress}");
    let calls = f.calls();
    assert!(
        calls
            .iter()
            .any(|c| c["bootstrap"] == true && c["stage"] == "verify")
    );
    assert!(
        calls
            .iter()
            .any(|c| c["change"] == "0002-program" && c["stage"] == "apply")
    );
    assert!(
        calls
            .iter()
            .any(|c| c["change"] == "9999-project-acceptance" && c["stage"] == "verify")
    );
    assert!(f.root.join("repo/.git/early-startup").is_file());
}

#[test]
fn failed_final_acceptance_repairs_and_independently_reviews_before_running_again() {
    let f = Fixture::new();
    Fixture::success(&f.execute("final-failure"));
    let calls = f.calls();
    assert!(
        calls
            .iter()
            .any(|c| c["change"] == "9999-project-acceptance" && c["stage"] == "repair")
    );
    assert!(
        calls
            .iter()
            .any(|c| c["change"] == "9999-project-acceptance" && c["stage"] == "verify")
    );
    assert_eq!(
        fs::read_to_string(f.root.join("repo/.git/gate-calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn completed_planning_survives_a_pause_and_resume_without_another_review() {
    let f = Fixture::new();
    let out = f
        .command()
        .args([
            "--max-iterations=1",
            "--supplied-contracts",
            "--contract=contract.md",
            "--obligation-inventory=inventory.json",
            "--acceptance-file=acceptance.sh",
            "--acceptance-command=sh acceptance.sh",
            "execute",
        ])
        .output()
        .unwrap();
    Fixture::success(&out);
    assert_eq!(f.state()["stage"], "complete");
    let count = f.calls().len();
    Fixture::success(
        &f.command()
            .args(["--resume", "--loop", "--max-iterations=2"])
            .output()
            .unwrap(),
    );
    assert!(
        f.calls()[count..]
            .iter()
            .all(|c| c["stage"] != "propose" && c["stage"] != "verify")
    );
    assert_eq!(f.state()["stage"], "done");
}

#[test]
fn exhausted_final_failure_resumes_at_repair_without_rerunning_the_known_failure() {
    let f = Fixture::new();
    let out = f
        .command()
        .env("PROPORTIONAL_MODE", "final-failure")
        .args([
            "--max-verify-retries=0",
            "--supplied-contracts",
            "--contract=contract.md",
            "--obligation-inventory=inventory.json",
            "--acceptance-file=acceptance.sh",
            "--acceptance-command=sh acceptance.sh",
            "execute",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(f.state()["stage"], "repair");
    let count = f.calls().len();
    assert!(
        !f.command()
            .args(["--resume", "--max-verify-retries=0"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(f.calls().len(), count);
    Fixture::success(
        &f.command()
            .args(["--resume", "--max-verify-retries=1"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(f.root.join("repo/.git/gate-calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn contradictory_input_stops_once_then_accepts_maintainer_edit_without_model_calls() {
    let f = Fixture::new();
    let contract = f.root.join("repo/contract.md");
    let source = fs::read_to_string(&contract).unwrap();
    fs::write(&contract, format!("{source}\nCONTRADICTORY\n")).unwrap();
    let result = f.execute("repair");
    assert!(!result.status.success());
    assert_eq!(f.calls().len(), 0);
    assert_eq!(f.preflights().len(), 1);
    fs::write(&contract, source).unwrap();
    let resume = f.command().arg("--resume").output().unwrap();
    assert!(!resume.status.success());
    assert!(String::from_utf8_lossy(&resume.stderr).contains("accept-changes"));
    let calls = f.calls().len();
    Fixture::success(&f.command().arg("accept-changes").output().unwrap());
    assert_eq!(f.calls().len(), calls);
    Fixture::success(&f.command().arg("--resume").output().unwrap());
    assert_eq!(f.state()["stage"], "done");
}

#[test]
fn correction_to_completed_slice_arranges_new_work_and_runs_corrected_acceptance() {
    let f = Fixture::new();
    Fixture::success(&f.execute("repair"));
    fs::write(f.root.join("repo/acceptance.sh"), "#!/bin/sh\nset -eu\nprintf 'gate\\n' >> .git/gate-calls\ntest \"$(python3 program.py)\" = corrected\ntest -s DELIVERY.md\n").unwrap();
    let before = f.calls().len();
    Fixture::success(&f.command().arg("accept-changes").output().unwrap());
    assert_eq!(f.calls().len(), before);
    Fixture::success(
        &f.command()
            .env("EXPECTED_MESSAGE", "corrected")
            .arg("--resume")
            .output()
            .unwrap(),
    );
    assert!(
        f.calls()[before..]
            .iter()
            .any(|c| c["change"] == "0003-maintainer-correction" && c["stage"] == "apply")
    );
    assert_eq!(
        fs::read_to_string(f.root.join("repo/.git/gate-calls"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        f.state()["campaign"]["completed"][0]["change"],
        "0001-delivery"
    );
}

#[test]
fn accept_changes_adopts_added_removed_inputs_inventory_and_changed_gate_settings() {
    let f = Fixture::new();
    Fixture::success(&f.execute("repair"));
    let repo = f.root.join("repo");
    fs::rename(repo.join("acceptance.sh"), repo.join("extra.sh")).unwrap();
    let mut inventory: Value =
        serde_json::from_slice(&fs::read(repo.join("inventory.json")).unwrap()).unwrap();
    inventory["acceptance"][0]["source"] = "extra.sh".into();
    fs::write(
        repo.join("replacement-inventory.json"),
        serde_json::to_vec(&inventory).unwrap(),
    )
    .unwrap();
    let args = json!([
        "--supplied-contracts",
        "--contract=contract.md",
        "--obligation-inventory=replacement-inventory.json",
        "--acceptance-file=extra.sh",
        "--acceptance-command=sh extra.sh",
        "--acceptance-command=test -s DELIVERY.md",
        "--acceptance-timeout-seconds=19"
    ]);
    fs::write(repo.join("opsx-build.md"), format!("# Campaign\n\n<!-- opsx-build:arguments -->\n```json\n{args}\n```\n<!-- /opsx-build:arguments -->\n")).unwrap();
    let calls = f.calls().len();
    Fixture::success(
        &f.command_with_config(true)
            .arg("accept-changes")
            .output()
            .unwrap(),
    );
    assert_eq!(f.calls().len(), calls);
    assert_eq!(
        f.state()["contracts"]["acceptance"]["commands"],
        json!(["sh extra.sh", "test -s DELIVERY.md"])
    );
    assert_eq!(f.state()["contracts"]["acceptance"]["timeout_seconds"], 19);
    assert!(
        f.state()["contracts"]["obligation_inventory"]
            .as_str()
            .unwrap()
            .ends_with("replacement-inventory.json")
    );
    Fixture::success(
        &f.command_with_config(true)
            .arg("--resume")
            .output()
            .unwrap(),
    );
    assert!(
        fs::read_to_string(repo.join("automation/slices/9999-project-acceptance.md"))
            .unwrap()
            .contains("19 seconds")
    );
}

#[test]
fn correction_keeps_open_findings_requests_and_unrelated_work() {
    let f = Fixture::new();
    let out = f
        .command()
        .env("PROPORTIONAL_MODE", "unrelated-finding")
        .args([
            "--max-verify-retries=0",
            "--supplied-contracts",
            "--contract=contract.md",
            "--obligation-inventory=inventory.json",
            "--acceptance-file=acceptance.sh",
            "--acceptance-command=sh acceptance.sh",
            "execute",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let before = f.state();
    assert_eq!(before["review"]["findings"]["F0001"]["status"], "open");
    let ledger =
        PathBuf::from(before["operating"]["directory"].as_str().unwrap()).join("ledger.json");
    let requests: Value = serde_json::from_slice(&fs::read(&ledger).unwrap()).unwrap();
    fs::write(f.root.join("repo/unrelated.txt"), "maintainer work\n").unwrap();
    fs::write(
        f.root.join("repo/contract.md"),
        format!(
            "{}\nClarified by maintainer.\n",
            fs::read_to_string(f.root.join("repo/contract.md")).unwrap()
        ),
    )
    .unwrap();
    Fixture::success(&f.command().arg("accept-changes").output().unwrap());
    let after = f.state();
    assert_eq!(after["review"], before["review"]);
    assert_eq!(after["verify_retries"], 0);
    assert_eq!(
        fs::read_to_string(f.root.join("repo/unrelated.txt")).unwrap(),
        "maintainer work\n"
    );
    let current: Value = serde_json::from_slice(&fs::read(ledger).unwrap()).unwrap();
    assert_eq!(current["requests"], requests["requests"]);
    assert_eq!(current["reviews"], requests["reviews"]);
}

#[test]
fn interrupted_assignment_resumes_and_invalid_graph_stops_before_semantic_review() {
    let f = Fixture::new();
    assert!(!f.execute("interrupt").status.success());
    Fixture::success(&f.command().arg("--resume").output().unwrap());
    assert_eq!(
        f.calls()
            .iter()
            .filter(|c| c["bootstrap"] == true && c["stage"] == "propose")
            .count(),
        2
    );
    let broken = Fixture::new();
    let out = broken.execute("cycle");
    assert!(!out.status.success());
    assert!(!broken.calls().iter().any(|c| c["stage"] == "verify"));
    assert!(
        broken.state()["pending_repair"]
            .as_str()
            .unwrap()
            .contains("cycles")
    );
}

#[test]
fn accepting_ordinary_context_changes_needs_no_inventory_or_model_session() {
    let f = Fixture::new();
    let repo = f.root.join("repo");
    fs::create_dir_all(repo.join("openspec")).unwrap();
    fs::write(
        repo.join("openspec/config.yaml"),
        "schema: spec-driven\ncontext: |\n  Original introduction\nrules: {}\n",
    )
    .unwrap();
    Fixture::success(
        &f.command()
            .args(["--change=ordinary-repair", "Repair the message"])
            .output()
            .unwrap(),
    );
    fs::write(
        repo.join("context.md"),
        "# Updated introduction\nExplain the corrected work in ordinary English.\n",
    )
    .unwrap();
    let calls = f.calls().len();
    let rejected = f
        .command()
        .args(["--contract=contract.md", "accept-changes"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(f.state()["contracts"].is_null());
    Fixture::success(
        &f.command()
            .args(["--worker-command=not-installed", "accept-changes"])
            .output()
            .unwrap(),
    );
    assert_eq!(f.calls().len(), calls);
    assert!(f.state()["contracts"].is_null());
    assert!(
        fs::read_to_string(repo.join("openspec/config.yaml"))
            .unwrap()
            .contains("Updated introduction")
    );
    Fixture::success(&f.command().arg("--resume").output().unwrap());
}

#[test]
fn accepting_a_templated_introduction_retains_its_original_definitions() {
    let f = Fixture::new();
    let brief = f.root.join("repo/brief.md");
    fs::write(&brief, "# Example\nRepair the {{language}} program.\n").unwrap();
    Fixture::success(
        &f.command()
            .args([
                "--supplied-contracts",
                "--contract=contract.md",
                "--obligation-inventory=inventory.json",
                "--acceptance-file=acceptance.sh",
                "--acceptance-command=sh acceptance.sh",
                "--define=language=Python",
                "--context=brief.md",
                "execute",
            ])
            .output()
            .unwrap(),
    );
    fs::write(
        &brief,
        "# Corrected introduction\nRepair the {{language}} program and preserve its report.\n",
    )
    .unwrap();
    Fixture::success(&f.command().arg("accept-changes").output().unwrap());
    let config = fs::read_to_string(f.root.join("repo/openspec/config.yaml")).unwrap();
    assert!(config.contains("Corrected introduction"));
    assert!(config.contains("Repair the Python program"));
    assert!(fs::read_to_string(brief).unwrap().contains("{{language}}"));
}

#[test]
fn dry_run_and_a_live_run_leave_the_checkpoint_untouched() {
    let f = Fixture::new();
    Fixture::success(&f.execute("repair"));
    let before = f.state();
    let ledger =
        PathBuf::from(before["operating"]["directory"].as_str().unwrap()).join("ledger.json");
    let ledger_bytes = fs::read(&ledger).unwrap();
    Fixture::success(
        &f.command()
            .args(["--dry-run", "accept-changes"])
            .output()
            .unwrap(),
    );
    assert_eq!(f.state(), before);
    assert_eq!(fs::read(&ledger).unwrap(), ledger_bytes);
    let lock = std::fs::File::options()
        .read(true)
        .write(true)
        .open(ledger.with_file_name("owner.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let out = f.command().arg("accept-changes").output().unwrap();
    assert!(!out.status.success());
    assert_eq!(f.state(), before);
}
