//! Passive accounting. Never changes a request, invokes a model, or gates a stage.
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    hash::{DefaultHasher, Hash, Hasher},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::ui::Ui;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageContext {
    pub stage: Option<String>,
    pub stage_label: Option<String>,
    pub change: Option<String>,
    pub iteration: Option<u32>,
}

/// Input includes cache reads/writes; reasoning is a subset of output.
/// None means unreported, never zero. Partial observations are labelled separately.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Tokens {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

impl Tokens {
    fn zero() -> Self {
        Self {
            input: Some(0),
            output: Some(0),
            cache_read: Some(0),
            cache_write: Some(0),
            reasoning: Some(0),
        }
    }

    fn add(&mut self, other: &Self) {
        for (a, b) in self.fields().into_iter().zip(other.values()) {
            *a = a.zip(b).map(|(a, b)| a.saturating_add(b));
        }
    }

    fn fields(&mut self) -> [&mut Option<u64>; 5] {
        [
            &mut self.input,
            &mut self.output,
            &mut self.cache_read,
            &mut self.cache_write,
            &mut self.reasoning,
        ]
    }

    fn values(&self) -> [Option<u64>; 5] {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            self.reasoning,
        ]
    }

    fn delta(&self, previous: &Self) -> Self {
        let mut result = Self::default();
        for ((target, current), old) in result
            .fields()
            .into_iter()
            .zip(self.values())
            .zip(previous.values())
        {
            *target = current.zip(old).and_then(|(a, b)| a.checked_sub(b));
        }
        result
    }

    fn invalidate_decreases(&mut self, previous: &Self) -> bool {
        let mut decreased = false;
        for (current, old) in self.fields().into_iter().zip(previous.values()) {
            if current.zip(old).is_some_and(|(a, b)| a < b) {
                *current = None;
                decreased = true;
            }
        }
        decreased
    }

    fn codex(value: &Value) -> Self {
        Self {
            input: number(value, "inputTokens"),
            output: number(value, "outputTokens"),
            cache_read: number(value, "cachedInputTokens"),
            cache_write: number(value, "cacheWriteInputTokens"),
            reasoning: number(value, "reasoningOutputTokens"),
        }
    }

    fn claude(value: &Value, camel: bool) -> Self {
        let (input, output, read, write) = if camel {
            (
                "inputTokens",
                "outputTokens",
                "cacheReadInputTokens",
                "cacheCreationInputTokens",
            )
        } else {
            (
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            )
        };
        let cache_read = number(value, read);
        let cache_write = number(value, write);
        Self {
            input: number(value, input)
                .zip(cache_read)
                .zip(cache_write)
                .map(|((i, r), w)| i.saturating_add(r).saturating_add(w)),
            output: number(value, output),
            cache_read,
            cache_write,
            reasoning: value
                .pointer("/output_tokens_details/thinking_tokens")
                .and_then(Value::as_u64),
        }
    }
}

fn number(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(Value::as_u64)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub tokens: Tokens,
    pub models: BTreeMap<String, Tokens>,
    pub models_reported: bool,
    pub reported_cost_usd: Option<f64>,
}

impl Snapshot {
    fn zero() -> Self {
        Self {
            tokens: Tokens::zero(),
            models: BTreeMap::new(),
            models_reported: true,
            reported_cost_usd: Some(0.0),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsage {
    pub id: String,
    pub name: String,
    pub fingerprint: Option<String>,
    pub repeated: bool,
    pub completed: bool,
    pub failed: Option<bool>,
    pub exit_code: Option<i64>,
    pub result_chars: Option<usize>,
    pub truncated: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub schema_version: u32,
    pub id: String,
    pub invocation_id: String,
    pub attempt: u32,
    pub repo: PathBuf,
    pub context: UsageContext,
    pub backend: String,
    pub connection: Option<String>,
    pub requested_model: Option<String>,
    pub reported_models: Vec<String>,
    pub session_id: String,
    pub activity: String,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
    pub outcome: String,
    pub tokens: Tokens,
    pub reported_cost_usd: Option<f64>,
    pub usage_scope: String,
    pub notes: Vec<String>,
    pub tool_events_observed: bool,
    pub tools: Vec<ToolUsage>,
    pub cumulative: Option<Snapshot>,
}

/// One backend attempt, including partial data if the backend returns an error.
pub(crate) struct Attempt<'a, U: Ui> {
    ui: &'a U,
    pub record: UsageRecord,
    started: Instant,
    baseline: Option<Snapshot>,
    fresh: bool,
    partial_baseline: bool,
    seen: HashSet<String>,
    fingerprints: HashSet<String>,
    messages: BTreeMap<String, Tokens>,
    finished: bool,
}

pub(crate) struct Identity<'a> {
    pub backend: &'a str,
    pub connection: Option<&'a str>,
    pub model: Option<&'a str>,
    pub session: &'a str,
    pub invocation: &'a str,
    pub attempt: u32,
    pub fresh: bool,
}

impl<'a, U: Ui> Attempt<'a, U> {
    pub fn new(ui: &'a U, repo: &Path, identity: Identity<'_>, activity: &str) -> Self {
        let baseline = if identity.fresh {
            Some(Snapshot::zero())
        } else {
            ui.usage_baseline(repo, identity.backend, identity.session)
        };
        Self {
            ui,
            started: Instant::now(),
            baseline,
            fresh: identity.fresh,
            partial_baseline: false,
            seen: HashSet::new(),
            fingerprints: HashSet::new(),
            messages: BTreeMap::new(),
            finished: false,
            record: UsageRecord {
                schema_version: 1,
                id: Uuid::new_v4().to_string(),
                invocation_id: identity.invocation.to_owned(),
                attempt: identity.attempt,
                repo: repo.to_path_buf(),
                context: ui.usage_context(),
                backend: identity.backend.to_owned(),
                connection: identity.connection.map(str::to_owned),
                requested_model: identity.model.map(str::to_owned),
                reported_models: Vec::new(),
                session_id: identity.session.to_owned(),
                activity: activity.to_owned(),
                started_at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                elapsed_ms: 0,
                outcome: "interrupted".to_owned(),
                tokens: Tokens::default(),
                reported_cost_usd: None,
                usage_scope: "unavailable".to_owned(),
                notes: Vec::new(),
                tool_events_observed: false,
                tools: Vec::new(),
                cumulative: None,
            },
        }
    }

    fn note(&mut self, text: &str) {
        if !self.record.notes.iter().any(|n| n == text) {
            self.record.notes.push(text.to_owned());
        }
    }

    pub fn model(&mut self, model: Option<&str>) {
        if let Some(model) = model
            && !self.record.reported_models.iter().any(|m| m == model)
        {
            self.record.reported_models.push(model.to_owned());
        }
    }

    fn tool(&mut self, id: &str, name: &str, input: &Value) {
        if self.record.tools.iter().any(|t| t.id == id) {
            return;
        }
        // Grouping identifier only, not a cryptographic digest; arguments are never persisted.
        let fingerprint = (!input.is_null()).then(|| {
            let mut hasher = DefaultHasher::new();
            (name, input.to_string()).hash(&mut hasher);
            format!("{:016x}", hasher.finish())
        });
        let repeated = fingerprint
            .as_ref()
            .is_some_and(|key| !self.fingerprints.insert(key.clone()));
        self.record.tools.push(ToolUsage {
            id: id.to_owned(),
            name: name.to_owned(),
            fingerprint,
            repeated,
            completed: false,
            failed: None,
            exit_code: None,
            result_chars: None,
            truncated: None,
        });
    }

    fn tool_result(
        &mut self,
        id: &str,
        output: Option<&str>,
        failed: Option<bool>,
        exit: Option<i64>,
    ) {
        if let Some(tool) = self.record.tools.iter_mut().find(|t| t.id == id) {
            tool.completed = true;
            tool.failed = failed;
            tool.exit_code = exit;
            if let Some(output) = output {
                tool.result_chars = Some(output.chars().count());
                tool.truncated = Some(output.contains("Warning: truncated output"));
            }
        }
    }

    pub fn line(&mut self, line: &str) {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            self.event(&value);
        }
    }

    pub fn output(&mut self, output: &str) {
        if let Ok(value) = serde_json::from_str::<Value>(output) {
            self.event(&value);
        } else {
            for line in output.lines() {
                self.line(line);
            }
        }
    }

    pub fn event(&mut self, event: &Value) {
        let session = match self.record.backend.as_str() {
            "claude" if event["parent_tool_use_id"].is_null() => event["session_id"].as_str(),
            "opencode" => event["sessionID"].as_str(),
            _ => None,
        };
        if let Some(session) = session.filter(|id| *id != self.record.session_id) {
            self.record.session_id = session.to_owned();
            if !self.fresh {
                self.baseline =
                    self.ui
                        .usage_baseline(&self.record.repo, &self.record.backend, session);
            }
        }
        match self.record.backend.as_str() {
            "claude" => self.claude(event),
            "codex" => self.codex(event),
            "opencode" => self.opencode(event),
            _ => {}
        }
    }

    fn claude(&mut self, event: &Value) {
        self.model(event.get("model").and_then(Value::as_str));
        match event.get("type").and_then(Value::as_str) {
            Some("system" | "assistant" | "user") => {
                self.record.tool_events_observed = true;
                let message = &event["message"];
                self.model(message.get("model").and_then(Value::as_str));
                if let Some(id) = message.get("id").and_then(Value::as_str)
                    && let Some(usage) = message.get("usage")
                {
                    let mut tokens = Tokens::claude(usage, false);
                    // Assistant output usage can be a placeholder; only results finalize it.
                    tokens.output = None;
                    tokens.reasoning = None;
                    self.messages.insert(id.to_owned(), tokens);
                }
                for block in message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match block.get("type").and_then(Value::as_str) {
                        Some("tool_use") => {
                            if let (Some(id), Some(name)) =
                                (block["id"].as_str(), block["name"].as_str())
                            {
                                self.tool(id, name, &block["input"]);
                            }
                        }
                        Some("tool_result") => {
                            if let Some(id) = block["tool_use_id"].as_str() {
                                let content = text_content(&block["content"]);
                                self.tool_result(
                                    id,
                                    content.as_deref(),
                                    block["is_error"].as_bool(),
                                    None,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("result") => {
                let key = event
                    .get("uuid")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| event.to_string());
                if !self.seen.insert(key) {
                    return;
                }
                self.record.tokens = Tokens::default();
                self.record.reported_cost_usd = None;
                self.record.usage_scope = "unavailable".to_owned();
                let mut snapshot = Snapshot::default();
                if let Some(models) = event.get("modelUsage").and_then(Value::as_object) {
                    snapshot.models_reported = !models.is_empty();
                    for (name, usage) in models {
                        self.model(Some(name));
                        snapshot
                            .models
                            .insert(name.clone(), Tokens::claude(usage, true));
                    }
                }
                snapshot.reported_cost_usd = event
                    .get("total_cost_usd")
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite() && *v >= 0.0);
                let mut decreased = false;
                if let Some(base) = &self.baseline {
                    if base
                        .models
                        .keys()
                        .any(|name| !snapshot.models.contains_key(name))
                    {
                        snapshot.models_reported = false;
                        decreased = true;
                    }
                    for (name, tokens) in &mut snapshot.models {
                        if let Some(previous) = base.models.get(name) {
                            decreased |= tokens.invalidate_decreases(previous);
                        }
                    }
                    if snapshot
                        .reported_cost_usd
                        .zip(base.reported_cost_usd)
                        .is_some_and(|(a, b)| a < b)
                    {
                        snapshot.reported_cost_usd = None;
                        decreased = true;
                    }
                    if snapshot.models_reported && base.models_reported {
                        self.record.tokens = Tokens::zero();
                        for (name, usage) in &snapshot.models {
                            self.record.tokens.add(
                                &usage.delta(base.models.get(name).unwrap_or(&Tokens::zero())),
                            );
                        }
                        self.record.usage_scope = "session_tree_delta".to_owned();
                    } else if self.fresh
                        && let Some(usage) = event.get("usage")
                    {
                        // Legacy result usage excludes subagents and covers only the latest turn.
                        self.record.tokens = Tokens::claude(usage, false);
                        self.record.usage_scope = "main_agent_last_result".to_owned();
                    }
                    self.record.reported_cost_usd = snapshot
                        .reported_cost_usd
                        .zip(base.reported_cost_usd)
                        .and_then(|(a, b)| (a >= b).then_some(a - b));
                } else {
                    self.note("Resumed session has no recorded cumulative baseline; prior spend is not charged to this attempt.");
                }
                if decreased {
                    self.note("Cumulative counters decreased or lost models; affected baselines are unknown until the next report.");
                }
                self.record.cumulative = Some(snapshot);
            }
            _ => {}
        }
    }

    fn codex(&mut self, event: &Value) {
        self.record.tool_events_observed = true;
        match event.get("method").and_then(Value::as_str) {
            Some("thread/tokenUsage/updated") => {
                if let Some(total) = event.pointer("/params/tokenUsage/total") {
                    let mut current = Tokens::codex(total);
                    if let Some(base) = &self.baseline {
                        let decreased = current.invalidate_decreases(&base.tokens);
                        self.record.tokens = current.delta(&base.tokens);
                        self.record.usage_scope = if self.partial_baseline {
                            "partial_thread_delta"
                        } else {
                            "thread_delta"
                        }
                        .to_owned();
                        if decreased {
                            self.note(
                                "Cumulative counters decreased; affected differences are unknown.",
                            );
                        }
                    } else {
                        self.baseline = Some(Snapshot {
                            tokens: current.clone(),
                            ..Snapshot::default()
                        });
                        self.partial_baseline = true;
                        self.note("No cumulative baseline: usage before the first observed update is unknown.");
                        self.record.usage_scope = "partial_thread_delta".to_owned();
                    }
                    self.record.cumulative = Some(Snapshot {
                        tokens: current,
                        ..Snapshot::default()
                    });
                }
            }
            Some("model/rerouted") => {
                self.model(event.pointer("/params/toModel").and_then(Value::as_str))
            }
            Some("item/started" | "item/completed") => {
                let item = &event["params"]["item"];
                let Some(id) = item["id"].as_str() else {
                    return;
                };
                let Some(kind) = item["type"].as_str() else {
                    return;
                };
                if matches!(
                    kind,
                    "commandExecution"
                        | "mcpToolCall"
                        | "dynamicToolCall"
                        | "fileChange"
                        | "webSearch"
                        | "imageGeneration"
                        | "collabAgentToolCall"
                ) {
                    let input = match kind {
                        "commandExecution" => {
                            serde_json::json!({"command": item.get("command"), "cwd": item.get("cwd")})
                        }
                        _ => {
                            let fields = [
                                "server",
                                "tool",
                                "arguments",
                                "changes",
                                "query",
                                "action",
                                "prompt",
                            ];
                            let fields: serde_json::Map<String, Value> = fields
                                .into_iter()
                                .filter_map(|key| {
                                    item.get(key)
                                        .filter(|v| !v.is_null())
                                        .map(|value| (key.to_owned(), value.clone()))
                                })
                                .collect();
                            if fields.is_empty() {
                                Value::Null
                            } else {
                                Value::Object(fields)
                            }
                        }
                    };
                    self.tool(id, kind, &input);
                    if event["method"] == "item/completed" {
                        let exit = item["exitCode"].as_i64();
                        let failed = exit.map(|v| v != 0).or_else(|| {
                            item["status"]
                                .as_str()
                                .map(|s| matches!(s, "failed" | "declined" | "interrupted"))
                        });
                        let output = item["aggregatedOutput"]
                            .as_str()
                            .map(str::to_owned)
                            .or_else(|| text_content(&item["result"]["content"]))
                            .or_else(|| text_content(&item["content"]));
                        self.tool_result(id, output.as_deref(), failed, exit);
                    }
                }
            }
            _ => {}
        }
    }

    fn opencode(&mut self, event: &Value) {
        self.record.tool_events_observed = true;
        let part = &event["part"];
        self.model(
            part.get("modelID")
                .or_else(|| event.get("modelID"))
                .and_then(Value::as_str),
        );
        match event["type"].as_str() {
            Some("step_finish") => {
                let Some(id) = part["id"].as_str() else {
                    self.note("OpenCode step omitted its ID; usage cannot be deduplicated safely.");
                    return;
                };
                if !self.seen.insert(id.to_owned()) {
                    return;
                }
                if self.record.usage_scope == "unavailable" {
                    self.record.tokens = Tokens::zero();
                    self.record.reported_cost_usd = Some(0.0);
                }
                let tokens = &part["tokens"];
                let read = tokens.pointer("/cache/read").and_then(Value::as_u64);
                let write = tokens.pointer("/cache/write").and_then(Value::as_u64);
                self.record.tokens.add(&Tokens {
                    input: number(tokens, "input")
                        .zip(read)
                        .zip(write)
                        .map(|((i, r), w)| i.saturating_add(r).saturating_add(w)),
                    // OpenCode reports visible output separately from reasoning.
                    output: number(tokens, "output")
                        .zip(number(tokens, "reasoning"))
                        .map(|(o, r)| o.saturating_add(r)),
                    cache_read: read,
                    cache_write: write,
                    reasoning: number(tokens, "reasoning"),
                });
                self.record.reported_cost_usd = self
                    .record
                    .reported_cost_usd
                    .zip(part["cost"].as_f64().filter(|v| v.is_finite() && *v >= 0.0))
                    .map(|(sum, cost)| sum + cost);
                self.record.usage_scope = "observed_steps".to_owned();
            }
            Some("tool_use") => {
                if let Some(id) = part
                    .get("callID")
                    .or_else(|| part.get("id"))
                    .and_then(Value::as_str)
                {
                    self.tool(
                        id,
                        part["tool"].as_str().unwrap_or("tool"),
                        &part["state"]["input"],
                    );
                    let status = part["state"]["status"].as_str();
                    if matches!(status, Some("completed" | "error")) {
                        let exit = part["state"]["metadata"]["exit"].as_i64();
                        self.tool_result(
                            id,
                            part["state"]["output"]
                                .as_str()
                                .or_else(|| part["state"]["error"].as_str()),
                            status.map(|s| s == "error" || exit.is_some_and(|code| code != 0)),
                            exit,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    pub fn finish(mut self, outcome: &str) {
        self.record.outcome = outcome.to_owned();
        self.flush();
    }

    fn flush(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.record.usage_scope == "unavailable" && !self.messages.is_empty() {
            self.record.tokens = Tokens::zero();
            for usage in self.messages.values() {
                self.record.tokens.add(usage);
            }
            self.record.usage_scope = "partial_assistant_input".to_owned();
            self.note("Assistant input observed without an attributable final total; output and full-tree usage remain unknown.");
        }
        self.record.elapsed_ms = self.started.elapsed().as_millis() as u64;
        self.ui.record_usage(&self.record);
    }
}

impl<U: Ui> Drop for Attempt<'_, U> {
    fn drop(&mut self) {
        self.flush();
    }
}

fn text_content(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned).or_else(|| {
        value.as_array().map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
    })
}

#[derive(Default)]
pub(crate) struct UsageLog {
    repos: HashMap<PathBuf, RepoLog>,
}

struct RepoLog {
    path: PathBuf,
    snapshots: HashMap<String, Snapshot>,
}

impl UsageLog {
    fn repo(&mut self, repo: &Path) -> std::io::Result<&mut RepoLog> {
        if !self.repos.contains_key(repo) {
            let output = Command::new("git")
                .args(["rev-parse", "--git-path", "opsx-build/usage.jsonl"])
                .current_dir(repo)
                .output()?;
            if !output.status.success() {
                return Err(std::io::Error::other("cannot locate usage metadata"));
            }
            let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            let path = if path.is_absolute() {
                path
            } else {
                repo.join(path)
            };
            let mut snapshots = HashMap::new();
            if let Ok(file) = fs::File::open(&path) {
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    if let Ok(record) = serde_json::from_str::<UsageRecord>(&line) {
                        let key = format!("{}:{}", record.backend, record.session_id);
                        if let Some(snapshot) = record.cumulative {
                            snapshots.insert(key, snapshot);
                        } else {
                            snapshots.remove(&key);
                        }
                    }
                }
            }
            self.repos
                .insert(repo.to_path_buf(), RepoLog { path, snapshots });
        }
        Ok(self.repos.get_mut(repo).expect("inserted above"))
    }

    pub fn baseline(&mut self, repo: &Path, backend: &str, session: &str) -> Option<Snapshot> {
        self.repo(repo)
            .ok()?
            .snapshots
            .get(&format!("{backend}:{session}"))
            .cloned()
    }

    pub fn fresh(&mut self, repo: &Path, backend: &str, session: &str) {
        if let Ok(log) = self.repo(repo) {
            log.snapshots
                .insert(format!("{backend}:{session}"), Snapshot::zero());
        }
    }

    pub fn append(&mut self, record: &UsageRecord) -> std::io::Result<()> {
        let log = self.repo(&record.repo)?;
        if let Some(snapshot) = &record.cumulative {
            log.snapshots.insert(
                format!("{}:{}", record.backend, record.session_id),
                snapshot.clone(),
            );
        } else {
            // A partial attempt may have spent tokens after the last snapshot.
            // Do not attribute that unknown interval to the following attempt.
            log.snapshots
                .remove(&format!("{}:{}", record.backend, record.session_id));
        }
        if let Some(parent) = log.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log.path)?
            .write_all(&line)
    }
}

pub(crate) fn summary(record: &UsageRecord) -> String {
    let count = |n: Option<u64>| n.map_or_else(|| "unknown".to_owned(), |n| n.to_string());
    let tools = if record.tool_events_observed {
        record.tools.len().to_string()
    } else {
        "unknown".to_owned()
    };
    let repeated = record.tools.iter().filter(|tool| tool.repeated).count();
    let repeats = if repeated == 0 {
        String::new()
    } else {
        format!(", {repeated} repeat candidates")
    };
    let cost = record
        .reported_cost_usd
        .map_or_else(String::new, |cost| format!(", estimated ${cost:.4}"));
    let coverage = match record.usage_scope.as_str() {
        "thread_delta" | "session_tree_delta" | "observed_steps" => "",
        "main_agent_last_result" => " (main agent's last result only)",
        "unavailable" => " (usage unavailable)",
        _ => " (partial usage)",
    };
    format!(
        "Usage (attempt {}): {} input ({} cached), {} output tokens; {tools} tool calls{repeats}{cost}{coverage}",
        record.attempt,
        count(record.tokens.input),
        count(record.tokens.cache_read),
        count(record.tokens.output),
    )
}

pub(crate) fn error_outcome(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<crate::process::PauseRequested>()
        .is_some()
    {
        "paused"
    } else if error
        .downcast_ref::<crate::process::WorkerEscalationRequested>()
        .is_some()
    {
        "escalated"
    } else if crate::backend::is_missing_terminal_result(error) {
        "missing_terminal_result"
    } else if error
        .downcast_ref::<crate::process::BackendStreamControlRequested>()
        .is_some()
    {
        "operator_control"
    } else {
        "error"
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::stream::{StreamControl, StreamItem};
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct RecordingUi {
        pub records: Mutex<Vec<UsageRecord>>,
        snapshots: Mutex<HashMap<String, Snapshot>>,
    }

    impl Ui for RecordingUi {
        fn usage_context(&self) -> UsageContext {
            UsageContext {
                stage: Some("Apply".into()),
                change: Some("0001-test".into()),
                iteration: Some(1),
                ..UsageContext::default()
            }
        }
        fn usage_baseline(&self, _: &Path, backend: &str, session: &str) -> Option<Snapshot> {
            self.snapshots
                .lock()
                .unwrap()
                .get(&format!("{backend}:{session}"))
                .cloned()
        }
        fn usage_session_started(&self, _: &Path, backend: &str, session: &str) {
            self.snapshots
                .lock()
                .unwrap()
                .insert(format!("{backend}:{session}"), Snapshot::zero());
        }
        fn record_usage(&self, record: &UsageRecord) {
            let key = format!("{}:{}", record.backend, record.session_id);
            if let Some(snapshot) = &record.cumulative {
                self.snapshots.lock().unwrap().insert(key, snapshot.clone());
            } else {
                self.snapshots.lock().unwrap().remove(&key);
            }
            self.records.lock().unwrap().push(record.clone());
        }
        fn banner(&self, _: &str) {}
        fn change_name(&self, _: Option<&str>) {}
        fn stage(&self, _: usize, _: usize, _: &str) {}
        fn info(&self, _: &str) {}
        fn warn(&self, _: &str) {}
        fn success(&self, _: &str) {}
        fn failure(&self, _: &str) {}
        fn command(&self, _: &str) {}
        fn debug(&self, _: &str) {}
        fn debug_prompt(&self, _: &str, _: &str) {}
        fn start_stream(&self, _: &str) {}
        fn poll_stream(&self) -> StreamControl {
            StreamControl::None
        }
        fn stream_message_sent(&self, _: &str) {}
        fn stream_item(&self, _: &StreamItem) {}
        fn finish_stream(&self, _: bool, _: &str) {}
        fn finish_dashboard(&self) {}
        fn output(&self, _: &str, _: &str) {}
        fn start_activity(&self, _: &str) -> Option<indicatif::ProgressBar> {
            None
        }
        fn finish_activity(&self, _: Option<indicatif::ProgressBar>, _: bool, _: &str) {}
    }

    fn attempt<'a>(ui: &'a RecordingUi, backend: &str, fresh: bool) -> Attempt<'a, RecordingUi> {
        Attempt::new(
            ui,
            Path::new("/repo"),
            Identity {
                backend,
                connection: Some("worker"),
                model: Some("preset"),
                session: "session",
                invocation: "invocation",
                attempt: 1,
                fresh,
            },
            "Applying change",
        )
    }

    fn codex_total(input: u64, output: u64) -> Value {
        json!({"method":"thread/tokenUsage/updated", "params":{"tokenUsage":{"total":{
            "inputTokens":input, "outputTokens":output, "cachedInputTokens":10,
            "reasoningOutputTokens":2
        }}}})
    }

    fn claude_result(id: &str, input: u64, output: u64, cost: f64) -> Value {
        json!({"type":"result", "uuid":id, "total_cost_usd":cost, "modelUsage":{
            "claude-test":{"inputTokens":input,"outputTokens":output,"cacheReadInputTokens":20,"cacheCreationInputTokens":5}
        }, "usage":{"input_tokens":99999}})
    }

    #[test]
    fn codex_cumulative_notifications_are_differenced_once_across_attempts() {
        let ui = RecordingUi::default();
        let mut first = attempt(&ui, "codex", true);
        first.event(&codex_total(100, 20));
        first.event(&codex_total(150, 30));
        first.event(&codex_total(150, 30));
        first.finish("provider_error");
        let mut second = attempt(&ui, "codex", false);
        second.event(&codex_total(200, 40));
        second.finish("ready");
        let records = ui.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tokens.input, Some(150));
        assert_eq!(records[1].tokens.input, Some(50));
        assert_eq!(records[1].tokens.output, Some(10));
        assert_eq!(records[1].tokens.cache_read, Some(0));
        assert_eq!(records[1].tokens.cache_write, None);
        assert_eq!(records[1].reported_cost_usd, None);
    }

    #[test]
    fn codex_unknown_baseline_and_counter_reset_never_charge_history() {
        let ui = RecordingUi::default();
        let mut unknown = attempt(&ui, "codex", false);
        unknown.event(&codex_total(10000, 1000));
        assert_eq!(unknown.record.tokens.input, None);
        unknown.event(&codex_total(10100, 1010));
        assert_eq!(unknown.record.tokens.input, Some(100));
        assert_eq!(unknown.record.usage_scope, "partial_thread_delta");
        unknown.finish("ready");
        let mut reset = attempt(&ui, "codex", false);
        reset.event(&codex_total(20, 5));
        assert_eq!(reset.record.tokens.input, None);
        assert_eq!(reset.record.tokens.output, None);
        assert!(!reset.record.notes.is_empty());
    }

    #[test]
    fn claude_latest_model_tree_total_includes_subagents_without_double_counting() {
        let ui = RecordingUi::default();
        let mut first = attempt(&ui, "claude", true);
        first.event(&claude_result("r1", 100, 10, 0.1));
        let mut result = claude_result("r2", 200, 20, 0.3);
        result["modelUsage"]["subagent-model"] = json!({"inputTokens":50,"outputTokens":5,"cacheReadInputTokens":0,"cacheCreationInputTokens":0});
        first.event(&result);
        first.output(&result.to_string());
        first.finish("ready");
        let mut second = attempt(&ui, "claude", false);
        result["uuid"] = json!("r3");
        result["total_cost_usd"] = json!(0.5);
        result["modelUsage"]["claude-test"]["inputTokens"] = json!(300);
        second.event(&result);
        second.finish("ready");
        let records = ui.records.lock().unwrap();
        assert_eq!(records[0].tokens.input, Some(275));
        assert_eq!(records[0].tokens.output, Some(25));
        assert_eq!(
            records[0].reported_models,
            ["claude-test", "subagent-model"]
        );
        assert_eq!(records[1].tokens.input, Some(100));
        assert_eq!(records[1].tokens.output, Some(0));
        assert!((records[1].reported_cost_usd.unwrap() - 0.2).abs() < 1e-10);
    }

    #[test]
    fn claude_resumed_session_without_baseline_omits_previous_spend() {
        let ui = RecordingUi::default();
        let mut unknown = attempt(&ui, "claude", false);
        unknown.event(&claude_result("r1", 100000, 1000, 25.0));
        assert_eq!(unknown.record.tokens.input, None);
        assert_eq!(unknown.record.reported_cost_usd, None);
        unknown.finish("ready");
        let mut next = attempt(&ui, "claude", false);
        next.event(&claude_result("r2", 100100, 1010, 25.1));
        assert_eq!(next.record.tokens.input, Some(100));
        assert!((next.record.reported_cost_usd.unwrap() - 0.1).abs() < 1e-10);
    }

    #[test]
    fn reset_or_empty_cumulative_results_do_not_rebill_old_usage_on_recovery() {
        let ui = RecordingUi::default();
        let mut first = attempt(&ui, "claude", true);
        first.event(&claude_result("first", 100, 10, 1.0));
        first.finish("ready");
        let mut reset = attempt(&ui, "claude", false);
        reset.event(
            &json!({"type":"result", "uuid":"reset", "total_cost_usd":0.0, "modelUsage":{}}),
        );
        assert_eq!(reset.record.tokens.input, None);
        reset.finish("error");
        let mut next = attempt(&ui, "claude", false);
        next.event(&claude_result("next", 150, 15, 1.5));
        assert_eq!(next.record.tokens.input, None);
        assert_eq!(next.record.reported_cost_usd, None);
        next.finish("ready");
        let mut recovered = attempt(&ui, "claude", false);
        recovered.event(&claude_result("recovered", 200, 20, 2.0));
        assert_eq!(recovered.record.tokens.input, Some(50));
        assert_eq!(recovered.record.reported_cost_usd, Some(0.5));

        let mut first = attempt(&ui, "codex", true);
        first.event(&codex_total(100, 10));
        first.finish("ready");
        let mut reset = attempt(&ui, "codex", false);
        reset.event(&codex_total(0, 0));
        reset.finish("error");
        let mut next = attempt(&ui, "codex", false);
        next.event(&codex_total(150, 15));
        assert_eq!(next.record.tokens.input, None);
        next.finish("ready");
        let mut recovered = attempt(&ui, "codex", false);
        recovered.event(&codex_total(200, 20));
        assert_eq!(recovered.record.tokens.input, Some(50));
    }

    #[test]
    fn missing_result_keeps_deduplicated_input_but_not_placeholder_output() {
        let ui = RecordingUi::default();
        {
            let mut partial = attempt(&ui, "claude", true);
            let message = json!({"type":"assistant", "message":{"id":"msg1","model":"actual",
                "usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":20,"cache_creation_input_tokens":5}}});
            partial.event(&message);
            partial.event(&message);
            // Drop simulates an early return before explicit completion.
        }
        let records = ui.records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, "interrupted");
        assert_eq!(records[0].tokens.input, Some(35));
        assert_eq!(records[0].tokens.output, None);
        assert_eq!(records[0].usage_scope, "partial_assistant_input");
        assert!(
            ui.usage_baseline(Path::new("/repo"), "claude", "session")
                .is_none()
        );
    }

    #[test]
    fn opencode_steps_deduplicate_and_normalize_reasoning_and_caches() {
        let ui = RecordingUi::default();
        let mut usage = attempt(&ui, "opencode", false);
        let step = json!({"type":"step_finish", "part":{"id":"step1", "cost":0.01,
            "tokens":{"input":10,"output":20,"reasoning":30,"cache":{"read":40,"write":5}}}});
        usage.event(&step);
        usage.output(&step.to_string());
        assert_eq!(usage.record.tokens.input, Some(55));
        assert_eq!(usage.record.tokens.output, Some(50));
        assert_eq!(usage.record.tokens.reasoning, Some(30));
        assert_eq!(usage.record.reported_cost_usd, Some(0.01));
        let mut missing = step.clone();
        missing["part"]["id"] = json!("step2");
        missing["part"]["tokens"]["cache"]["read"] = Value::Null;
        missing["part"]["cost"] = Value::Null;
        usage.event(&missing);
        usage.event(&json!({"type":"step_finish", "part":{"id":"step3", "tokens":step["part"]["tokens"], "cost":0.01}}));
        assert_eq!(usage.record.tokens.input, None);
        assert_eq!(usage.record.tokens.output, Some(150));
        assert_eq!(usage.record.tokens.cache_read, None);
        assert_eq!(usage.record.reported_cost_usd, None);
    }

    #[test]
    fn tool_observations_deduplicate_without_persisting_arguments_or_content() {
        let ui = RecordingUi::default();
        let mut usage = attempt(&ui, "codex", true);
        let start = json!({"method":"item/started","params":{"item":{
            "id":"tool1","type":"commandExecution","command":"cat private.txt","cwd":"/secret"}}});
        usage.event(&start);
        let mut completed = start.clone();
        completed["method"] = json!("item/completed");
        completed["params"]["item"]["exitCode"] = json!(1);
        completed["params"]["item"]["aggregatedOutput"] =
            json!("private content\nWarning: truncated output");
        usage.event(&completed);
        usage.event(&completed);
        let mut repeated = start.clone();
        repeated["params"]["item"]["id"] = json!("tool2");
        usage.event(&repeated);
        repeated["params"]["item"]["id"] = json!("tool3");
        repeated["params"]["item"]["cwd"] = json!("/different");
        usage.event(&repeated);
        assert_eq!(usage.record.tools.len(), 3);
        assert!(usage.record.tools[0].completed);
        assert_eq!(usage.record.tools[0].failed, Some(true));
        assert_eq!(usage.record.tools[0].truncated, Some(true));
        assert!(usage.record.tools[1].repeated);
        assert!(!usage.record.tools[2].repeated);
        let serialized = serde_json::to_string(&usage.record).unwrap();
        for secret in ["private.txt", "/secret", "private content", "/different"] {
            assert!(!serialized.contains(secret));
        }
    }

    #[test]
    fn unknown_tool_arguments_do_not_generate_false_repeat_candidates() {
        let ui = RecordingUi::default();
        let mut usage = attempt(&ui, "claude", true);
        for id in ["a", "b"] {
            usage.tool(id, "Bash", &Value::Null);
        }
        assert!(
            usage
                .record
                .tools
                .iter()
                .all(|t| !t.repeated && t.fingerprint.is_none())
        );
        assert_eq!(usage.record.tokens, Tokens::default());
        assert!(summary(&usage.record).contains("unknown input"));
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn journal_reloads_baselines_tolerates_bad_lines_and_invalidates_partial_attempts() {
        let repo = std::env::temp_dir().join(format!("opsx-usage-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let ui = RecordingUi::default();
        let mut usage = attempt(&ui, "codex", true);
        usage.event(&codex_total(100, 10));
        let mut record = usage.record.clone();
        record.repo = repo.clone();
        let mut log = UsageLog::default();
        log.append(&record).unwrap();
        let path = repo.join(".git/opsx-build/usage.jsonl");
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{bad record}\n")
            .unwrap();
        let mut reloaded = UsageLog::default();
        assert_eq!(
            reloaded
                .baseline(&repo, "codex", "session")
                .unwrap()
                .tokens
                .input,
            Some(100)
        );
        record.cumulative = None;
        reloaded.append(&record).unwrap();
        assert!(
            UsageLog::default()
                .baseline(&repo, "codex", "session")
                .is_none()
        );
        assert_eq!(fs::read_to_string(path).unwrap().lines().count(), 3);
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn worktree_uses_its_own_git_metadata_and_logging_failure_does_not_stop_ui() {
        let repo = std::env::temp_dir().join(format!("opsx-usage-tree-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "initial",
                "--allow-empty",
            ],
        );
        let worktree = repo.join("tree");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let mut log = UsageLog::default();
        let path = log.repo(&worktree).unwrap().path.clone();
        assert!(path.starts_with(repo.join(".git/worktrees")));
        let ui = RecordingUi::default();
        let mut record = attempt(&ui, "codex", true).record.clone();
        record.repo = worktree;
        log.append(&record).unwrap();
        assert!(path.is_file());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap(); // Opening a directory for append must fail.
        assert!(log.append(&record).is_err());
        let terminal = crate::ui::TerminalUi::new(false, false, false);
        terminal.record_usage(&record);
        terminal.record_usage(&record);
        fs::remove_dir_all(repo).unwrap();
    }
}
