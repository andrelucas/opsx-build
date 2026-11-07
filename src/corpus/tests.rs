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
