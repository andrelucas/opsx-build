use super::*;
use crate::{
    ui::Ui,
    usage::{Attempt, Identity, tests::RecordingUi},
};

struct Fixture {
    root: PathBuf,
    policy: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("opsx-corpus-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("source.rs"), "original uncommitted source").unwrap();
        fs::write(root.join("requirements.md"), "original requirements").unwrap();
        let policy = root.join(".git/opsx-build/policy/run-one");
        fs::create_dir_all(&policy).unwrap();
        Self { root, policy }
    }
    fn recorder(&self) -> Recorder {
        Recorder::open(&self.policy).unwrap()
    }
    fn corpus(&self) -> PathBuf {
        self.root.join(".git/opsx-build/corpus")
    }
    fn export(&self) -> PathBuf {
        let output = self.root.join(format!("export-{}", uuid::Uuid::new_v4()));
        export(&self.corpus(), &output, None).unwrap();
        output
    }
    fn usage(&self) -> UsageRecord {
        let ui = RecordingUi::default();
        Attempt::new(
            &ui,
            &self.root,
            Identity {
                backend: "fixture",
                connection: None,
                model: Some("actor-model"),
                session: "session-one",
                invocation: "invocation-one",
                attempt: 1,
                fresh: true,
            },
            "Implement the exact requirement",
        )
        .record
        .clone()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn replayed_tool_results_are_captured_once_for_each_backend() {
    for backend in ["claude", "codex", "opencode"] {
        let fixture = Fixture::new();
        let ui = RecordingUi::default();
        let run =
            crate::operating_policy::Run::new(&fixture.root, Default::default(), &ui).unwrap();
        ui.set_operating_policy(std::sync::Arc::new(run.open(None).unwrap()))
            .unwrap();
        let mut usage = Attempt::new(
            &ui,
            &fixture.root,
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
            .before_request_with_prompt("Check the source")
            .unwrap();
        let (started, completed) = match backend {
            "claude" => (
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":"true"}}]}}),
                json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call","content":"passed","is_error":false}]}}),
            ),
            "codex" => (
                json!({"method":"item/started","params":{"item":{"type":"commandExecution","id":"call","command":"true","cwd":"."}}}),
                json!({"method":"item/completed","params":{"item":{"type":"commandExecution","id":"call","command":"true","cwd":".","aggregatedOutput":"passed","exitCode":0}}}),
            ),
            _ => (
                json!({"type":"tool_use","part":{"callID":"call","tool":"bash","state":{"status":"running","input":{"command":"true"}}}}),
                json!({"type":"tool_use","part":{"callID":"call","tool":"bash","state":{"status":"completed","input":{"command":"true"},"output":"passed","metadata":{"exit":0}}}}),
            ),
        };
        usage.event(&started);
        usage.event(&completed);
        // Claude streams events, then replays stdout through this path. Other
        // backends may also repeat completion notifications.
        usage.output(&completed.to_string());
        assert_eq!(usage.record.tools.len(), 1);
        assert!(usage.record.tools[0].completed);
        assert_eq!(usage.record.tools[0].failed, Some(false));
        usage.finish("ready");
        let output = fixture.export();
        let events = lines(&output.join("events.jsonl"));
        assert_eq!(
            events.iter().filter(|e| e["kind"] == "tool_result").count(),
            1,
            "{backend}"
        );
    }
}

#[test]
fn tool_result_deduplication_preserves_updates_calls_sessions_and_dispatches() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let mut usage = fixture.usage();
    recorder.start(&usage).unwrap();
    let original = json!({"call_id":"one","output":"first","failed":null,"exit_code":null});
    let mut expected = vec![original.clone()];
    recorder
        .observation(&usage, "tool_result", original.clone())
        .unwrap();
    recorder
        .observation(&usage, "tool_result", original.clone())
        .unwrap();
    let mut changed = original.clone();
    for (field, value) in [
        ("output", json!("second")),
        ("failed", json!(false)),
        ("exit_code", json!(0)),
        ("failed", json!(true)),
    ] {
        changed[field] = value;
        expected.push(changed.clone());
        recorder
            .observation(&usage, "tool_result", changed.clone())
            .unwrap();
    }
    // A nonconsecutive replay is still the same delivery, not a new update.
    recorder
        .observation(&usage, "tool_result", original.clone())
        .unwrap();
    let mut another_call = original.clone();
    another_call["call_id"] = json!("two");
    expected.push(another_call.clone());
    recorder
        .observation(&usage, "tool_result", another_call)
        .unwrap();
    usage.session_id = "another-session".into();
    expected.push(original.clone());
    recorder
        .observation(&usage, "tool_result", original.clone())
        .unwrap();
    // Compatibility dispatches may reuse a usage ID; ordinary retries do not.
    for change_request_id in [false, true] {
        if change_request_id {
            usage.id = "another-request".into();
        }
        recorder.start(&usage).unwrap();
        expected.push(original.clone());
        recorder
            .observation(&usage, "tool_result", original.clone())
            .unwrap();
    }
    let (events, _) = read_events(&recorder.directory).unwrap();
    let results = events
        .iter()
        .filter(|e| e.kind == "tool_result")
        .map(|e| e.data.clone())
        .collect::<Vec<_>>();
    assert_eq!(results, expected);
}

#[test]
fn tool_result_deduplication_uses_full_output_before_capture_clipping() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let usage = fixture.usage();
    recorder.start(&usage).unwrap();
    let first = json!({"call_id":"call","output":format!("{}a", "x".repeat(MAX_TEXT)),"failed":false,"exit_code":0});
    let mut changed = first.clone();
    changed["output"] = json!(format!("{}b", "x".repeat(MAX_TEXT)));
    assert_eq!(bounded(&first), bounded(&changed));
    recorder
        .observation(&usage, "tool_result", first.clone())
        .unwrap();
    recorder.observation(&usage, "tool_result", first).unwrap();
    recorder
        .observation(&usage, "tool_result", changed)
        .unwrap();
    let (events, _) = read_events(&recorder.directory).unwrap();
    assert_eq!(events.iter().filter(|e| e.kind == "tool_result").count(), 2);
}

#[test]
fn legacy_tool_result_replays_are_removed_only_from_model_inputs() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    let usage = fixture.usage();
    let first = json!({"call_id":"call","output":"first","failed":false,"exit_code":0});
    let changed = json!({"call_id":"call","output":"changed","failed":true,"exit_code":7});
    let another = json!({"call_id":"another","output":"first","failed":false,"exit_code":0});
    recorder.start(&usage).unwrap();
    // Write raw events to reproduce recordings made before capture deduplication.
    recorder.event("tool_result", first.clone()).unwrap();
    recorder.decision("first", "observe", json!({})).unwrap();
    recorder.event("tool_result", first.clone()).unwrap();
    recorder.event("tool_result", changed.clone()).unwrap();
    recorder.event("tool_result", changed.clone()).unwrap();
    recorder.event("tool_result", another.clone()).unwrap();
    recorder.decision("second", "observe", json!({})).unwrap();
    recorder.start(&usage).unwrap();
    recorder.event("tool_result", first.clone()).unwrap();
    recorder
        .decision("next-dispatch", "observe", json!({}))
        .unwrap();
    let (raw, _) = read_events(&recorder.directory).unwrap();
    let output = fixture.export();
    assert_eq!(lines(&output.join("events.jsonl")).len(), raw.len());
    let inputs = lines(&output.join("model-inputs.jsonl"));
    let results = inputs
        .iter()
        .map(|input| {
            input["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["kind"] == "tool_result")
                .map(|event| event["data"].clone())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        vec![vec![first.clone()], vec![changed, another], vec![first]]
    );
    let decisions = lines(&output.join("decisions.jsonl"));
    let ids = raw
        .iter()
        .filter(|e| e.kind == "decision")
        .map(|e| format!("{}:{}", e.run_id, e.sequence))
        .collect::<Vec<_>>();
    assert_eq!(
        decisions
            .iter()
            .map(|d| d["decision_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        ids
    );
    for (decision, input) in decisions.iter().zip(inputs) {
        assert_eq!(decision["input"], input);
        assert!(
            input["events"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["sequence"].as_u64().unwrap()
                    < input["cutoff_sequence"].as_u64().unwrap())
        );
    }
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["duplicate_tool_results_omitted_from_inputs"], 2);
}

#[test]
fn incomplete_legacy_tool_results_remain_in_inputs_without_assuming_equality() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.start(&fixture.usage()).unwrap();
    for data in [
        json!({"call_id":null,"output":"same","failed":false,"exit_code":0}),
        json!({"truncated":true,"original_bytes":99999,"retained_json_prefix":"same prefix"}),
        json!({"call_id":"call","output":"[REDACTED]","failed":false,"exit_code":0}),
    ] {
        recorder.event("tool_result", data.clone()).unwrap();
        recorder.event("tool_result", data).unwrap();
    }
    recorder
        .decision("checkpoint", "observe", json!({}))
        .unwrap();
    let output = fixture.export();
    let inputs = lines(&output.join("model-inputs.jsonl"));
    assert_eq!(
        inputs[0]["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "tool_result")
            .count(),
        6
    );
}

#[test]
fn exported_inputs_are_frozen_prefixes_with_durable_dirty_source_and_later_outcomes_separate() {
    let fixture = Fixture::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            fixture.root.join("source.rs"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let usage = fixture.usage();
    let mut recorder = fixture.recorder();
    recorder.start(&usage).unwrap();
    recorder
        .prompt(&usage, "Implement the original requirement")
        .unwrap();
    recorder
        .observation(
            &usage,
            "tool_call",
            json!({"call_id":"read-1","name":"Read","input":{"file_path":"source.rs"}}),
        )
        .unwrap();
    recorder
        .decision("checkpoint", "observe", json!({}))
        .unwrap();
    fs::write(fixture.root.join("source.rs"), "future repair").unwrap();
    recorder
        .observation(&usage, "agent_text", json!({"text":"future diagnosis"}))
        .unwrap();
    recorder.finish(&usage, None).unwrap();
    recorder
        .event("run_outcome", json!({"status":"accepted later"}))
        .unwrap();
    recorder.event("capture_closed", json!({})).unwrap();
    let output = fixture.export();
    let inputs = lines(&output.join("model-inputs.jsonl"));
    assert_eq!(inputs.len(), 3);
    assert!(!inputs[0].to_string().contains("future"));
    assert!(!inputs[1].to_string().contains("future"));
    assert!(!inputs[0].to_string().contains("actor-model"));
    assert!(inputs[2].to_string().contains("future diagnosis"));
    for input in &inputs {
        let cutoff = input["cutoff_sequence"].as_u64().unwrap();
        for event in input["events"].as_array().unwrap() {
            assert!(event["sequence"].as_u64().unwrap() < cutoff);
            assert!(!event["kind"].as_str().unwrap().ends_with("_outcome"));
        }
    }
    let hash = inputs[0]["file_snapshot"]["data"]["manifest_sha256"]
        .as_str()
        .unwrap();
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("objects").join(hash)).unwrap()).unwrap();
    let original = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "source.rs")
        .unwrap();
    #[cfg(unix)]
    assert_eq!(original["executable"], true);
    assert_eq!(
        fs::read_to_string(
            output
                .join("objects")
                .join(original["sha256"].as_str().unwrap())
        )
        .unwrap(),
        "original uncommitted source"
    );
    assert_eq!(
        inputs[2]["baseline"]["file_snapshot"],
        inputs[1]["file_snapshot"]
    );
    let decisions = lines(&output.join("decisions.jsonl"));
    assert!(
        decisions
            .iter()
            .all(|d| d["annotation"].is_null() && d["training_eligible"] == false)
    );
    assert!(
        decisions[0]["later_outcome_sequences"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );
    assert_eq!(lines(&output.join("outcomes.jsonl")).len(), 2);
    assert!(output.join("schema.json").is_file());
}

#[test]
fn resume_keeps_identity_and_sequence_after_an_interrupted_final_line() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder
        .event("decision", json!({"observed_action":"observe"}))
        .unwrap();
    let last = recorder.sequence;
    recorder.file.write_all(b"{\"partial\":").unwrap();
    recorder.file.flush().unwrap();
    drop(recorder);
    let mut resumed = fixture.recorder();
    assert!(resumed.sequence > last);
    resumed.decision("resume", "observe", json!({})).unwrap();
    resumed.event("capture_closed", json!({})).unwrap();
    let output = fixture.export();
    let events = lines(&output.join("events.jsonl"));
    assert!(events.iter().all(|e| e["run_id"] == "run-one"));
    let sequences = events
        .iter()
        .map(|e| e["sequence"].as_u64().unwrap())
        .collect::<Vec<_>>();
    assert!(sequences.windows(2).all(|p| p[0] < p[1]));
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert!(!manifest["capture_gaps"].as_array().unwrap().is_empty());
}

#[test]
fn clean_resume_does_not_hide_an_earlier_interrupted_capture_segment() {
    let fixture = Fixture::new();
    let mut interrupted = fixture.recorder();
    interrupted.start(&fixture.usage()).unwrap();
    interrupted
        .prompt(&fixture.usage(), "unfinished work")
        .unwrap();
    drop(interrupted); // Complete final JSON line, but no process-close event.
    let mut resumed = fixture.recorder();
    resumed.event("capture_closed", json!({})).unwrap();
    let output = fixture.export();
    let manifest = fs::read_to_string(output.join("manifest.json")).unwrap();
    assert!(manifest.contains("segment is live or interrupted"));
    assert!(lines(&output.join("outcomes.jsonl")).is_empty());
}

#[test]
fn private_reasoning_is_excluded_and_public_tool_results_are_retained() {
    let fixture = Fixture::new();
    let ui = RecordingUi::default();
    let run = crate::operating_policy::Run::new(&fixture.root, Default::default(), &ui).unwrap();
    let guard = std::sync::Arc::new(run.open(None).unwrap());
    ui.set_operating_policy(guard.clone()).unwrap();
    let mut attempt = Attempt::new(
        &ui,
        &fixture.root,
        Identity {
            backend: "claude",
            connection: None,
            model: None,
            session: "session",
            invocation: "invocation",
            attempt: 1,
            fresh: true,
        },
        "Apply",
    );
    attempt
        .before_request_with_prompt("Fix the source")
        .unwrap();
    attempt.event(&json!({"type":"assistant","message":{"content":[
        {"type":"thinking","thinking":"PRIVATE_REASONING_SENTINEL"},
        {"type":"text","text":"Inspecting the compiler failure"},
        {"type":"tool_use","id":"call-1","name":"Bash","input":{"command":"cargo check","api_key":"STRUCTURED_SECRET_SENTINEL"}}
    ]}}));
    attempt.event(&json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call-1","content":"compiler exit 1","is_error":true}]}}));
    attempt.finish("retry");
    for backend in ["codex", "opencode"] {
        let mut attempt = Attempt::new(
            &ui,
            &fixture.root,
            Identity {
                backend,
                connection: None,
                model: None,
                session: backend,
                invocation: backend,
                attempt: 1,
                fresh: true,
            },
            "Apply",
        );
        attempt
            .before_request_with_prompt("Fix the source")
            .unwrap();
        if backend == "codex" {
            attempt.event(&json!({"method":"item/completed","params":{"item":{"id":"r","type":"reasoning","text":"PRIVATE_REASONING_SENTINEL"}}}));
            attempt.event(&json!({"method":"item/completed","params":{"item":{"id":"a","type":"agentMessage","text":"Public Codex observation"}}}));
        } else {
            attempt
                .event(&json!({"type":"reasoning","part":{"text":"PRIVATE_REASONING_SENTINEL"}}));
            attempt.event(&json!({"type":"text","part":{"text":"Public OpenCode observation"}}));
        }
        attempt.finish("ready");
    }
    guard.corpus_outcome(json!({"status":"paused"}));
    let output = fixture.export();
    let text = fs::read_to_string(output.join("events.jsonl")).unwrap();
    assert!(!text.contains("PRIVATE_REASONING_SENTINEL"));
    assert!(!text.contains("STRUCTURED_SECRET_SENTINEL"));
    assert!(text.contains("[REDACTED]"));
    assert!(text.contains("compiler exit 1"));
    assert!(text.contains("Inspecting the compiler failure"));
    assert!(text.contains("Public Codex observation"));
    assert!(text.contains("Public OpenCode observation"));
}

#[test]
fn mutable_receipts_and_logs_are_copied_at_observation_time() {
    let fixture = Fixture::new();
    let directory = fixture.root.join(".git/opsx-build/checks/scope");
    let check = directory.join("check");
    fs::create_dir_all(&check).unwrap();
    fs::write(
        directory.join("context.json"),
        serde_json::to_vec(&json!({"generation":3,"repositories":[fixture.root]})).unwrap(),
    )
    .unwrap();
    fs::write(
        check.join("result.json"),
        br#"{"outcome":"failed","exit_code":7,"inputs":{"hash":"old"}}"#,
    )
    .unwrap();
    fs::write(check.join("stdout.log"), "original failure output").unwrap();
    let mut recorder = fixture.recorder();
    recorder.scope(&directory).unwrap();
    recorder.start(&fixture.usage()).unwrap();
    recorder.checks(Some(&directory)).unwrap();
    recorder
        .decision("checkpoint", "observe", json!({}))
        .unwrap();
    let acceptance = directory.join("acceptance.json");
    fs::write(
        &acceptance,
        br#"{"outcome":"failed","results":[{"exit_code":4}]}"#,
    )
    .unwrap();
    fs::write(directory.join("1.stdout"), "independent acceptance failure").unwrap();
    recorder
        .artifact("acceptance_outcome", &acceptance)
        .unwrap();
    fs::remove_file(directory.join("1.stdout")).unwrap();
    fs::write(check.join("stdout.log"), "future success output").unwrap();
    fs::remove_file(check.join("result.json")).unwrap();
    let output = fixture.export();
    let inputs = lines(&output.join("model-inputs.jsonl"));
    assert!(inputs[0].to_string().contains("original failure output"));
    assert!(!inputs[0].to_string().contains("future success output"));
    assert_eq!(inputs[0]["check_scope"]["scope_id"], "scope");
    assert_eq!(inputs[0]["check_scope"]["generation"], 3);
    assert!(
        fs::read_to_string(output.join("outcomes.jsonl"))
            .unwrap()
            .contains("independent acceptance failure")
    );
}

#[test]
fn sampling_and_capture_limits_are_explicit_without_labeling_progress() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join(".env"), "DO_NOT_COPY").unwrap();
    fs::write(
        fixture.root.join("large"),
        vec![b'x'; MAX_FILE as usize + 1],
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("source.rs", fixture.root.join("link")).unwrap();
    let mut recorder = fixture.recorder();
    recorder.start(&fixture.usage()).unwrap();
    let sequence = recorder.sequence;
    recorder.sample(None, 100).unwrap();
    assert_eq!(recorder.sequence, sequence);
    recorder.sampled = Instant::now() - SAMPLE_INTERVAL;
    recorder.sample(None, 60_000).unwrap();
    assert!(recorder.sequence > sequence);
    let output = fixture.export();
    let inputs = lines(&output.join("model-inputs.jsonl"));
    let hash = inputs[0]["file_snapshot"]["data"]["manifest_sha256"]
        .as_str()
        .unwrap();
    let manifest = fs::read_to_string(output.join("objects").join(hash)).unwrap();
    assert!(manifest.contains("sensitive filename"));
    assert!(manifest.contains("byte budget"));
    assert!(!manifest.contains("DO_NOT_COPY"));
    let clipped = bounded(&json!({"text":"界".repeat(MAX_TEXT)}));
    assert_eq!(clipped["truncated"], true);
    assert!(clipped["retained_json_prefix"].as_str().unwrap().len() <= MAX_TEXT);
}

#[test]
fn export_rejects_corruption_without_publishing_a_partial_bundle_or_overwriting_files() {
    let fixture = Fixture::new();
    let mut recorder = fixture.recorder();
    recorder.start(&fixture.usage()).unwrap();
    recorder.prompt(&fixture.usage(), "Goal").unwrap();
    let hash = digest(b"original uncommitted source");
    fs::write(recorder.directory.join("objects").join(hash), "corrupt").unwrap();
    let output = fixture.root.join("export");
    assert!(export(&fixture.corpus(), &output, None).is_err());
    assert!(!output.exists());
    fs::create_dir(&output).unwrap();
    fs::write(output.join("keep"), "keep").unwrap();
    assert!(export(&fixture.corpus(), &output, None).is_err());
    assert_eq!(fs::read_to_string(output.join("keep")).unwrap(), "keep");
}

#[test]
fn export_does_not_treat_source_json_as_an_object_manifest_or_copy_orphans() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("config.json"),
        r#"{"files":[{"sha256":"not-a-reference"}]}"#,
    )
    .unwrap();
    let mut recorder = fixture.recorder();
    recorder.start(&fixture.usage()).unwrap();
    recorder.prompt(&fixture.usage(), "Goal").unwrap();
    let orphan = recorder.object(b"orphaned future content").unwrap();
    let output = fixture.export();
    assert!(!output.join("objects").join(orphan).exists());
}
