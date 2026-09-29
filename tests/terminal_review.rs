//! Resume the terminal review at its remediation limit without a real model.
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
        let fixture = Self {
            root: std::env::temp_dir()
                .join(format!("opsx-terminal-review-{}", uuid::Uuid::new_v4())),
        };
        fs::create_dir_all(fixture.root.join("bin")).unwrap();
        fs::create_dir_all(fixture.root.join("repo/automation/slices")).unwrap();
        fs::create_dir_all(fixture.root.join("repo/openspec")).unwrap();
        fs::write(
            fixture.root.join("repo/openspec/config.yaml"),
            "schema: spec-driven\n",
        )
        .unwrap();
        fs::write(
            fixture
                .root
                .join("repo/automation/slices/9999-project-acceptance.md"),
            "# Project acceptance\n\n## Objective\n\nVerify the complete project.\n",
        )
        .unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "fixture@example.invalid"],
            vec!["config", "user.name", "Fixture"],
            vec!["add", "."],
            vec!["commit", "-qm", "Completed remediation"],
        ] {
            fixture.git(&args);
        }
        fs::write(
            fixture.root.join("bin/openspec"),
            r#"#!/bin/sh
case "$1" in
 --version) echo '1.13.2';;
 list) echo '{"changes":[]}';;
 *) exit 80;;
esac
"#,
        )
        .unwrap();
        fs::write(fixture.root.join("bin/claude"), r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'Claude fixture'; exit 0; fi
for arg in "$@"; do
 case "$arg" in /rename\ *) echo '{"type":"result","subtype":"success","result":"Renamed"}'; exit 0;; esac
 case "$arg" in stream-json) streamed=true;; esac
done
if [ "$streamed" = true ]; then IFS= read -r prompt; fi
printf 'call\n' >> "$TERMINAL_TEST_ROOT/calls"
count=$(wc -l < "$TERMINAL_TEST_ROOT/calls" | tr -d ' ')
printf '%s\n' "$@" "$prompt" > "$TERMINAL_TEST_ROOT/prompt-$count"
status=BLOCKED
summary='Fixture stopped at Propose'
case "$TERMINAL_TEST_MODE:$count" in
 ready:1) status=READY; summary='Project ready';;
 blocked:1) summary='REQ-01 remains unsatisfied; further remediation needs a maintainer decision';;
 replanned:1)
  printf '# Remediation\n## Objective\nComplete the remaining behaviour.\n## Prerequisites\nEarlier slices.\n## Acceptance Criteria\nRequired behaviour works.\n## Required Tests\nRun conformance checks.\n' > automation/slices/0009-unapproved.md
  printf '# Agenda\n0009: Remediation\n9999: Project acceptance\n' > automation/slices/README.md
  git add automation/slices/0009-unapproved.md automation/slices/README.md
  git commit -qm 'opsx: add acceptance remediation' || exit 90
  status=REPLANNED; summary='Attempted fourth remediation';;
esac
printf '{"type":"result","subtype":"success","session_id":"fixture-session","result":"%s","structured_output":{"opsx_status":"%s","summary":"%s"}}\n' "$summary" "$status" "$summary"
"#).unwrap();
        for name in ["openspec", "claude"] {
            fs::set_permissions(
                fixture.root.join("bin").join(name),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let initial = fixture.run("initial", false);
        assert!(
            String::from_utf8_lossy(&initial.stderr).contains("Fixture stopped at Propose"),
            "{}",
            String::from_utf8_lossy(&initial.stderr)
        );
        let mut state = fixture.state();
        state["terminal_remediations"] = 3.into();
        fixture.save_state(&state);
        fs::remove_file(fixture.root.join("calls")).unwrap();
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root.join("repo"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn run(&self, mode: &str, resume: bool) -> Output {
        self.command(mode, resume).output().unwrap()
    }

    fn command(&self, mode: &str, resume: bool) -> Command {
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
            .env("TERMINAL_TEST_ROOT", &self.root)
            .env("TERMINAL_TEST_MODE", mode)
            .args([
                "--no-config",
                "--no-campaign-config",
                "--worker-command=claude",
                "--worker-backend=claude",
                "--frontier-command=claude",
                "--frontier-backend=claude",
                "--apply-command=noop",
                "--verify-command=noop",
                "--archive-command=noop",
                if resume { "--resume" } else { "advance" },
            ]);
        command
    }

    fn state(&self) -> serde_json::Value {
        serde_json::from_slice(
            &fs::read(self.root.join("repo/.git/opsx-build/last-run.json")).unwrap(),
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
fn resume_reviews_readiness_after_the_last_remediation_round() {
    let fixture = Fixture::new();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    let status = fixture.git(&["status", "--porcelain"]);
    let output = fixture.run("ready", true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Frontier review confirmed readiness"),
        "{stderr}"
    );
    assert!(stderr.contains("Fixture stopped at Propose"), "{stderr}");
    assert_eq!(fixture.calls(), 2);
    assert_eq!(fixture.state()["terminal_remediations"], 3);
    assert_eq!(fixture.state()["terminal_review_complete"], true);
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.git(&["status", "--porcelain"]), status);
    let prompt = fs::read_to_string(fixture.root.join("prompt-1")).unwrap();
    assert!(prompt.contains("readiness review only"));
    assert!(!prompt.contains("Add one or more bounded"));
}

#[test]
fn remaining_gaps_are_reported_without_resetting_the_limit() {
    let fixture = Fixture::new();
    let before = fixture.state();
    let output = fixture.run("blocked", true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("REQ-01 remains unsatisfied"));
    assert_eq!(fixture.calls(), 1);
    assert_eq!(fixture.state(), before);
}

#[test]
fn an_attempted_fourth_remediation_is_rolled_back() {
    let fixture = Fixture::new();
    let before = fixture.state();
    let head = fixture.git(&["rev-parse", "HEAD"]);
    let status = fixture.git(&["status", "--porcelain"]);
    let output = fixture.run("replanned", true);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("further remediation was not accepted")
    );
    assert_eq!(fixture.calls(), 1);
    assert_eq!(fixture.state(), before);
    assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.git(&["status", "--porcelain"]), status);
    assert!(
        !fixture
            .root
            .join("repo/automation/slices/0009-unapproved.md")
            .exists()
    );
}

#[test]
fn a_failed_terminal_attempt_still_stops_at_the_remediation_limit() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    state["terminal_review_complete"] = true.into();
    state["stage"] = "verify".into();
    state["too_large"] = serde_json::json!({"stage": "verify", "summary": "Missing behaviour"});
    fixture.save_state(&state);
    let output = fixture.run("ready", true);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("limit of 3 frontier remediation rounds")
    );
    assert_eq!(fixture.calls(), 0);
    assert_eq!(fixture.state(), state);
}

#[test]
fn raising_the_limit_on_resume_allows_another_remediation_without_resetting_count() {
    for escalated in [false, true] {
        let fixture = Fixture::new();
        if escalated {
            let mut state = fixture.state();
            state["terminal_review_complete"] = true.into();
            state["stage"] = "verify".into();
            state["too_large"] =
                serde_json::json!({"stage": "verify", "summary": "Missing behaviour"});
            fixture.save_state(&state);
        }
        let output = fixture
            .command("replanned", true)
            .arg("--max-terminal-remediations=10")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Frontier inserted acceptance remediation"),
            "{stderr}"
        );
        assert_eq!(fixture.state()["terminal_remediations"], 4);
        assert_eq!(fixture.state()["change"], "0009-unapproved");
        assert_eq!(fixture.state()["terminal_review_complete"], false);
        assert_eq!(fixture.state()["too_large"], serde_json::Value::Null);
        assert_eq!(fixture.calls(), 2);
        let prompt = fs::read_to_string(fixture.root.join("prompt-1")).unwrap();
        assert!(prompt.contains("Add one or more bounded"));
        assert!(!prompt.contains("readiness review only"));
    }
}

#[test]
fn zero_remediation_allowance_still_permits_readiness_review() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    state["terminal_remediations"] = 0.into();
    fixture.save_state(&state);
    let output = fixture
        .command("replanned", true)
        .env("OPSX_BUILD_MAX_TERMINAL_REMEDIATIONS", "0")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("limit of 0 frontier remediation rounds")
    );
    assert_eq!(fixture.state(), state);
    fs::remove_file(fixture.root.join("calls")).unwrap();
    let output = fixture
        .command("ready", true)
        .env("OPSX_BUILD_MAX_TERMINAL_REMEDIATIONS", "7")
        .arg("--max-terminal-remediations=0")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Terminal remediation limit reached (0/0)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Frontier review confirmed readiness"),
        "{stderr}"
    );
    assert_eq!(fixture.state()["terminal_remediations"], 0);
}
