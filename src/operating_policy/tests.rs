use super::*;
use crate::usage::{Attempt, Identity, tests::RecordingUi};

struct Fixture {
    root: PathBuf,
    run: Run,
}
impl Fixture {
    fn new(policy: Policy) -> Self {
        let root = std::env::temp_dir().join(format!("opsx-policy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let run = Run::new(&root, policy, &RecordingUi::default()).unwrap();
        Self { root, run }
    }
    fn report(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.run.directory.join("stop.json")).unwrap()).unwrap()
    }
    fn ledger(&self) -> Ledger {
        serde_json::from_slice(&fs::read(self.run.directory.join("ledger.json")).unwrap()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn request(root: &Path, activity: &str) -> UsageRecord {
    let ui = RecordingUi::default();
    let mut usage = Attempt::new(
        &ui,
        root,
        Identity {
            backend: "fixture",
            connection: None,
            model: None,
            session: "owned-session",
            invocation: "test",
            attempt: 1,
            fresh: true,
        },
        activity,
    )
    .record
    .clone();
    usage.context.stage = Some("Verify".into());
    usage.context.change = Some("delivery".into());
    usage.outcome = "retry".into();
    usage
}

fn age_request(guard: &Guard, seconds: u64) {
    guard
        .active_request
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .started = Instant::now() - Duration::from_secs(seconds);
}

fn check_started(guard: &Guard, usage: &UsageRecord, id: &str, timeout_ms: u64) {
    guard.observe_agent(
        usage,
        "tool_call",
        serde_json::json!({"call_id":id,"name":"Bash",
        "input":{"command":"go test ./conformance/copyscenario", "timeout":timeout_ms}}),
    );
}

#[test]
fn pending_bounded_check_outlives_the_hour_and_expires_at_its_own_deadline() {
    for stage in ["Apply", "Repair"] {
        let f = Fixture::new(Policy::default());
        let guard = f.run.open(None).unwrap();
        let mut usage = request(&f.root, "long native calibration");
        usage.context.stage = Some(stage.into());
        guard.before_request(&usage).unwrap();
        age_request(&guard, 3458);
        check_started(&guard, &usage, "native", 3_500_000);
        age_request(&guard, 3601);
        guard.poll().unwrap();
        assert!(guard.remaining_request_time() > Duration::from_secs(3300));
        let ledger = f.ledger();
        assert_eq!(
            ledger.requests[&usage.id].watchdog.as_ref().unwrap()["pending_checks"][0]["call_id"],
            "native"
        );
        assert_eq!(ledger.no_progress, 0);
        assert_eq!(ledger.stage_requests, 1);
        // Several quiet checkpoints cannot forget the still-pending check.
        age_request(&guard, 5000);
        guard.poll().unwrap();
        // A fresh file edit cannot keep an overdue check alive.
        fs::write(f.root.join("implementation.go"), "new code").unwrap();
        age_request(&guard, 6959);
        assert!(is_stop(&guard.poll().unwrap_err()));
        assert!(
            f.report()["reason"]
                .as_str()
                .unwrap()
                .contains("declared wait deadline")
        );
    }
}

#[test]
fn frontier_worker_allows_a_quiet_check_to_run_for_half_an_hour() {
    for frontier_worker in [false, true] {
        let f = Fixture::new(Policy::load(None, frontier_worker).unwrap());
        let guard = f.run.open(None).unwrap();
        let mut usage = request(&f.root, "quiet gate");
        usage.context.stage = Some("Apply".into());
        guard.before_request(&usage).unwrap();
        age_request(&guard, 2600);
        guard.observe_agent(
            &usage,
            "tool_call",
            serde_json::json!({
                "call_id":"gate", "name":"Bash", "input":{"command":"go test ./..."}
            }),
        );
        // First reassessment, just over 16 minutes into the gate.
        age_request(&guard, 3601);
        if frontier_worker {
            guard.poll().unwrap();
            age_request(&guard, 4401);
        }
        assert!(is_stop(&guard.poll().unwrap_err()));
    }
}

#[test]
fn completed_check_releases_its_long_wait_and_recent_result_grace_expires() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    let mut usage = request(&f.root, "native check");
    usage.context.stage = Some("Apply".into());
    guard.before_request(&usage).unwrap();
    age_request(&guard, 3500);
    check_started(&guard, &usage, "check", 3_600_000);
    age_request(&guard, 3601);
    guard.poll().unwrap();
    guard.observe_agent(
        &usage,
        "tool_result",
        serde_json::json!({"call_id":"check","failed":false,"exit_code":null,"output":"FAIL"}),
    );
    // Transport success is not promoted to a passing test verdict.
    assert!(guard.remaining_request_time() <= Duration::from_secs(901));
    guard.poll().unwrap();
    assert!(
        f.ledger().requests[&usage.id].watchdog.as_ref().unwrap()["pending_checks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    age_request(&guard, 4502);
    assert!(is_stop(&guard.poll().unwrap_err()));
}

#[test]
fn replayed_results_and_unchanged_command_retries_cannot_renew_progress() {
    let f = Fixture::new(Policy {
        max_request_seconds: 1,
        request_progress_grace_seconds: Some(1),
        ..Policy::default()
    });
    let guard = f.run.open(None).unwrap();
    let mut usage = request(&f.root, "repeated test");
    usage.context.stage = Some("Repair".into());
    guard.before_request(&usage).unwrap();
    check_started(&guard, &usage, "one", 5000);
    let result = serde_json::json!({"call_id":"one","failed":true,"exit_code":1,"output":"FAIL"});
    guard.observe_agent(&usage, "tool_result", result.clone());
    age_request(&guard, 2);
    guard.observe_agent(&usage, "tool_result", result);
    // A new ID, new description and a larger timeout do not change inputs.
    guard.observe_agent(&usage, "tool_call", serde_json::json!({"call_id":"two","name":"Bash",
        "input":{"command":"go test ./conformance/copyscenario","description":"this time it will work","timeout":3_600_000}}));
    assert!(is_stop(&guard.poll().unwrap_err()));
    assert!(
        f.ledger().requests[&usage.id]
            .progress_extensions
            .is_empty()
    );
}

#[test]
fn fresh_source_changes_earn_grace_even_without_corpus_and_unchanged_files_do_not_renew_it() {
    for unavailable in [false, true] {
        let f = Fixture::new(Policy {
            max_request_seconds: 1,
            request_progress_grace_seconds: Some(10),
            ..Policy::default()
        });
        fs::write(f.root.join("code.go"), "original").unwrap();
        if unavailable {
            fs::write(f.root.join(".git/opsx-build/corpus"), "unavailable").unwrap();
        }
        let guard = f.run.open(None).unwrap();
        let mut usage = request(&f.root, "implementation");
        usage.context.stage = Some("Apply".into());
        guard.before_request(&usage).unwrap();
        fs::write(f.root.join("code.go"), "actual changed source").unwrap();
        age_request(&guard, 2);
        guard.poll().unwrap();
        assert!(guard.remaining_request_time() > Duration::from_secs(9));
        age_request(&guard, 13);
        // Same bytes with a new mtime are not new work.
        fs::write(f.root.join("code.go"), "actual changed source").unwrap();
        assert!(is_stop(&guard.poll().unwrap_err()));
    }
}

#[test]
fn narration_reads_and_unbounded_silence_do_not_earn_reassessment_time() {
    let f = Fixture::new(Policy {
        max_request_seconds: 1,
        ..Policy::default()
    });
    let guard = f.run.open(None).unwrap();
    let mut usage = request(&f.root, "busy but unchanged");
    usage.context.stage = Some("Apply".into());
    guard.before_request(&usage).unwrap();
    guard.observe_agent(
        &usage,
        "agent_text",
        serde_json::json!({"text":"Making great progress"}),
    );
    guard.observe_agent(
        &usage,
        "tool_call",
        serde_json::json!({"call_id":"read","name":"Read","input":{"file_path":"code.go"}}),
    );
    age_request(&guard, 2);
    assert!(is_stop(&guard.poll().unwrap_err()));
}

#[test]
fn implementation_reassessment_cannot_override_other_stages_opt_out_or_active_time() {
    for stage in [
        "Verify",
        "Propose",
        "Explore",
        "Archive",
        "Completion commit",
        "Apply",
        "Repair",
    ] {
        let f = Fixture::new(Policy {
            max_request_seconds: 1,
            request_progress_grace_seconds: if matches!(stage, "Apply" | "Repair") {
                None
            } else {
                Some(10)
            },
            ..Policy::default()
        });
        let guard = f.run.open(None).unwrap();
        let mut usage = request(&f.root, "check");
        usage.context.stage = Some(stage.into());
        guard.before_request(&usage).unwrap();
        check_started(&guard, &usage, "check", 3_600_000);
        fs::write(f.root.join("code.go"), "changed").unwrap();
        age_request(&guard, 2);
        assert!(is_stop(&guard.poll().unwrap_err()), "{stage}");
    }
    let f = Fixture::new(Policy {
        max_active_seconds: Some(1),
        ..Policy::default()
    });
    let mut guard = f.run.open(None).unwrap();
    let mut usage = request(&f.root, "check");
    usage.context.stage = Some("Apply".into());
    guard.before_request(&usage).unwrap();
    check_started(&guard, &usage, "check", 3_600_000);
    guard.active_since = Instant::now() - Duration::from_secs(2);
    assert!(is_stop(&guard.poll().unwrap_err()));
    assert_eq!(f.report()["trigger"], "active_time_limit");
}

#[test]
fn fresh_implementation_progress_does_not_reset_independent_review_safeguards() {
    let f = Fixture::new(Policy {
        max_request_seconds: 1,
        ..Policy::default()
    });
    let guard = f.run.open(None).unwrap();
    guard.review(review(1, &[], &[])).unwrap();
    guard.review(review(2, &[], &[])).unwrap();
    assert_eq!(f.ledger().no_progress, 1);
    let mut usage = request(&f.root, "repair");
    usage.context.stage = Some("Repair".into());
    guard.before_request(&usage).unwrap();
    fs::write(f.root.join("code.go"), "candidate repair").unwrap();
    age_request(&guard, 2);
    guard.poll().unwrap();
    assert_eq!(f.ledger().no_progress, 1);
    guard.record_usage(&usage);
    guard.review(review(3, &[], &[])).unwrap();
    assert!(is_stop(&guard.poll().unwrap_err()));
    assert_eq!(f.report()["trigger"], "no_verified_progress");
}

#[test]
fn control_requests_keep_their_deadline_even_inside_an_implementation_stage() {
    for stage in ["Apply", "Repair"] {
        let f = Fixture::new(Policy {
            max_request_seconds: 1,
            ..Policy::default()
        });
        let guard = f.run.open(None).unwrap();
        let mut usage = request(&f.root, "Compacting session");
        usage.context.stage = Some(stage.into());
        guard.before_control_request(&usage).unwrap();
        check_started(&guard, &usage, "check", 3_600_000);
        fs::write(f.root.join("code.go"), "changed by some other activity").unwrap();
        age_request(&guard, 2);
        assert!(is_stop(&guard.poll().unwrap_err()));
        assert!(
            f.ledger().requests[&usage.id]
                .progress_extensions
                .is_empty()
        );
    }
}

#[test]
fn all_backends_feed_pending_check_state_and_completion_into_reassessment() {
    use serde_json::json;
    for backend in ["claude", "codex", "opencode"] {
        let f = Fixture::new(Policy {
            max_request_seconds: 1,
            request_progress_grace_seconds: Some(10),
            ..Policy::default()
        });
        let ui = RecordingUi::default();
        *ui.stage_override.lock().unwrap() = Some("Apply".into());
        let guard = std::sync::Arc::new(f.run.open(None).unwrap());
        ui.set_operating_policy(guard.clone()).unwrap();
        let mut usage = Attempt::new(
            &ui,
            &f.root,
            Identity {
                backend,
                connection: None,
                model: None,
                session: "session",
                invocation: "invocation",
                attempt: 1,
                fresh: true,
            },
            "Apply",
        );
        usage
            .before_request_with_prompt("Finish the implementation")
            .unwrap();
        let (started, completed) = match backend {
            "claude" => (
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"check","name":"Bash","input":{"command":"go test ./native","timeout":60_000}}]}}),
                json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"check","content":"ok","is_error":false}]}}),
            ),
            "codex" => (
                json!({"method":"item/started","params":{"item":{"type":"commandExecution","id":"check","command":"go test ./native","cwd":"."}}}),
                json!({"method":"item/completed","params":{"item":{"type":"commandExecution","id":"check","command":"go test ./native","cwd":".","aggregatedOutput":"ok","exitCode":0}}}),
            ),
            _ => (
                json!({"type":"tool_use","part":{"callID":"check","tool":"bash","state":{"status":"running","input":{"command":"go test ./native","timeout":60_000}}}}),
                json!({"type":"tool_use","part":{"callID":"check","tool":"bash","state":{"status":"completed","input":{"command":"go test ./native","timeout":60_000},"output":"ok","metadata":{"exit":0}}}}),
            ),
        };
        usage.event(&started);
        age_request(&guard, 2);
        guard.poll().unwrap();
        assert_eq!(
            f.ledger().requests[&usage.record.id]
                .watchdog
                .as_ref()
                .unwrap()["pending_checks"][0]["call_id"],
            "check",
            "{backend}"
        );
        usage.event(&completed);
        guard.poll().unwrap();
        assert!(
            f.ledger().requests[&usage.record.id]
                .watchdog
                .as_ref()
                .unwrap()["pending_checks"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{backend}"
        );
        usage.finish("ready");
    }
}

fn review(number: u32, closed: &[&str], reopened: &[&str]) -> Progress {
    Progress {
        structured: true,
        id: format!("round-{number}"),
        evidence_path: format!("round-{number}.json").into(),
        change: "delivery".into(),
        verdict: "Retry".into(),
        open: vec!["F0001".into()],
        blocking_details: Vec::new(),
        closed: closed.iter().map(|id| (*id).into()).collect(),
        reopened: reopened.iter().map(|id| (*id).into()).collect(),
        same_input_snapshots: Some(true),
        summary: "Independent review evidence".into(),
        unfinished_areas: Vec::new(),
    }
}

#[test]
fn progress_grace_defaults_on_and_an_explicit_opt_out_survives_resume() {
    let default: Policy = serde_json::from_str("{}").unwrap();
    assert_eq!(default.request_progress_grace_seconds, Some(900));
    let disabled: Policy =
        serde_json::from_str(r#"{"request_progress_grace_seconds":null}"#).unwrap();
    let f = Fixture::new(disabled);
    let guard = f.run.open(None).unwrap();
    assert_eq!(f.ledger().policy.request_progress_grace_seconds, None);
    drop(guard);
}

#[test]
fn explicit_progress_grace_overrides_the_frontier_default_including_null() {
    let f = Fixture::new(Policy::default());
    let path = f.root.join("policy.json");
    for (contents, expected) in [
        (r#"{"max_request_seconds":120}"#, Some(1800)),
        (r#"{"request_progress_grace_seconds":900}"#, Some(900)),
        (r#"{"request_progress_grace_seconds":72}"#, Some(72)),
        (r#"{"request_progress_grace_seconds":null}"#, None),
    ] {
        fs::write(&path, contents).unwrap();
        let policy = Policy::load(Some(&path), true).unwrap();
        assert_eq!(policy.request_progress_grace_seconds, expected);
    }
}

#[cfg(unix)]
#[test]
fn preflight_budget_cancels_the_reviewer_and_expires_with_its_scope() {
    use crate::process::{CommandSpec, ProcessRunner};
    use std::sync::Arc;

    let fixture = Fixture::new(Policy::default());
    let ui = RecordingUi::default();
    let guard = Arc::new(fixture.run.open(None).unwrap());
    ui.set_operating_policy(guard.clone()).unwrap();
    let command = CommandSpec::new("sh", &fixture.root)
        .args(["-c", "sleep 10"])
        .policy_request();
    let budget = Duration::from_millis(100);
    let started = Instant::now();

    let result = guard.with_preflight_budget(budget, || {
        assert!(guard.remaining_request_time() <= budget);
        ProcessRunner::new(&ui).run(&command, "Checking the contract")
    });

    assert!(is_stop(&result.unwrap_err()));
    assert_eq!(fixture.report()["trigger"], "preflight_timeout");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(guard.remaining_request_time() > budget);
    guard.poll().unwrap();
}

#[test]
fn maintainer_correction_replenishes_local_allowances_without_erasing_history() {
    let f = Fixture::new(Policy {
        max_requests_per_stage: 1,
        ..Policy::default()
    });
    let guard = f.run.open(None).unwrap();
    let request = request(&f.root, "completed request");
    guard.before_request(&request).unwrap();
    guard.record_usage(&request);
    guard.review(review(1, &[], &[])).unwrap();
    guard.review(review(2, &[], &[])).unwrap();
    guard.review(review(3, &[], &[])).unwrap();
    assert!(guard.check(true).is_err());
    let before = f.ledger();
    guard.accept_changes().unwrap();
    guard.check(true).unwrap();
    let after = f.ledger();
    assert_eq!(after.requests.len(), before.requests.len());
    assert_eq!(after.reviews.len(), before.reviews.len());
    assert_eq!(after.policy, before.policy);
    assert!(after.active_ms >= before.active_ms);
    assert_eq!(after.stage_requests, 0);
    assert_eq!(after.no_progress, 0);
}

#[test]
fn a_reopened_defect_remains_blocking_despite_other_closures_or_changed_inputs() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    guard.review(review(1, &[], &[])).unwrap();
    guard.review(review(2, &["F0001"], &[])).unwrap();
    guard.review(review(3, &["F0002"], &["F0001"])).unwrap();
    guard.boundary(Some("delivery"), "Repair").unwrap();
    guard.review(review(4, &["F0001"], &[])).unwrap();
    let mut again = review(5, &["F0003"], &["F0001"]);
    again.same_input_snapshots = Some(false); // Changed dependencies confer no immunity.
    guard.review(again).unwrap();
    assert!(is_stop(
        &guard.boundary(Some("delivery"), "Repair").unwrap_err()
    ));
    assert_eq!(f.report()["trigger"], "recurring_findings");
    assert_eq!(f.report()["trend"]["no_progress_reviews"], 0);
    assert_eq!(f.report()["trend"]["reopened_finding_rounds"]["F0001"], 2);
    assert_eq!(f.report()["reviews"].as_array().unwrap().len(), 5);
}

#[test]
fn unknown_review_coverage_is_not_manufactured_into_a_no_progress_classification() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    for number in 1..=3 {
        let mut observation = review(number, &[], &[]);
        observation.structured = false;
        observation.same_input_snapshots = None;
        guard.review(observation).unwrap();
        if number < 3 {
            guard.boundary(Some("delivery"), "Repair").unwrap();
        }
    }
    assert_eq!(f.ledger().no_progress, 0);
    assert!(is_stop(
        &guard.boundary(Some("delivery"), "Repair").unwrap_err()
    ));
    assert_eq!(f.report()["trigger"], "review_evidence_gap");
    assert_eq!(f.report()["trend"]["no_progress_reviews"], 0);
    assert_eq!(f.report()["trend"]["unassessable_review_transitions"], 2);
}

#[test]
fn a_gap_in_review_evidence_breaks_the_consecutive_no_progress_comparison() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    for number in 1..=5 {
        let mut observation = review(number, &[], &[]);
        if number == 3 {
            observation.structured = false;
        }
        guard.review(observation).unwrap();
        guard.boundary(Some("delivery"), "Repair").unwrap();
    }
    assert_eq!(f.ledger().no_progress, 1);
}

#[test]
fn unfinished_review_does_not_consume_repair_progress_and_keeps_partial_closures_on_resume() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    guard.review(review(1, &[], &[])).unwrap();
    let mut partial = review(2, &["F0001"], &[]);
    partial.verdict = "Incomplete".into();
    guard.incomplete_review(partial).unwrap();
    let mut pending = review(3, &[], &[]);
    pending.verdict = "Incomplete".into();
    guard.incomplete_review(pending).unwrap();
    assert_eq!(f.ledger().reviews.len(), 1);
    assert_eq!(f.ledger().no_progress, 0);
    assert_eq!(f.ledger().unassessable, 0);
    drop(guard);
    let guard = f.run.open(None).unwrap();
    guard.review(review(4, &[], &[])).unwrap();
    guard.boundary(Some("delivery"), "Repair").unwrap();
    let ledger = f.ledger();
    assert_eq!(ledger.no_progress, 0);
    assert_eq!(ledger.reviews[1].closed, ["F0001"]);
    assert!(ledger.incomplete_review.is_none());
}

#[test]
fn repeated_reopening_in_partial_reviews_still_triggers_the_existing_guard() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    guard.boundary(Some("delivery"), "Verify").unwrap();
    guard.review(review(1, &[], &[])).unwrap();
    for number in 2..=3 {
        let mut partial = review(number, &[], &["F0001"]);
        partial.verdict = "Incomplete".into();
        guard.incomplete_review(partial.clone()).unwrap();
        guard.incomplete_review(partial).unwrap(); // Recording the same evidence is idempotent.
    }
    assert!(is_stop(
        &guard.boundary(Some("delivery"), "Verify").unwrap_err()
    ));
    assert_eq!(f.report()["trigger"], "recurring_findings");
    assert_eq!(f.report()["incomplete_review"]["id"], "round-3");
}

#[test]
fn completed_summary_clock_is_preserved_across_resume() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    guard.boundary(Some("delivery"), "Propose").unwrap();
    guard.boundary(Some("delivery"), "Complete").unwrap();
    let finished = guard.summary_context(true).unwrap();
    drop(guard);
    let resumed = f.run.open(None).unwrap();
    let repeated = resumed.summary_context(true).unwrap();
    assert_eq!(
        finished.timing.started_at_unix_ms,
        repeated.timing.started_at_unix_ms
    );
    assert_eq!(
        finished.timing.finished_at_unix_ms,
        repeated.timing.finished_at_unix_ms
    );
    assert_eq!(
        finished.timing.finished_active_ms,
        repeated.timing.finished_active_ms
    );
    assert_eq!(resumed.change_elapsed_seconds("delivery"), Some(0));
    resumed.summary_work_started();
    let requalification = resumed.summary_context(false).unwrap();
    assert_eq!(
        requalification.timing.started_at_unix_ms,
        finished.timing.started_at_unix_ms
    );
    assert!(requalification.timing.finished_at_unix_ms.is_none());
}

#[test]
fn request_allowance_covers_retries_and_compaction_and_survives_resume_and_policy_changes() {
    let f = Fixture::new(Policy {
        max_requests_per_stage: 2,
        ..Policy::default()
    });
    let guard = f.run.open(None).unwrap();
    for activity in ["Retrying provider", "Compacting session"] {
        let usage = request(&f.root, activity);
        guard.before_request(&usage).unwrap();
        guard.record_usage(&usage);
    }
    drop(guard);
    let guard = f.run.open(None).unwrap();
    assert!(is_stop(
        &guard
            .before_request(&request(&f.root, "one more retry"))
            .unwrap_err()
    ));
    assert_eq!(f.ledger().requests.len(), 2);
    drop(guard);
    let guard = f
        .run
        .open(Some(Policy {
            max_requests_per_stage: 3,
            ..Policy::default()
        }))
        .unwrap();
    let usage = request(&f.root, "bounded recovery");
    guard.before_request(&usage).unwrap();
    guard.record_usage(&usage);
    assert_eq!(f.ledger().requests.len(), 3);
    assert_eq!(f.ledger().stage_requests, 3);
    assert_eq!(f.ledger().revisions.len(), 1);
}

#[test]
fn ambiguous_in_flight_work_requires_specific_reconciliation_without_resetting_usage() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    let usage = request(&f.root, "lost connection");
    guard.before_request(&usage).unwrap(); // Simulate termination before an outcome is saved.
    let inherited_until_exec = guard.owner_lock.try_clone().unwrap();
    drop(guard);
    let guard = f.run.open(None).unwrap();
    drop(inherited_until_exec);
    let error = guard
        .before_request(&request(&f.root, "retry"))
        .unwrap_err();
    assert!(is_stop(&error));
    assert_eq!(f.report()["trigger"], "unresolved_request");
    assert_eq!(f.report()["cost_unknown_attempts"], 1);
    assert_eq!(
        f.report()["requests"][&usage.id]["session"],
        "owned-session"
    );
    drop(guard);
    let policy = Policy {
        acknowledge_unresolved_requests: vec![usage.id],
        ..Policy::default()
    };
    let guard = f.run.open(Some(policy)).unwrap();
    let retry = request(&f.root, "reconciled bounded attempt");
    guard.before_request(&retry).unwrap();
    guard.record_usage(&retry);
    assert!(f.ledger().unresolved.is_empty());
    assert_eq!(f.ledger().requests.len(), 2);
    assert_eq!(f.ledger().stage_requests, 2);
}

#[test]
fn cancellation_failures_stay_unresolved_even_when_the_local_request_returns() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    let mut usage = request(&f.root, "interrupted");
    guard.before_request(&usage).unwrap();
    guard.cancellation_unconfirmed("backend abort did not acknowledge cancellation".into());
    usage.outcome = "policy_stop_incomplete".into();
    guard.record_usage(&usage);
    assert!(is_stop(
        &guard
            .before_request(&request(&f.root, "retry"))
            .unwrap_err()
    ));
    assert_eq!(
        f.report()["unresolved_requests"],
        serde_json::json!([usage.id])
    );
    assert_eq!(
        f.report()["cancellation_notes"].as_array().unwrap().len(),
        1
    );
}

#[test]
fn cost_is_context_and_missing_values_never_become_a_zero_total() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    let mut known = request(&f.root, "observed");
    known.reported_cost_usd = Some(1.5);
    guard.before_request(&known).unwrap();
    guard.record_usage(&known);
    guard.record_usage(&known);
    let unknown = request(&f.root, "unreported");
    guard.before_request(&unknown).unwrap();
    guard.record_usage(&unknown);
    guard.review(review(1, &[], &[])).unwrap();
    guard.review(review(2, &[], &[])).unwrap();
    guard.review(review(3, &[], &[])).unwrap();
    guard.boundary(Some("delivery"), "Repair").unwrap_err();
    assert_eq!(f.report()["reported_known_cost_subtotal_usd"], 1.5);
    assert_eq!(f.report()["cost_unknown_attempts"], 1);
}

#[test]
fn repeating_a_required_native_check_is_not_itself_a_waste_verdict() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    for _ in 0..3 {
        let mut usage = request(&f.root, "native premise changed; recheck required");
        usage.outcome = "completed".into();
        guard.before_request(&usage).unwrap();
        guard.record_usage(&usage);
        guard.boundary(Some("delivery"), "Verify").unwrap();
    }
    assert!(!f.run.directory.join("stop.json").exists());
}

#[test]
fn replan_allowance_stops_before_recovery_and_independent_progress_replenishes_it() {
    let f = Fixture::new(Policy::default());
    let guard = f.run.open(None).unwrap();
    fs::write(f.root.join("candidate"), "preserved partial work").unwrap();
    guard.before_replan().unwrap();
    guard.before_replan().unwrap();
    assert!(is_stop(&guard.before_replan().unwrap_err()));
    assert_eq!(
        fs::read_to_string(f.root.join("candidate")).unwrap(),
        "preserved partial work"
    );
    guard
        .verified("independent Verify for a delivered child slice".into())
        .unwrap();
    guard.before_replan().unwrap();
    assert_eq!(f.ledger().replans_without_progress, 1);
}

#[test]
fn policy_limits_are_validated_and_concurrent_resume_cannot_double_spend_the_allowance() {
    let f = Fixture::new(Policy::default());
    let path = f.root.join("policy.json");
    for invalid in [r#"{"max_no_progress_reviews":0}"#, r#"{"max_cost_usd":1}"#] {
        fs::write(&path, invalid).unwrap();
        assert!(Policy::load(Some(&path), false).is_err());
    }
    let _guard = f.run.open(None).unwrap();
    assert!(
        f.run
            .open(None)
            .err()
            .unwrap()
            .to_string()
            .contains("another process owns")
    );
}

#[test]
fn productive_reviews_cannot_silently_extend_a_selected_time_limit() {
    let f = Fixture::new(Policy {
        max_active_seconds: Some(1),
        ..Policy::default()
    });
    let mut guard = f.run.open(None).unwrap();
    guard.review(review(1, &[], &[])).unwrap();
    guard.review(review(2, &["F0001"], &[])).unwrap();
    guard.active_since = Instant::now() - Duration::from_secs(2);
    assert!(is_stop(&guard.poll().unwrap_err()));
    assert_eq!(f.report()["trigger"], "active_time_limit");
    assert_eq!(f.report()["trend"]["no_progress_reviews"], 0);
}

#[test]
fn corpus_failure_does_not_change_request_allowances_or_timeout_decisions() {
    for unavailable in [false, true] {
        let fixture = Fixture::new(Policy {
            max_requests_per_stage: 1,
            max_request_seconds: 1,
            ..Policy::default()
        });
        if unavailable {
            fs::write(
                fixture.root.join(".git/opsx-build/corpus"),
                "cannot create a directory here",
            )
            .unwrap();
        }
        let guard = fixture.run.open(None).unwrap();
        let usage = request(&fixture.root, "request");
        guard.before_request(&usage).unwrap();
        guard
            .active_request
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .started = Instant::now() - Duration::from_secs(2);
        assert!(is_stop(&guard.poll().unwrap_err()));
        assert_eq!(fixture.report()["trigger"], "request_timeout");
        guard.record_usage(&usage);
        assert!(is_stop(
            &guard
                .before_request(&request(&fixture.root, "next"))
                .unwrap_err()
        ));
        assert_eq!(fixture.report()["trigger"], "request_limit");
        assert_eq!(fixture.ledger().stage_requests, 1);
    }
}

#[test]
fn corpus_capture_preserves_watchdog_check_extension_and_its_expiry() {
    for unavailable in [false, true] {
        let fixture = Fixture::new(Policy {
            max_request_seconds: 1,
            request_progress_grace_seconds: Some(10),
            ..Policy::default()
        });
        if unavailable {
            fs::write(
                fixture.root.join(".git/opsx-build/corpus"),
                "capture unavailable",
            )
            .unwrap();
        }
        let directory = fixture.root.join(".git/opsx-build/checks/scope");
        fs::create_dir_all(directory.join("check")).unwrap();
        fs::write(
            directory.join("context.json"),
            serde_json::to_vec(&serde_json::json!({"repositories":[fixture.root],"generation":0}))
                .unwrap(),
        )
        .unwrap();
        let guard = fixture.run.open(None).unwrap();
        guard.observe_check_directory(directory.clone());
        let mut usage = request(&fixture.root, "implementation");
        usage.context.stage = Some("Apply".into());
        guard.before_request(&usage).unwrap();
        let now = crate::run_summary::now_ms();
        {
            let mut active = guard.active_request.lock().unwrap();
            let active = active.as_mut().unwrap();
            active.started = Instant::now() - Duration::from_secs(2);
            active.dispatched_at = now - 500;
        }
        let path = directory.join("check/result.json");
        let mut receipt = serde_json::json!({
            "format_version":1,"runner_version":"fixture","path":path,"key":"test","change":"delivery","stage":"Apply",
            "started_at_unix_ms":now-100,"sequence":1,"generation":0,
            "spec":{"key":"test","command":"true","cwd":fixture.root,"tools":[],"runtime_reason":"fixture","timeout_seconds":10},
            "inputs":null,"outcome":"passed","exit_code":0,"elapsed_ms":0,
            "stdout":directory.join("check/stdout.log"),"stderr":directory.join("check/stderr.log"),
            "log_hashes":null,"reused_from":null,"invalidation_reasons":[],"reuse_limitations":[]
        });
        fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        guard.poll().unwrap();
        assert_eq!(
            fixture.ledger().requests[&usage.id]
                .progress_extensions
                .len(),
            1
        );
        receipt["started_at_unix_ms"] = serde_json::json!(now - 20_000);
        fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        guard
            .active_request
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .progress
            .as_mut()
            .unwrap()
            .recheck_at = Duration::ZERO;
        assert!(is_stop(&guard.poll().unwrap_err()));
        assert_eq!(fixture.report()["trigger"], "request_timeout");
        assert!(
            fixture.report()["reason"]
                .as_str()
                .unwrap()
                .contains("stopped making check progress")
        );
    }
}
