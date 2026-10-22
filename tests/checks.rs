//! Actual subprocess receipts and invalidation, without any model/provider.
#![cfg(unix)]
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    scope: uuid::Uuid,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("opsx-check-receipts-{}", uuid::Uuid::new_v4()));
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("build")).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        for (name, contents) in [
            (".gitignore", "build/\n"),
            ("shared.rs", "shared rule\n"),
            ("tests.rs", "required assertion\n"),
            ("dependency.lock", "original\n"),
            ("delivery.md", "claimed evidence\n"),
            ("contract.md", "original contract\n"),
            ("build/dependency", "fixed dependency\n"),
            ("build/control", "pass\n"),
        ] {
            fs::write(repo.join(name), contents).unwrap();
        }
        let scope = uuid::Uuid::new_v4();
        let fixture = Self { root, repo, scope };
        fs::create_dir_all(fixture.directory()).unwrap();
        fs::write(
            fixture.directory().join("context.json"),
            serde_json::to_vec(&json!({
                "change": "0001-delivery", "stage": "Apply", "repositories": [fixture.repo],
                "protected": [fixture.repo.join("contract.md")],
            }))
            .unwrap(),
        )
        .unwrap();
        fixture.write_tool("original");
        fixture
    }

    fn directory(&self) -> PathBuf {
        self.repo
            .join(".git/opsx-build/checks")
            .join(self.scope.to_string())
    }

    fn write_tool(&self, message: &str) {
        fs::write(self.root.join("tool"), format!(
            "#!/bin/sh\nprintf 'run\\n' >> '{}'\nif grep -q fail build/control; then printf 'real failure\\n' >&2; exit 7; fi\nprintf '{message}\\n'\n",
            self.root.join("executions").display(),
        )).unwrap();
        fs::set_permissions(self.root.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn spec(&self) -> Value {
        json!({
            "key": "unit", "command": format!("'{}'", self.root.join("tool").display()),
            "cwd": ".", "tools": [self.root.join("tool"), "/usr/bin/grep"],
            "additional_inputs": ["build/dependency", "build/control"], "review_only": [],
            "runtime": "stable", "runtime_reason": "Fixed deterministic fixture, all execution dependencies declared",
            "timeout_seconds": 10,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_opsx-build"));
        command
            .args(["check", "--repo"])
            .arg(&self.repo)
            .args(["--scope", &self.scope.to_string()])
            .env("CHECK_BOUNDARY", "one")
            .env("CHECK_SECRET", "secret-value-which-must-not-be-retained")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run_with(&self, spec: &Value, mutate: impl FnOnce(&mut Command)) -> Output {
        let mut command = self.command();
        mutate(&mut command);
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(spec).unwrap())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn run(&self, spec: &Value) -> Value {
        receipt(&self.run_with(spec, |_| {}))
    }

    fn count(&self) -> usize {
        fs::read_to_string(self.root.join("executions"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn record(receipt: &Value) -> Value {
        serde_json::from_slice(&fs::read(receipt["receipt"].as_str().unwrap()).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn receipt(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "receipt {error}; status {:?}; stdout {}; stderr {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

#[test]
fn actual_results_and_logs_survive_an_evidence_only_correction() {
    let f = Fixture::new();
    // This command does not load the delivery record. No file class is excluded.
    let mut spec = f.spec();
    spec["review_only"] = json!([{"path": f.repo.join("delivery.md"), "reason": "This command never reads the delivery evidence claim"}]);
    fs::write(
        f.repo.join("opsx-build.md"),
        "invalid campaign configuration; helper must not load this",
    )
    .unwrap();
    let first = f.run(&spec);
    assert_eq!(first["outcome"], "passed");
    assert_eq!(first["exit_code"], 0);
    assert_eq!(first["executed"], true);
    assert!(
        first["receipt"]
            .as_str()
            .unwrap()
            .starts_with(f.directory().to_str().unwrap())
    );
    assert_eq!(
        fs::read_to_string(first["stdout"].as_str().unwrap()).unwrap(),
        "original\n"
    );
    assert_eq!(
        fs::read_to_string(first["stderr"].as_str().unwrap()).unwrap(),
        ""
    );
    let recorded = Fixture::record(&first);
    assert_eq!(recorded["stage"], "Apply");
    assert!(recorded["inputs"]["environment"].is_string());
    assert!(
        !recorded
            .to_string()
            .contains("secret-value-which-must-not-be-retained")
    );
    fs::write(f.repo.join("delivery.md"), "corrected evidence claim\n").unwrap();
    let second = f.run(&spec);
    assert_eq!(second["executed"], false);
    assert_eq!(second["reused_from"], first["receipt"]);
    assert_eq!(second["stdout"], first["stdout"]);
    assert_eq!(f.count(), 1);
    assert_eq!(Fixture::record(&second)["sequence"], 2);
}

#[test]
fn shared_code_tests_configuration_and_unclassified_docs_all_invalidate() {
    let f = Fixture::new();
    let spec = f.spec();
    f.run(&spec);
    for (index, path) in [
        "shared.rs",
        "tests.rs",
        "dependency.lock",
        "delivery.md",
        "new-helper.rs",
    ]
    .iter()
    .enumerate()
    {
        fs::write(f.repo.join(path), "changed\n").unwrap();
        let result = f.run(&spec);
        assert_eq!(result["executed"], true);
        assert!(result["invalidation_reasons"].to_string().contains(path));
        assert_eq!(f.count(), index + 2);
    }
    fs::remove_file(f.repo.join("new-helper.rs")).unwrap();
    assert_eq!(f.run(&spec)["executed"], true);
}

#[test]
fn tools_ignored_dependencies_and_environment_are_execution_inputs() {
    let f = Fixture::new();
    let spec = f.spec();
    f.run(&spec);
    assert_eq!(f.run(&spec)["executed"], false);
    f.write_tool("changed tool");
    assert!(
        f.run(&spec)["invalidation_reasons"]
            .to_string()
            .contains("tool identities changed")
    );
    fs::write(
        f.repo.join("build/dependency"),
        "changed ignored dependency\n",
    )
    .unwrap();
    assert!(
        f.run(&spec)["invalidation_reasons"]
            .to_string()
            .contains("dependency/fixture inputs changed")
    );
    let changed = receipt(&f.run_with(&spec, |command| {
        command.env("CHECK_BOUNDARY", "two");
    }));
    assert!(
        changed["invalidation_reasons"]
            .to_string()
            .contains("environment changed")
    );
    assert_eq!(f.count(), 4);
}

#[test]
fn additional_directory_additions_and_missing_dependencies_prevent_reuse() {
    let f = Fixture::new();
    let mut spec = f.spec();
    spec["additional_inputs"] = json!(["build"]);
    f.run(&spec);
    fs::write(f.repo.join("build/new-fixture"), "new fixture\n").unwrap();
    assert_eq!(f.run(&spec)["executed"], true);
    spec["additional_inputs"] = json!(["build/missing-fixture"]);
    let missing = f.run(&spec);
    assert_eq!(missing["outcome"], "passed"); // An unavailable identity is not a fabricated process failure.
    assert!(
        missing["reuse_limitations"]
            .to_string()
            .contains("inputs unavailable")
    );
    assert_eq!(f.run(&spec)["executed"], true);
    assert_eq!(f.count(), 4);
}

#[test]
fn unknown_live_and_undeclared_tool_premises_require_execution() {
    let f = Fixture::new();
    for runtime in ["unknown", "live"] {
        let mut spec = f.spec();
        spec["runtime"] = runtime.into();
        f.run(&spec);
        let again = f.run(&spec);
        assert_eq!(again["executed"], true);
        assert!(
            again["reuse_limitations"]
                .to_string()
                .contains("runtime premise")
        );
    }
    let mut spec = f.spec();
    spec["tools"] = json!([]);
    f.run(&spec);
    assert_eq!(f.run(&spec)["executed"], true);
    assert_eq!(f.count(), 6);
}

#[test]
fn latest_failure_is_not_hidden_by_an_older_matching_pass() {
    let f = Fixture::new();
    let spec = f.spec();
    f.run(&spec);
    fs::write(f.repo.join("build/control"), "fail\n").unwrap();
    let failed = f.run_with(&spec, |_| {});
    assert_eq!(failed.status.code(), Some(7));
    let failed = receipt(&failed);
    assert_eq!(failed["outcome"], "failed");
    assert_eq!(failed["exit_code"], 7);
    assert!(
        fs::read_to_string(failed["stderr"].as_str().unwrap())
            .unwrap()
            .contains("real failure")
    );
    // Returning every input to the first passing state still requires execution.
    fs::write(f.repo.join("build/control"), "pass\n").unwrap();
    let recovered = f.run(&spec);
    assert_eq!(recovered["executed"], true);
    assert!(
        recovered["invalidation_reasons"]
            .to_string()
            .contains("latest attempt was failed")
    );
    assert_eq!(f.count(), 3);
}

#[test]
fn missing_or_modified_logs_and_input_mutation_cannot_supply_cached_passes() {
    let f = Fixture::new();
    let mut spec = f.spec();
    let first = f.run(&spec);
    fs::write(first["stdout"].as_str().unwrap(), "forged pass\n").unwrap();
    let second = f.run(&spec);
    assert!(
        second["invalidation_reasons"]
            .to_string()
            .contains("logs are missing or changed")
    );
    fs::remove_file(second["stderr"].as_str().unwrap()).unwrap();
    assert_eq!(f.run(&spec)["executed"], true);
    spec["command"] = format!(
        "{}; printf 'mutated\\n' >> shared.rs",
        spec["command"].as_str().unwrap()
    )
    .into();
    let mutating = f.run(&spec);
    assert_eq!(mutating["outcome"], "passed");
    assert!(
        mutating["reuse_limitations"]
            .to_string()
            .contains("inputs changed during the check")
    );
    assert_eq!(f.run(&spec)["executed"], true);
    assert_eq!(f.count(), 5);
}

#[test]
fn protected_inputs_and_directory_exclusions_are_rejected_before_execution() {
    let f = Fixture::new();
    for path in [f.repo.join("contract.md"), f.repo.join("build")] {
        let mut spec = f.spec();
        spec["review_only"] = json!([{"path": path, "reason": "attempted exclusion"}]);
        let result = f.run_with(&spec, |_| {});
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(
            if path.ends_with("contract.md") {
                "protected input cannot be excluded"
            } else {
                "individual files"
            }
        ));
    }
    assert_eq!(f.count(), 0);
}

#[test]
fn both_sidecar_roots_are_inputs_and_new_runs_do_not_inherit_cached_results() {
    let mut f = Fixture::new();
    let product = f.root.join("product");
    fs::create_dir(&product).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&product)
            .status()
            .unwrap()
            .success()
    );
    fs::write(product.join("source.rs"), "original\n").unwrap();
    let context_path = f.directory().join("context.json");
    let mut context: Value = serde_json::from_slice(&fs::read(&context_path).unwrap()).unwrap();
    context["repositories"] = json!([f.repo, product]);
    fs::write(&context_path, serde_json::to_vec(&context).unwrap()).unwrap();
    let spec = f.spec();
    f.run(&spec);
    fs::write(product.join("source.rs"), "changed product\n").unwrap();
    assert!(
        f.run(&spec)["invalidation_reasons"]
            .to_string()
            .contains("source.rs")
    );
    fs::write(f.repo.join("shared.rs"), "changed planning source\n").unwrap();
    assert_eq!(f.run(&spec)["executed"], true);
    f.scope = uuid::Uuid::new_v4();
    fs::create_dir_all(f.directory()).unwrap();
    fs::write(
        f.directory().join("context.json"),
        serde_json::to_vec(&context).unwrap(),
    )
    .unwrap();
    assert_eq!(f.run(&spec)["executed"], true);
    assert_eq!(f.count(), 4);
}

#[test]
fn timeout_and_interrupt_retain_logs_without_claiming_a_process_exit() {
    for interrupt in [false, true] {
        let f = Fixture::new();
        let mut spec = f.spec();
        spec["command"] = "printf 'started\\n'; sleep 30".into();
        spec["timeout_seconds"] = if interrupt { 10.into() } else { 1.into() };
        let mut child = f.command().spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&spec).unwrap())
            .unwrap();
        if interrupt {
            let started = Instant::now();
            loop {
                let ready = fs::read_dir(f.directory())
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        fs::read(entry.path().join("stdout.log"))
                            .is_ok_and(|bytes| !bytes.is_empty())
                    });
                if ready {
                    break;
                }
                assert!(started.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(10));
            }
            // SAFETY: child is the helper started by this test and still running.
            assert_eq!(
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
                0
            );
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(if interrupt { 143 } else { 124 })
        );
        let result = receipt(&output);
        assert_eq!(
            result["outcome"],
            if interrupt {
                "interrupted"
            } else {
                "timed out"
            }
        );
        assert_eq!(result["exit_code"], Value::Null);
        assert!(
            fs::read_to_string(result["stdout"].as_str().unwrap())
                .unwrap()
                .contains("started")
        );
        assert_eq!(Fixture::record(&result)["outcome"], result["outcome"]);
    }
}

#[test]
fn concurrent_identical_checks_are_serialized_and_reuse_the_completed_result() {
    let f = Fixture::new();
    let mut spec = f.spec();
    spec["command"] = format!("sleep 0.2; {}", spec["command"].as_str().unwrap()).into();
    let mut children = Vec::new();
    for _ in 0..2 {
        let mut child = f.command().spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&spec).unwrap())
            .unwrap();
        children.push(child);
    }
    let mut results = children
        .into_iter()
        .map(|child| receipt(&child.wait_with_output().unwrap()))
        .collect::<Vec<_>>();
    results.sort_by_key(|result| !result["executed"].as_bool().unwrap());
    assert_eq!(results[0]["executed"], true);
    assert_eq!(results[1]["executed"], false);
    assert_eq!(f.count(), 1);
    assert_eq!(Fixture::record(&results[1])["sequence"], 2);
}

#[test]
fn incomplete_or_corrupt_receipt_history_forces_execution_without_using_an_older_pass() {
    let f = Fixture::new();
    let spec = f.spec();
    let first = f.run(&spec);
    fs::write(
        first["receipt"].as_str().unwrap(),
        "interrupted/corrupt record",
    )
    .unwrap();
    let second = f.run(&spec);
    assert_eq!(second["executed"], true);
    assert!(
        second["invalidation_reasons"]
            .to_string()
            .contains("history incomplete/unreadable")
    );
    fs::remove_file(first["receipt"].as_str().unwrap()).unwrap();
    assert_eq!(f.run(&spec)["executed"], true);
    assert_eq!(f.count(), 3);
}

#[test]
fn a_later_failed_independent_acceptance_invalidates_unchanged_helper_passes() {
    let f = Fixture::new();
    let spec = f.spec();
    f.run(&spec);
    let path = f.directory().join("context.json");
    let mut context: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    context["generation"] = 1.into();
    fs::write(path, serde_json::to_vec(&context).unwrap()).unwrap();
    let result = f.run(&spec);
    assert_eq!(result["executed"], true);
    assert!(
        result["invalidation_reasons"]
            .to_string()
            .contains("independent acceptance failed or was interrupted")
    );
    assert_eq!(f.count(), 2);
}

#[test]
fn command_working_directory_and_timeout_changes_require_execution() {
    let f = Fixture::new();
    let mut spec = f.spec();
    spec["command"] = "printf 'result\\n'".into();
    spec["tools"] = json!(["/bin/sh"]);
    spec["additional_inputs"] = json!([]);
    f.run(&spec);
    for field in ["command", "cwd", "timeout_seconds"] {
        spec[field] = match field {
            "command" => "printf 'different result\\n'".into(),
            "cwd" => "build".into(),
            _ => 9.into(),
        };
        let result = f.run(&spec);
        assert_eq!(result["executed"], true);
        assert!(
            result["invalidation_reasons"]
                .to_string()
                .contains("command, cwd, timeout")
        );
    }
}
