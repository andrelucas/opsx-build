//! Full runner boundaries with a deterministic agent, never a live campaign/model.
use serde_json::Value;
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
            std::env::temp_dir().join(format!("opsx-assignment-runner-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("repo")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        for (name, contents) in [
            (
                "contract.md",
                include_str!("fixtures/assignment-workflow/contract.md"),
            ),
            (
                "inventory.json",
                include_str!("fixtures/assignment-workflow/inventory.json"),
            ),
            (
                "acceptance.sh",
                include_str!("fixtures/assignment-workflow/acceptance.sh"),
            ),
            (
                "context.md",
                "Implement the complete supplied example component with owned outputs under src/example.\n",
            ),
        ] {
            fs::write(root.join("repo").join(name), contents).unwrap();
        }
        fs::write(
            root.join("record.json"),
            include_str!("fixtures/assignment-workflow/assignments.json"),
        )
        .unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "user.name", "Fixture"],
            vec!["add", "."],
            vec!["commit", "-qm", "Maintainer inputs"],
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
        // Keep the old bootstrap lifecycle covered using a real version-6
        // checkpoint. Fresh runs use the proportional workflow in proportional.rs.
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("openspec/schemas/opsx-supplied-contracts/templates"))
            .unwrap();
        fs::create_dir_all(repo.join("automation")).unwrap();
        fs::create_dir_all(repo.join(".git/opsx-build")).unwrap();
        fs::write(
            repo.join("openspec/config.yaml"),
            "schema: opsx-supplied-contracts\n",
        )
        .unwrap();
        let bootstrap = include_str!("../assets/supplied-contracts-legacy/bootstrap.md");
        fs::write(repo.join("automation/bootstrap.md"), bootstrap).unwrap();
        for (file, contents) in [
            (
                "schema.yaml",
                include_str!("../assets/supplied-contracts-legacy/schema.yaml"),
            ),
            (
                "templates/proposal.md",
                include_str!("../assets/supplied-contracts-legacy/proposal.md"),
            ),
            (
                "templates/design.md",
                include_str!("../assets/supplied-contracts-legacy/design.md"),
            ),
            (
                "templates/tasks.md",
                include_str!("../assets/supplied-contracts-legacy/tasks.md"),
            ),
        ] {
            fs::write(
                repo.join("openspec/schemas/opsx-supplied-contracts")
                    .join(file),
                contents,
            )
            .unwrap();
        }
        let inputs = ["contract.md", "acceptance.sh", "inventory.json"].iter().map(|name| {
            let path = repo.join(name).canonicalize().unwrap();
            let hash = Command::new("git").current_dir(&repo).args(["hash-object", "--", name]).output().unwrap();
            serde_json::json!({"path":path,"resolved":path,"hash":String::from_utf8(hash.stdout).unwrap().trim(),"executable":false,"contract":*name != "acceptance.sh"})
        }).collect::<Vec<_>>();
        fs::write(repo.join(".git/opsx-build/last-run.json"), serde_json::to_vec(&serde_json::json!({
            "schema_version":6,"request":"bootstrap","change":"bootstrap-implementation-slices","stage":"propose","verify_retries":0,
            "before_changes":{"changes":{}},"planning_session":null,"pending_repair":null,"pending_direction":null,"proposal_head":null,"final_head":null,
            "bootstrap":true,"agenda":{"path":"automation/bootstrap.md","change":"bootstrap-implementation-slices","title":"Legacy bootstrap","content":bootstrap},
            "contracts":{"inputs":inputs,"obligation_inventory":repo.join("inventory.json").canonicalize().unwrap(),
                "acceptance":{"commands":["sh acceptance.sh"],"timeout_seconds":37,"next_stage":"archive","failures":0,"evidence":null}}
        })).unwrap()).unwrap();
        fs::write(
            root.join("bin/openspec"),
            r#"#!/bin/sh
case "$1" in
 --version) echo '1.13.2';;
 init) mkdir -p openspec;;
 list)
  if [ -f openspec/changes/bootstrap-implementation-slices/proposal.md ]; then
   echo '{"changes":[{"name":"bootstrap-implementation-slices","lastModified":"written"}]}'
  else echo '{"changes":[]}'; fi;;
 status) echo '{"isPlanningComplete":true,"schemaName":"opsx-supplied-contracts","nextSteps":[]}';;
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
printf 'call\n' >> "$ASSIGNMENT_ROOT/calls"
count=$(wc -l < "$ASSIGNMENT_ROOT/calls" | tr -d ' ')
printf '%s\n' "$@" "$prompt" > "$ASSIGNMENT_ROOT/prompt-$count"
change=openspec/changes/bootstrap-implementation-slices
status=READY
summary='Fixture stage complete'
case "$count" in
 1)
  mkdir -p "$change"
  printf '## Supplied requirements\n- [OWN-01](../../../contract.md#OWN-01)\n' > "$change/proposal.md"
  printf '## Design\nUse the assignment record.\n' > "$change/design.md"
  printf '%s\n' '- [ ] 1.1 Materialize and check the assignment.' > "$change/tasks.md"
  cp "$ASSIGNMENT_ROOT/record.json" "$change/assignments.json";;
 2) git add .; git commit -qm 'openspec: propose bootstrap-implementation-slices' || exit 90;;
 3)
  mkdir -p automation/slices
  mv "$change/assignments.json" automation/slices/assignments.json
  for name in 0001-delivery.md 9999-project-acceptance.md; do
   printf '# Delivery\n\n## Objective\nImplement the complete assigned sources.\n\n## Prerequisites\nPending render.\n\n## Supplied requirements\nPending render.\n\n## Acceptance Criteria\nSatisfy the full assigned source conditions.\n\n## Required Tests\nImplement real assertions for the assigned observations.\n' > "automation/slices/$name"
  done
  if [ "$ASSIGNMENT_MODE" = missing ] || [ "$ASSIGNMENT_MODE" = repair ]; then
   python3 -c 'import json,pathlib; p=pathlib.Path("automation/slices/assignments.json"); d=json.loads(p.read_text()); d["obligations"].pop(0); p.write_text(json.dumps(d))'
  else
   "$ASSIGNMENT_RUNNER" --no-config --no-campaign-config render-agenda > "$ASSIGNMENT_ROOT/render.log" 2>&1 || exit 91
  fi
  if [ "$ASSIGNMENT_MODE" = stale ]; then
   python3 -c 'import pathlib; p=pathlib.Path("automation/slices/9999-project-acceptance.md"); p.write_text(p.read_text().replace("sh acceptance.sh", "true"))'
  fi
  if [ "$ASSIGNMENT_MODE" = semantic ]; then
   printf '\nAll outcomes require a profile.\n' >> automation/slices/0001-delivery.md
  fi;;
 4)
  status=VERIFIED; summary='Independent fixture review passed'
  if [ "$ASSIGNMENT_MODE" = semantic ]; then
   status=RETRY; summary='OWN-01 contract.md: generated prescription requires profile for every outcome; this rejects permitted non-verified outcomes. Correct the derived brief while preserving the source.'
  fi;;
 *) status=BLOCKED; summary='Fixture stops at Archive';;
esac
if [ "$ASSIGNMENT_MODE:$count" = repair-resume:4 ]; then
 cp "$ASSIGNMENT_ROOT/record.json" automation/slices/assignments.json
 "$ASSIGNMENT_RUNNER" --no-config --no-campaign-config render-agenda > "$ASSIGNMENT_ROOT/render.log" 2>&1 || exit 91
 status=READY; summary='Repaired assignment and rendered its views'
fi
if [ "$ASSIGNMENT_MODE:$count" = repair-resume:5 ]; then status=VERIFIED; summary='Repaired agenda verified'; fi
if [ "$ASSIGNMENT_MODE" = verify-mutation ]; then
 python3 -c 'import json,pathlib; p=pathlib.Path("automation/slices/assignments.json"); d=json.loads(p.read_text()); d["obligations"].pop(0); p.write_text(json.dumps(d))'
 status=VERIFIED; summary='Untrustworthy verification changed the assignment'
fi
printf '{"type":"result","subtype":"success","session_id":"assignment-fixture","result":"%s","structured_output":{"opsx_status":"%s","summary":"%s"}}\n' "$summary" "$status" "$summary"
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
    fn command(&self) -> Command {
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
            .env("ASSIGNMENT_ROOT", &self.root)
            .env("ASSIGNMENT_RUNNER", env!("CARGO_BIN_EXE_opsx-build"))
            .args(["--no-config", "--no-campaign-config"]);
        c
    }
    fn run(&self, mode: &str) -> Output {
        self.command()
            .env("ASSIGNMENT_MODE", mode)
            .args([
                "--worker-command=claude",
                "--worker-backend=claude",
                "--frontier-command=claude",
                "--frontier-backend=claude",
                "--apply-command=noop",
                "--verify-command=noop",
                "--archive-command=noop",
                "--max-verify-retries=0",
                "--supplied-contracts",
                "--contract=contract.md",
                "--obligation-inventory=inventory.json",
                "--acceptance-file=acceptance.sh",
                "--acceptance-command=sh acceptance.sh",
                "--acceptance-timeout-seconds=37",
                "--resume",
            ])
            .output()
            .unwrap()
    }
    fn state(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.root.join("repo/.git/opsx-build/last-run.json")).unwrap(),
        )
        .unwrap()
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.root.join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn bootstrap_materializes_assignment_and_independently_reviews_without_running_acceptance() {
    let f = Fixture::new();
    let result = f.run("valid");
    let output = String::from_utf8_lossy(&result.stderr);
    assert!(
        output.contains("Fixture stops at Archive"),
        "{output}; {}",
        fs::read_to_string(f.root.join("render.log")).unwrap_or_default()
    );
    assert_eq!(f.calls(), 5);
    assert_eq!(f.state()["stage"], "archive");
    assert_eq!(f.state()["schema_version"], 7);
    assert!(
        f.state()["contracts"]["obligation_inventory"]
            .as_str()
            .unwrap()
            .ends_with("inventory.json")
    );
    let verify = fs::read_to_string(f.root.join("prompt-4")).unwrap();
    assert!(verify.contains("Mechanical completeness does not prove source meaning"));
    assert!(verify.contains("weaker substitute acceptance"));
    assert!(verify.contains("Do not render, repair"));
    assert!(!verify.contains("anti-Karen rule. Report RETRY only"));
    assert!(!f.root.join("repo/.git/opsx-build/acceptance").exists());
    let terminal = fs::read_to_string(
        f.root
            .join("repo/automation/slices/9999-project-acceptance.md"),
    )
    .unwrap();
    assert!(terminal.contains("37 seconds"));
    assert!(terminal.contains("sh acceptance.sh"));
    let checkpoint = fs::read(f.root.join("repo/.git/opsx-build/last-run.json")).unwrap();
    assert!(
        f.command()
            .arg("check-agenda")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(
        checkpoint,
        fs::read(f.root.join("repo/.git/opsx-build/last-run.json")).unwrap()
    );
    let before = fs::read(f.root.join("repo/inventory.json")).unwrap();
    assert_eq!(
        before,
        include_bytes!("fixtures/assignment-workflow/inventory.json")
    );
    // Pinned inventory mutation cannot be recaptured by rendering or plain resume.
    fs::write(f.root.join("repo/inventory.json"), b"{}\n").unwrap();
    for args in [
        vec!["render-agenda"],
        vec![
            "--worker-command=claude",
            "--apply-command=noop",
            "--verify-command=noop",
            "--archive-command=noop",
            "--resume",
        ],
    ] {
        let result = f.command().args(args).output().unwrap();
        assert!(!result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("protected supplied input changed"),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    assert_eq!(f.calls(), 5);
}

#[test]
fn missing_assignment_and_weakened_commands_fail_before_semantic_review() {
    for mode in ["missing", "stale"] {
        let f = Fixture::new();
        let result = f.run(mode);
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(
            error.contains(if mode == "missing" {
                "unassigned obligation"
            } else {
                "stale"
            }),
            "{error}"
        );
        assert_eq!(f.calls(), 3);
        assert!(
            f.state()["pending_repair"]
                .as_str()
                .unwrap()
                .contains("deterministic bootstrap agenda check failed")
        );
        assert!(!f.root.join("repo/.git/opsx-build/acceptance").exists());
    }
}

#[test]
fn structurally_complete_assignment_still_requires_semantic_review() {
    let f = Fixture::new();
    let result = f.run("semantic");
    assert!(!result.status.success());
    assert_eq!(f.calls(), 4);
    assert_eq!(f.state()["stage"], "repair");
    assert!(
        f.state()["pending_repair"]
            .as_str()
            .unwrap()
            .contains("rejects permitted non-verified outcomes")
    );
    // The mock models the review signal; it does not establish live-model accuracy.
    assert!(
        f.command()
            .arg("check-agenda")
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(!f.root.join("repo/.git/opsx-build/acceptance").exists());
}

#[test]
fn saved_assignment_defects_resume_through_repair_and_rendering() {
    let f = Fixture::new();
    assert!(!f.run("repair").status.success());
    assert_eq!(f.calls(), 3);
    let result = f
        .command()
        .env("ASSIGNMENT_MODE", "repair-resume")
        .args([
            "--worker-command=claude",
            "--frontier-command=claude",
            "--apply-command=noop",
            "--verify-command=noop",
            "--archive-command=noop",
            "--resume",
            "--max-verify-retries=1",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("Fixture stops at Archive"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(f.calls(), 6);
    assert_eq!(f.state()["stage"], "archive");
    let repair = fs::read_to_string(f.root.join("prompt-4")).unwrap();
    assert!(repair.contains("Assignment repair inputs: protected inventory"));
    assert!(repair.contains("Do not change the inventory or acceptance kinds"));
    assert!(
        f.command()
            .arg("check-agenda")
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn successful_verify_cannot_leave_a_broken_assignment_for_a_delivery_slice() {
    let f = Fixture::new();
    f.run("valid");
    let mut state = f.state();
    state["bootstrap"] = false.into();
    state["stage"] = "verify".into();
    fs::write(
        f.root.join("repo/.git/opsx-build/last-run.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let result = f
        .command()
        .env("ASSIGNMENT_MODE", "verify-mutation")
        .args([
            "--worker-command=claude",
            "--apply-command=noop",
            "--verify-command=noop",
            "--archive-command=noop",
            "--resume",
            "--max-verify-retries=0",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(f.calls(), 6);
    assert_eq!(f.state()["stage"], "repair");
    assert!(
        f.state()["pending_repair"]
            .as_str()
            .unwrap()
            .contains("unassigned obligation")
    );
}

#[test]
fn legacy_checkpoint_accepts_a_maintainer_correction_without_switching_its_schema() {
    let f = Fixture::new();
    f.run("valid");
    let before = f.state();
    assert_eq!(before["contracts"]["compact"], false);
    let calls = f.calls();
    let path = f.root.join("repo/contract.md");
    fs::write(
        &path,
        format!(
            "{}\nMaintainer clarification.\n",
            fs::read_to_string(&path).unwrap()
        ),
    )
    .unwrap();
    let result = f.command().arg("accept-changes").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(f.calls(), calls);
    assert_eq!(f.state()["contracts"]["compact"], false);
    assert_eq!(f.state()["review"], before["review"]);
    let schema = fs::read_to_string(
        f.root
            .join("repo/openspec/schemas/opsx-supplied-contracts/schema.yaml"),
    )
    .unwrap();
    assert_eq!(
        schema,
        include_str!("../assets/supplied-contracts-legacy/schema.yaml")
    );
}

#[test]
fn inventory_is_required_only_for_new_supplied_contract_bootstrap() {
    let f = Fixture::new();
    let checkpoint = fs::read(f.root.join("repo/.git/opsx-build/last-run.json")).unwrap();
    let result = f
        .command()
        .args([
            "--supplied-contracts",
            "--contract=contract.md",
            "--worker-command=claude",
            "--frontier-command=claude",
            "bootstrap",
            "--context=context.md",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("requires --obligation-inventory"));
    assert_eq!(
        checkpoint,
        fs::read(f.root.join("repo/.git/opsx-build/last-run.json")).unwrap()
    );
    assert_eq!(f.calls(), 0);
}
