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
        assert!(Policy::read(&path).is_err());
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
