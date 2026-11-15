use super::*;

struct Repo(PathBuf);
impl Repo {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("opsx-request-progress-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        fs::write(root.join("code.rs"), "original").unwrap();
        Self(root)
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn touched_files_git_metadata_ignored_outputs_and_revisited_trees_do_not_renew_work() {
    let repo = Repo::new();
    let mut history = History::new(&repo.0, None);
    let grace = Duration::from_secs(10);
    fs::write(repo.0.join("code.rs"), "original").unwrap();
    fs::write(repo.0.join(".git/log-output"), "new log").unwrap();
    fs::create_dir(repo.0.join("build")).unwrap();
    fs::write(repo.0.join("build/output"), "new output").unwrap();
    history.sample(Duration::from_secs(1), grace, true);
    assert_eq!(history.allowance(Duration::from_secs(1), grace), None);
    fs::write(repo.0.join("code.rs"), "new implementation").unwrap();
    history.sample(Duration::from_secs(2), grace, true);
    assert_eq!(
        history.allowance(Duration::from_secs(2), grace),
        Some(Duration::from_secs(12))
    );
    for (second, text) in [(3, "original"), (4, "new implementation")] {
        fs::write(repo.0.join("code.rs"), text).unwrap();
        history.sample(Duration::from_secs(second), grace, true);
    }
    assert_eq!(history.allowance(Duration::from_secs(12), grace), None);
}

#[test]
fn product_repository_changes_are_observed_and_unavailable_inputs_are_not_new_work() {
    let repo = Repo::new();
    let product = Repo::new();
    let scope = repo.0.join(".git/scope");
    fs::create_dir(&scope).unwrap();
    fs::write(
        scope.join("context.json"),
        json!({"repositories":[repo.0,product.0]}).to_string(),
    )
    .unwrap();
    let mut history = History::new(&repo.0, Some(&scope));
    let grace = Duration::from_secs(10);
    fs::write(product.0.join("code.rs"), "actual product repair").unwrap();
    history.sample(Duration::from_secs(1), grace, true);
    assert_eq!(
        history.allowance(Duration::from_secs(1), grace),
        Some(Duration::from_secs(11))
    );
    let mut unknown = History::new(&repo.0.join("missing"), None);
    unknown.sample(Duration::from_secs(1), grace, true);
    assert_eq!(unknown.allowance(Duration::from_secs(1), grace), None);
    assert_eq!(
        unknown.state(Duration::from_secs(1), grace)["inputs_available"],
        false
    );
    let missing = repo.0.join("missing");
    fs::create_dir(&missing).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&missing)
            .status()
            .unwrap()
            .success()
    );
    fs::write(missing.join("code.rs"), "first available inputs").unwrap();
    unknown.sample(Duration::from_secs(2), grace, true);
    assert_eq!(unknown.allowance(Duration::from_secs(2), grace), None);
    fs::write(missing.join("code.rs"), "actual changed inputs").unwrap();
    unknown.sample(Duration::from_secs(3), grace, true);
    assert_eq!(
        unknown.allowance(Duration::from_secs(3), grace),
        Some(Duration::from_secs(13))
    );
}

#[test]
fn a_result_received_before_its_replayed_call_cannot_reopen_a_pending_check() {
    let repo = Repo::new();
    let mut history = History::new(&repo.0, None);
    let grace = Duration::from_secs(10);
    assert!(!history.observe(
        Duration::ZERO,
        grace,
        "tool_result",
        &json!({"call_id":"old","output":"done"})
    ));
    assert!(!history.observe(Duration::from_secs(1), grace, "tool_call", &json!({"call_id":"old","name":"Bash","input":{"command":"go test ./...","timeout":3_600_000}})));
    assert_eq!(history.allowance(Duration::from_secs(1), grace), None);
    assert!(
        history.state(Duration::from_secs(1), grace)["pending_checks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn unknown_check_deadlines_get_only_one_grace_window_from_start() {
    let repo = Repo::new();
    let mut history = History::new(&repo.0, None);
    let grace = Duration::from_secs(10);
    let call = json!({"call_id":"proof","name":"commandExecution","input":{"command":"java -cp tools.jar tlc2.TLC Spec"}});
    assert!(history.observe(Duration::from_secs(1), grace, "tool_call", &call));
    assert_eq!(
        history.allowance(Duration::from_secs(10), grace),
        Some(Duration::from_secs(11))
    );
    assert!(!history.observe(Duration::from_secs(10), grace, "tool_call", &call));
    assert_eq!(history.allowance(Duration::from_secs(11), grace), None);
    assert!(!history.expired(Duration::from_secs(11)));
}

#[test]
fn changed_execution_inputs_allow_a_repeat_but_fresh_ids_and_longer_waits_do_not() {
    let repo = Repo::new();
    let mut history = History::new(&repo.0, None);
    let grace = Duration::from_secs(10);
    let mut call =
        json!({"call_id":"one","name":"Bash","input":{"command":"go test ./...","timeout":1000}});
    history.observe(Duration::ZERO, grace, "tool_call", &call);
    history.observe(
        Duration::ZERO,
        grace,
        "tool_result",
        &json!({"call_id":"one","output":"FAIL"}),
    );
    call["call_id"] = json!("two");
    call["input"]["description"] = json!("new description");
    call["input"]["timeout"] = json!(3_600_000);
    history.observe(Duration::from_secs(11), grace, "tool_call", &call);
    assert_eq!(history.allowance(Duration::from_secs(11), grace), None);
    fs::write(repo.0.join("code.rs"), "fixed").unwrap();
    call["call_id"] = json!("three");
    history.observe(Duration::from_secs(12), grace, "tool_call", &call);
    assert_eq!(
        history.allowance(Duration::from_secs(12), grace),
        Some(Duration::from_secs(3612))
    );
}

#[test]
fn check_detection_covers_native_proof_and_cluster_checks_but_not_polling_or_helper_bypasses() {
    for command in [
        "go test ./...",
        "/usr/local/bin/go test ./...",
        "cargo check",
        "python3 checks/native_calibration.py",
        "python3 -m unittest",
        "java -cp tla2tools.jar tlc2.TLC Spec",
        "java -jar tla2tools.jar Spec.tla",
        "tlapm Spec.tla",
        "apalache-mc check Spec.tla",
        "kubectl -n fixture wait --for=condition=complete job/check --timeout=10m",
        "helm test release --timeout 5m",
    ] {
        assert!(
            check_command(&json!({"name":"Bash","input":{"command":command}})).is_some(),
            "{command}"
        );
    }
    for command in [
        "echo still working",
        "sleep 1000",
        "kubectl get pods",
        "kubectl logs pod",
        "echo 'go test ./...' | /tmp/opsx-build check --scope x",
        "echo 'go test ./...' | /tmp/opsx-build --no-config check --scope x",
    ] {
        assert!(
            check_command(&json!({"name":"Bash","input":{"command":command}})).is_none(),
            "{command}"
        );
    }
    assert!(check_command(&json!({"name":"Read","input":{"command":"go test"}})).is_none());
}
