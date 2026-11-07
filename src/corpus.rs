//! Passive, local progress evidence. Nothing in this module grants more work or acceptance.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::usage::UsageRecord;

const MAX_TEXT: usize = 64 * 1024;
const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 4096;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Event {
    schema_version: u32,
    run_id: String,
    sequence: u64,
    recorded_at_unix_ms: u64,
    kind: String,
    request: Option<Value>,
    data: Value,
}

pub(crate) struct Capture {
    writer: Mutex<Option<Recorder>>,
}

pub(crate) struct Recorder {
    directory: PathBuf,
    run_id: String,
    file: File,
    sequence: u64,
    request: Option<Value>,
    repo: Option<PathBuf>,
    repositories: Vec<PathBuf>,
    sampled: Instant,
    receipts: BTreeMap<PathBuf, String>,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Structured credential fields are redacted. Arbitrary source/output text is
/// still private project data; export is local, never automatic publication.
fn redact(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let name = key.to_ascii_lowercase().replace('-', "_");
                    let secret = [
                        "password",
                        "secret",
                        "api_key",
                        "apikey",
                        "authorization",
                        "access_token",
                        "refresh_token",
                        "private_key",
                    ]
                    .iter()
                    .any(|part| name.contains(part));
                    (
                        key.clone(),
                        if secret {
                            json!("[REDACTED]")
                        } else {
                            redact(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        _ => value.clone(),
    }
}

fn bounded(value: &Value) -> Value {
    let value = redact(value);
    let text = serde_json::to_string(&value).expect("JSON value");
    if text.len() <= MAX_TEXT {
        value
    } else {
        let mut end = MAX_TEXT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        json!({"truncated":true, "original_bytes":text.len(), "retained_json_prefix":&text[..end]})
    }
}

fn log_excerpt(path: &Path) -> Value {
    let result = (|| -> std::io::Result<Value> {
        let file = File::open(path)?;
        let total = file.metadata()?.len();
        let mut data = Vec::new();
        file.take(MAX_TEXT as u64).read_to_end(&mut data)?;
        Ok(
            json!({"text":String::from_utf8_lossy(&data),"original_bytes":total,
            "truncated":total > data.len() as u64}),
        )
    })();
    result.unwrap_or_else(|error| json!({"unavailable":error.to_string()}))
}

impl Capture {
    pub(crate) fn open(policy_directory: &Path) -> Self {
        let writer = Recorder::open(policy_directory);
        if let Err(error) = &writer {
            eprintln!("Warning: corpus capture unavailable (workflow continues): {error:#}");
        }
        Self {
            writer: Mutex::new(writer.ok()),
        }
    }

    pub(crate) fn observe(&self, operation: impl FnOnce(&mut Recorder) -> Result<()>) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(recorder) = writer.as_mut()
            && let Err(error) = operation(recorder)
        {
            let message = format!("corpus capture disabled (workflow continues): {error:#}");
            let _ = fs::write(recorder.directory.join("capture-error.txt"), &message);
            eprintln!("Warning: {message}");
            *writer = None;
        }
    }

    pub(crate) fn close(&self) {
        self.observe(|recorder| recorder.event("capture_closed", json!({})));
        *self
            .writer
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

impl Recorder {
    fn open(policy: &Path) -> Result<Self> {
        let run_id = policy
            .file_name()
            .context("policy run ID missing")?
            .to_string_lossy()
            .into_owned();
        let directory = policy
            .parent()
            .and_then(Path::parent)
            .context("policy metadata missing")?
            .join("corpus")
            .join(&run_id);
        fs::create_dir_all(directory.join("objects"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        let (events, gaps) = read_events(&directory)?;
        let sequence = events.last().map_or(0, |event| event.sequence);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(directory.join(format!("events-{}.jsonl", uuid::Uuid::new_v4())))?;
        let mut recorder = Self {
            directory,
            run_id,
            file,
            sequence,
            request: None,
            repo: None,
            repositories: Vec::new(),
            sampled: Instant::now(),
            receipts: BTreeMap::new(),
        };
        recorder.event("capture_started", json!({"runner_version":env!("CARGO_PKG_VERSION"),
            "prior_capture_gaps":gaps, "policy_run":policy,
            "host":Command::new("hostname").output().ok().filter(|o|o.status.success()).map(|o|String::from_utf8_lossy(&o.stdout).trim().to_owned())}))?;
        Ok(recorder)
    }

    pub(crate) fn event(&mut self, kind: &str, data: Value) -> Result<()> {
        let event = Event {
            schema_version: 1,
            run_id: self.run_id.clone(),
            sequence: self.sequence + 1,
            recorded_at_unix_ms: crate::run_summary::now_ms(),
            kind: kind.into(),
            request: self.request.clone(),
            data,
        };
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        self.sequence = event.sequence;
        Ok(())
    }

    fn object(&self, bytes: &[u8]) -> Result<String> {
        let hash = digest(bytes);
        let path = self.directory.join("objects").join(&hash);
        if path.exists() {
            if digest(&fs::read(&path)?) != hash {
                bail!("corrupt corpus object {hash}");
            }
        } else {
            let temporary = path.with_extension(uuid::Uuid::new_v4().to_string());
            fs::write(&temporary, bytes)?;
            fs::rename(temporary, path)?;
        }
        Ok(hash)
    }

    pub(crate) fn start(&mut self, usage: &UsageRecord) -> Result<()> {
        self.request = Some(
            json!({"request_id":usage.id, "invocation_id":usage.invocation_id,
            "attempt":usage.attempt, "session_id":usage.session_id,
            "context":{"stage":usage.context.stage,"change":usage.context.change,"iteration":usage.context.iteration}}),
        );
        self.repo = Some(usage.repo.clone());
        self.sampled = Instant::now();
        self.event(
            "request_started",
            json!({"repo":usage.repo, "activity":usage.activity,
            "backend":usage.backend, "requested_model":usage.requested_model}),
        )?;
        self.snapshot()?;
        Ok(())
    }

    pub(crate) fn observation(
        &mut self,
        usage: &UsageRecord,
        kind: &str,
        data: Value,
    ) -> Result<()> {
        // Observed IDs may be updated by the provider after dispatch.
        if let Some(request) = self.request.as_mut() {
            request["session_id"] = json!(usage.session_id);
        }
        self.event(kind, bounded(&data))
    }

    pub(crate) fn prompt(&mut self, usage: &UsageRecord, prompt: &str) -> Result<()> {
        self.observation(usage, "task", json!({"text":prompt}))?;
        self.decision("request_dispatch", "dispatch", json!({}))
    }

    pub(crate) fn finish(&mut self, usage: &UsageRecord, checks: Option<&Path>) -> Result<()> {
        self.checks(checks)?;
        self.snapshot()?;
        self.decision("request_finished", "observe", json!({}))?;
        self.event("request_outcome", bounded(&serde_json::to_value(usage)?))?;
        self.request = None;
        Ok(())
    }

    pub(crate) fn sample(&mut self, checks: Option<&Path>, active_ms: u64) -> Result<()> {
        if self.request.is_none() || self.sampled.elapsed() < SAMPLE_INTERVAL {
            return Ok(());
        }
        self.sampled = Instant::now();
        self.checks(checks)?;
        self.snapshot()?;
        self.decision(
            "periodic_observation",
            "observe",
            json!({"active_elapsed_known_ms":active_ms}),
        )
    }

    pub(crate) fn decision(&mut self, trigger: &str, action: &str, state: Value) -> Result<()> {
        self.event(
            "decision",
            json!({"trigger":trigger, "observed_action":action, "state":state}),
        )
    }

    pub(crate) fn scope(&mut self, directory: &Path) -> Result<()> {
        let mut context: Value =
            serde_json::from_slice(&fs::read(directory.join("context.json"))?)?;
        self.repositories = context["repositories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect();
        context["scope_id"] = json!(directory.file_name().and_then(|name| name.to_str()));
        self.event("check_scope", context)
    }

    pub(crate) fn artifact(&mut self, kind: &str, path: &Path) -> Result<()> {
        if !path.exists() {
            return self.event("artifact_missing", json!({"kind":kind,"source_path":path}));
        }
        let bytes = fs::read(path)?;
        // Artifacts are immutable copies at observation time, not deferred paths.
        let mut value: Value = serde_json::from_slice(&bytes)?;
        let object = self.object(&serde_json::to_vec(&redact(&value))?)?;
        if let Some(fields) = value.as_object_mut() {
            fields.remove("inputs");
            fields.remove("files");
        }
        let mut logs = BTreeMap::new();
        if kind == "acceptance_outcome" {
            for index in 1..=value["results"].as_array().map_or(0, Vec::len) {
                for suffix in ["stdout", "stderr"] {
                    let name = format!("{index}.{suffix}");
                    logs.insert(
                        name.clone(),
                        log_excerpt(&path.parent().context("artifact parent")?.join(name)),
                    );
                }
            }
        }
        self.event(
            kind,
            json!({"source_path":path, "record":bounded(&value),"artifact_sha256":object,"logs":logs}),
        )
    }

    pub(crate) fn checks(&mut self, directory: Option<&Path>) -> Result<()> {
        let Some(directory) = directory else {
            return Ok(());
        };
        let mut paths = fs::read_dir(directory)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join("result.json"))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let bytes = fs::read(&path)?;
            let hash = digest(&bytes);
            if self.receipts.get(&path) == Some(&hash) {
                continue;
            }
            let mut record: Value = serde_json::from_slice(&bytes)?;
            let object = self.object(&serde_json::to_vec(&redact(&record))?)?;
            if let Some(fields) = record.as_object_mut() {
                fields.remove("inputs");
            }
            let mut logs = BTreeMap::new();
            // Read only the logs adjacent to the receipt, not arbitrary paths in JSON.
            for name in ["stdout.log", "stderr.log"] {
                let log = path.parent().expect("receipt parent").join(name);
                logs.insert(name, log_excerpt(&log));
            }
            self.event("check_receipt", json!({"source_path":path, "record":bounded(&record), "logs":logs,"artifact_sha256":object}))?;
            self.receipts.insert(path, hash);
        }
        Ok(())
    }

    fn snapshot(&mut self) -> Result<()> {
        let Some(repo) = &self.repo else {
            return Ok(());
        };
        let mut roots = self.repositories.clone();
        if !roots.contains(repo) {
            roots.push(repo.clone());
        }
        let started = crate::run_summary::now_ms();
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        let mut total = 0;
        for root in roots {
            let output = Command::new("git")
                .args([
                    "--no-optional-locks",
                    "ls-files",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                    "-z",
                ])
                .current_dir(&root)
                .output()?;
            if !output.status.success() {
                bail!("cannot list corpus inputs in {}", root.display());
            }
            let paths = output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|path| !path.is_empty())
                .collect::<BTreeSet<_>>();
            for raw in paths {
                let Ok(name) = std::str::from_utf8(raw) else {
                    skipped.push(json!({"root":root,"reason":"non-UTF-8 path"}));
                    continue;
                };
                let relative = Path::new(name);
                let path = root.join(relative);
                let reason = if !relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                    || relative
                        .ancestors()
                        .filter(|p| !p.as_os_str().is_empty())
                        .any(|p| {
                            fs::symlink_metadata(root.join(p))
                                .is_ok_and(|m| m.file_type().is_symlink())
                        }) {
                    Some("unsafe or symlink path")
                } else if relative
                    .file_name()
                    .and_then(|p| p.to_str())
                    .is_some_and(|name| {
                        name == ".env"
                            || name.starts_with(".env.")
                            || matches!(name, "id_rsa" | "id_ed25519" | "credentials.json")
                            || name.ends_with(".pem")
                    })
                {
                    Some("sensitive filename")
                } else if files.len() >= MAX_FILES || total >= MAX_SNAPSHOT {
                    Some("snapshot budget")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    skipped.push(json!({"root":root,"path":name,"reason":reason}));
                    continue;
                }
                match File::open(&path) {
                    Ok(file) if file.metadata()?.is_file() => {
                        #[cfg(unix)]
                        let executable = {
                            use std::os::unix::fs::PermissionsExt;
                            file.metadata()?.permissions().mode() & 0o111 != 0
                        };
                        #[cfg(not(unix))]
                        let executable = false;
                        let mut bytes = Vec::new();
                        file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
                        if bytes.len() as u64 > MAX_FILE || total + bytes.len() > MAX_SNAPSHOT {
                            skipped.push(json!({"root":root,"path":name,"reason":"file or snapshot byte budget"}));
                            continue;
                        }
                        let object = self.object(&bytes)?;
                        total += bytes.len();
                        files.push(
                            json!({"root":root,"path":name,"sha256":object,"bytes":bytes.len(),"executable":executable}),
                        );
                    }
                    _ => skipped.push(
                        json!({"root":root,"path":name,"reason":"missing, unreadable or non-file"}),
                    ),
                }
            }
        }
        let manifest = serde_json::to_vec(&json!({"started_at_unix_ms":started,
            "finished_at_unix_ms":crate::run_summary::now_ms(), "files":files, "skipped":skipped,
            "limitations":["tracked and non-ignored files only; not an atomic worktree or runtime snapshot"]}))?;
        let hash = self.object(&manifest)?;
        self.event(
            "file_snapshot",
            json!({"manifest_sha256":hash,"files":files.len(),"skipped":skipped.len()}),
        )
    }
}

fn read_events(directory: &Path) -> Result<(Vec<Event>, Vec<String>)> {
    let mut paths = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    let mut events = Vec::new();
    let mut gaps = Vec::new();
    for path in paths {
        let bytes = fs::read(&path)?;
        let mut closed = false;
        let mut lines = bytes.split_inclusive(|byte| *byte == b'\n').peekable();
        while let Some(line) = lines.next() {
            if !line.ends_with(b"\n") && lines.peek().is_none() {
                gaps.push(format!(
                    "ignored interrupted final line in {}",
                    path.display()
                ));
                continue;
            }
            let event = serde_json::from_slice::<Event>(line)
                .with_context(|| format!("invalid corpus event in {}", path.display()))?;
            closed = event.kind == "capture_closed";
            events.push(event);
        }
        if !closed {
            gaps.push(format!(
                "{}: capture segment is live or interrupted; final outcome may be unknown",
                path.display()
            ));
        }
    }
    events.sort_by_key(|event| event.sequence);
    if events.first().is_some_and(|event| event.sequence != 1) {
        gaps.push("initial event segment missing".into());
    }
    for pair in events.windows(2) {
        if pair[0].sequence >= pair[1].sequence {
            bail!("duplicate corpus event sequence");
        }
        if pair[0].sequence + 1 != pair[1].sequence {
            gaps.push(format!(
                "missing event sequences between {} and {}",
                pair[0].sequence, pair[1].sequence
            ));
        }
    }
    if events.iter().any(|event| {
        event.schema_version != 1
            || directory
                .file_name()
                .is_none_or(|name| name != event.run_id.as_str())
    }) {
        bail!("corpus event version or run identity mismatch");
    }
    Ok((events, gaps))
}

#[derive(Parser)]
#[command(
    name = "opsx-build corpus",
    about = "Export locally captured progress evidence; never invokes an agent"
)]
struct Args {
    #[command(subcommand)]
    command: CorpusCommand,
}

#[derive(Subcommand)]
enum CorpusCommand {
    /// Export all retained runs, or one selected run, into a new local directory.
    Export {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        run: Option<String>,
    },
}

pub fn run_cli() -> Result<Option<i32>> {
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_none_or(|arg| arg != "corpus") {
        return Ok(None);
    }
    let Args {
        command: CorpusCommand::Export { repo, output, run },
    } = Args::parse_from(std::iter::once(args[0].clone()).chain(args.into_iter().skip(2)));
    let ui = crate::ui::TerminalUi::new(false, false, false);
    let root = crate::git::metadata_dir(&repo, &ui)?.join("corpus");
    export(&root, &output, run.as_deref())?;
    println!(
        "Exported local corpus to {} (unlabelled; not cleared for training or publication)",
        output.display()
    );
    Ok(Some(0))
}

fn write_jsonl(path: &Path, records: &[Value]) -> Result<()> {
    let mut file = File::create(path)?;
    for record in records {
        serde_json::to_writer(&mut file, record)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

fn export(root: &Path, output: &Path, selected: Option<&str>) -> Result<()> {
    if output.exists() {
        bail!("export destination already exists: {}", output.display());
    }
    let mut directories = fs::read_dir(root)
        .context("no corpus capture exists for this repository")?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir() && selected.is_none_or(|run| path.file_name().is_some_and(|n| n == run))
        })
        .collect::<Vec<_>>();
    directories.sort();
    if directories.is_empty() {
        bail!("no matching captured corpus runs");
    }
    // A sibling temporary directory prevents failed exports from looking complete.
    let temporary = output.with_file_name(format!(".opsx-corpus-export-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o700))?;
    }
    let result = export_into(&directories, &temporary)
        .and_then(|()| fs::rename(&temporary, output).map_err(Into::into));
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

fn export_into(directories: &[PathBuf], output: &Path) -> Result<()> {
    fs::create_dir(output.join("objects"))?;
    let mut all = Vec::new();
    let mut decisions = Vec::new();
    let mut inputs = Vec::new();
    let mut outcomes = Vec::new();
    let mut gaps = Vec::new();
    for directory in directories {
        let (events, warnings) = read_events(directory)?;
        gaps.extend(warnings);
        if let Ok(error) = fs::read_to_string(directory.join("capture-error.txt")) {
            gaps.push(error);
        }
        let mut previous = 0;
        let mut task = Value::Null;
        let mut snapshot = Value::Null;
        let mut baseline_snapshot = Value::Null;
        let mut review = Value::Null;
        let mut baseline_review = Value::Null;
        let mut scope = Value::Null;
        let mut objects = BTreeSet::new();
        let mut manifests = BTreeSet::new();
        let mut copied = BTreeSet::new();
        let mut window = Vec::new();
        for event in &events {
            let value = serde_json::to_value(event)?;
            for key in ["manifest_sha256", "artifact_sha256"] {
                if let Some(hash) = event.data[key].as_str() {
                    objects.insert(hash.to_owned());
                }
            }
            if let Some(hash) = event.data["manifest_sha256"].as_str() {
                manifests.insert(hash.to_owned());
            }
            all.push(value.clone());
            if event.kind.ends_with("_outcome") {
                outcomes.push(value);
                continue;
            }
            if event.kind == "decision" {
                let id = format!("{}:{}", event.run_id, event.sequence);
                let input = json!({"decision_id":id, "cutoff_sequence":event.sequence,
                    "recorded_at_unix_ms":event.recorded_at_unix_ms, "context":event.request,
                    "task":task, "file_snapshot":snapshot, "events":window,"check_scope":scope,
                    "baseline":{"cutoff_sequence":previous,"file_snapshot":baseline_snapshot,"review":baseline_review},
                    "latest_review":review,
                    "operating_state":event.data["state"].as_object().map(|fields|fields.iter().filter(|(key,_)|matches!(key.as_str(),"active_elapsed_known_ms"|"policy"|"no_progress_reviews"|"stage_requests")).map(|(k,v)|(k.clone(),v.clone())).collect::<serde_json::Map<String,Value>>()),
                    "limitations":["event sequence defines observation order; timestamps are not independent clock attestations", "tool outputs and reviewer findings are observations, not human ground truth"]});
                let later = events
                    .iter()
                    .filter(|later| {
                        later.sequence > event.sequence
                            && later.kind.ends_with("_outcome")
                            && (later.request.is_none()
                                || later.request.as_ref().and_then(|r| r.get("request_id"))
                                    == event.request.as_ref().and_then(|r| r.get("request_id")))
                    })
                    .map(|later| later.sequence)
                    .collect::<Vec<_>>();
                decisions.push(
                    json!({"schema_version":1, "decision_id":id, "run_id":event.run_id,
                    "previous_cutoff_sequence":previous, "input":input,
                    "observed_policy":event.data, "later_outcome_sequences":later,
                    "annotation":null,"training_eligible":false,
                    "split":{"run_group":event.run_id,"family":null,"assignment":"unassigned"}}),
                );
                inputs.push(input);
                previous = event.sequence;
                baseline_snapshot = snapshot.clone();
                baseline_review = review.clone();
                window.clear();
            } else {
                if matches!(
                    event.kind.as_str(),
                    "request_started" | "capture_started" | "stage_boundary"
                ) {
                    task = Value::Null;
                }
                if event.kind == "task" {
                    task = serde_json::to_value(event)?;
                }
                if event.kind == "file_snapshot" {
                    snapshot = serde_json::to_value(event)?;
                }
                if event.kind == "review_record" {
                    review = serde_json::to_value(event)?;
                }
                if event.kind == "check_scope" {
                    scope = event.data.clone();
                }
                // Model/provider metadata and outcomes stay outside the judge view.
                if !matches!(
                    event.kind.as_str(),
                    "request_started" | "capture_started" | "capture_closed"
                ) {
                    window.push(value);
                }
            }
        }
        while let Some(name) = objects.pop_first() {
            if !copied.insert(name.clone()) {
                continue;
            }
            if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid corpus object reference");
            }
            let path = directory.join("objects").join(&name);
            let bytes = fs::read(&path)?;
            if digest(&bytes) != name {
                bail!("corrupt corpus object {}", path.display());
            }
            // Snapshot manifests reference the retained source contents. Do not
            // copy orphan objects from a later/incomplete concurrent capture.
            if manifests.contains(&name)
                && let Ok(manifest) = serde_json::from_slice::<Value>(&bytes)
                && let Some(files) = manifest["files"].as_array()
            {
                for file in files {
                    if let Some(hash) = file["sha256"].as_str() {
                        objects.insert(hash.to_owned());
                    }
                }
            }
            let target = output.join("objects").join(&name);
            if !target.exists() {
                fs::write(target, bytes)?;
            }
        }
    }
    write_jsonl(&output.join("events.jsonl"), &all)?;
    write_jsonl(&output.join("decisions.jsonl"), &decisions)?;
    write_jsonl(&output.join("model-inputs.jsonl"), &inputs)?;
    write_jsonl(&output.join("outcomes.jsonl"), &outcomes)?;
    fs::write(
        output.join("schema.json"),
        include_str!("../assets/corpus.schema.json"),
    )?;
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&json!({"schema_version":1,
        "runner_version":env!("CARGO_PKG_VERSION"),"runs":directories.len(),"decisions":decisions.len(),
        "events":all.len(),"capture_gaps":gaps,"content_hash":"SHA-256",
        "training_eligible":false,"annotations":"not supplied; observed policy actions are not labels",
        "privacy":"local project contents and visible tool data; review before external use"}))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
