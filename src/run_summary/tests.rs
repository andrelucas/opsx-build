use super::*;
use crate::usage::{Attempt, Identity, UsageRecord, tests::RecordingUi};

struct Fixture {
    root: PathBuf,
    context: Context,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("opsx-summary-{}", uuid::Uuid::new_v4()));
        let directory = root.join("policy/run");
        fs::create_dir_all(&directory).unwrap();
        Self {
            root,
            context: Context {
                directory,
                timing: Timing {
                    started_at_unix_ms: Some(1000),
                    finished_at_unix_ms: Some(101000),
                    finished_active_ms: Some(40000),
                    ..Timing::default()
                },
                active_ms: 90000,
                active_time_has_gap: false,
                requests: BTreeMap::new(),
            },
        }
    }
    fn record(&mut self, id: &str, backend: &str, model: &str) -> UsageRecord {
        let ui = RecordingUi::default();
        let mut record = Attempt::new(
            &ui,
            &self.root,
            Identity {
                backend,
                connection: None,
                model: None,
                session: "session",
                invocation: "invocation",
                attempt: 1,
                fresh: true,
            },
            "test",
        )
        .record
        .clone();
        record.id = id.into();
        record.backend = backend.into();
        record.reported_models = vec![model.into()];
        record.usage_scope = "thread_delta".into();
        record.tokens = crate::usage::Tokens {
            input: Some(100),
            output: Some(20),
            cache_read: Some(80),
            cache_write: None,
            reasoning: Some(5),
        };
        record.elapsed_ms = 3000;
        record.outcome = "verified".into();
        self.context.requests.insert(
            id.into(),
            Request {
                backend: backend.into(),
                context: UsageContext::default(),
                elapsed_ms: 3000,
                outcome: Some("verified".into()),
                dispatches: 1,
            },
        );
        record
    }
    fn usage(&self, records: &[UsageRecord]) {
        fs::write(
            self.root.join("usage.jsonl"),
            records
                .iter()
                .map(|r| serde_json::to_string(r).unwrap() + "\n")
                .collect::<String>(),
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn clocks_and_change_timing_survive_resume_without_counting_paused_time_as_active() {
    let mut timing = Timing {
        started_at_unix_ms: Some(1000),
        ..Timing::default()
    };
    timing.boundary(Some("slice"), "Propose", 100, true);
    timing.boundary(Some("slice"), "Apply", 1000, false);
    let mut timing: Timing = serde_json::from_slice(&serde_json::to_vec(&timing).unwrap()).unwrap();
    timing.boundary(Some("slice"), "Verify", 2100, false);
    timing.boundary(Some("slice"), "Complete", 4100, false);
    assert_eq!(timing.elapsed_seconds("slice", 9000), Some(4));
    timing.boundary(Some("legacy"), "Apply", 5000, false);
    assert_eq!(timing.elapsed_seconds("legacy", 9000), None);
    timing.boundary(Some("interrupted"), "Apply", 6000, true);
    timing.interrupted(Some("interrupted"));
    assert_eq!(timing.elapsed_seconds("interrupted", 9000), None);
    timing.boundary(Some("later"), "Propose", 7000, true);
    assert_eq!(timing.elapsed_seconds("later", 9000), Some(2));
    let f = Fixture::new();
    let summary = Summary::collect(&f.context, "complete", "Done", None, vec![]);
    assert_eq!(summary.wall_clock_seconds, Some(100));
    assert_eq!(summary.active_seconds, 40);
}

#[test]
fn ordinary_explore_and_propose_time_is_kept_before_the_change_is_named() {
    let mut timing = Timing::default();
    timing.boundary(None, "Explore", 100, true);
    let mut timing: Timing = serde_json::from_slice(&serde_json::to_vec(&timing).unwrap()).unwrap();
    timing.boundary(None, "Propose", 2100, false);
    timing.boundary(Some("ordinary-change"), "Apply", 4100, true);
    timing.boundary(Some("ordinary-change"), "Complete", 9100, false);
    assert_eq!(timing.elapsed_seconds("ordinary-change", 20000), Some(9));
    let mut legacy = Timing::default();
    legacy.boundary(None, "Explore", 100, false);
    legacy.boundary(Some("legacy-change"), "Apply", 2100, true);
    assert_eq!(legacy.elapsed_seconds("legacy-change", 9100), None);
    let mut interrupted = Timing::default();
    interrupted.boundary(None, "Explore", 100, true);
    interrupted.interrupted(None);
    interrupted.boundary(Some("interrupted-change"), "Apply", 2100, true);
    assert_eq!(
        interrupted.elapsed_seconds("interrupted-change", 9100),
        None
    );
}

#[test]
fn usage_is_scoped_deduplicated_and_never_adds_cached_or_reasoning_tokens_twice() {
    let mut f = Fixture::new();
    let mut first = f.record("first", "codex", "worker-model");
    first.reported_cost_usd = Some(1.25);
    let mut duplicate = first.clone();
    duplicate.tokens.input = Some(150);
    let second = f.record("second", "codex", "worker-model");
    let mut unrelated = second.clone();
    unrelated.id = "another-run".into();
    unrelated.tokens.input = Some(999999);
    f.usage(&[first, duplicate, second, unrelated]);
    let summary = Summary::collect(&f.context, "complete", "Done", None, vec![]);
    let model = &summary.models["codex / worker-model"];
    assert_eq!(summary.attempts, 2);
    assert_eq!(model.input.known, 250);
    assert_eq!(model.cached_input.known, 160);
    assert_eq!(model.output.known, 40);
    assert_eq!(model.reasoning.known, 10);
    assert_eq!(model.reported_cost_usd, 1.25);
    assert!(summary.render().contains("$1.2500+? reported"));
    assert_eq!(summary.usage_records_missing, 0);
}

#[test]
fn missing_partial_and_mixed_model_observations_are_labelled_without_estimated_costs() {
    let mut f = Fixture::new();
    let mut mixed = f.record("mixed", "claude", "opus");
    mixed.reported_models.push("haiku".into());
    mixed.usage_scope = "main_agent_last_result".into();
    let mut missing = f.record("missing", "claude", "opus");
    missing.tokens.input = None;
    missing.tokens.output = None;
    missing.tokens.cache_read = None;
    f.record("lost", "codex", "worker-model");
    f.usage(&[mixed, missing]);
    let summary = Summary::collect(
        &f.context,
        "failed",
        "Verify",
        Some("blocked".into()),
        vec![],
    );
    let text = summary.render();
    assert!(text.contains("claude / haiku + opus"));
    assert!(text.contains("100+? input"));
    assert!(text.contains("unknown input"));
    assert!(text.contains("cost unavailable"));
    assert!(text.contains("Detailed usage unavailable for 1 attempt(s)"));
    assert_eq!(
        summary.models["codex / unreported"].input.reported_attempts,
        0
    );
}

#[test]
fn saved_failure_and_pause_summaries_keep_completed_bootstrap_rows_and_unknowns() {
    let f = Fixture::new();
    let rows = vec![
        Row {
            name: "Bootstrap".into(),
            status: "complete".into(),
            elapsed_seconds: Some(65),
            commit: Some("abcdef0123456789".into()),
        },
        Row {
            name: "0001-delivery".into(),
            status: "unfinished: Verify".into(),
            elapsed_seconds: None,
            commit: None,
        },
    ];
    let summary = Summary::collect(
        &f.context,
        "failed",
        "Verify",
        Some("actual blocker".into()),
        rows,
    );
    let path = summary.save(&f.context).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["rows"][0]["commit"], "abcdef0123456789");
    assert_eq!(saved["rows"][1]["elapsed_seconds"], serde_json::Value::Null);
    assert_eq!(saved["error"], "actual blocker");
    let text = fs::read_to_string(f.context.directory.join("summary.txt")).unwrap();
    assert!(text.contains("RUN SUMMARY — FAILED"));
    assert!(text.contains("1m 05s"));
    assert!(text.contains("abcdef012345"));
    assert!(text.contains("unfinished: Verify"));
    let paused = Summary::collect(&f.context, "paused", "Propose", None, vec![]);
    assert!(paused.render().contains("RUN SUMMARY — PAUSED"));
}

#[test]
fn absent_historical_clock_and_usage_do_not_become_zero_or_prevent_a_summary() {
    let mut f = Fixture::new();
    f.context.timing = Timing::default();
    f.context.active_time_has_gap = true;
    f.record("lost", "codex", "worker-model");
    let summary = Summary::collect(&f.context, "failed", "Apply", None, vec![]);
    assert_eq!(summary.wall_clock_seconds, None);
    assert!(summary.render().contains("Wall clock: unknown"));
    assert!(summary.render().contains("recorded subtotal"));
    assert!(summary.render().contains("cost unavailable"));
}
