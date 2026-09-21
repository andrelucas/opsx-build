use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    backend::{
        AgentBackend, SessionId, SessionMode, StageProtocol, StageResult, stage_result_from_text,
    },
    cli::{AgentConnection, BackendKind, ConnectionEnvironmentValue},
    codex_server::CodexServer,
    process::{CommandSpec, PauseRequested, WorkerEscalationRequested},
    stream::{StreamControl, StreamFilter, StreamItem},
    ui::Ui,
};

const PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_secs(5);
const PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LauncherEnvironment {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLauncher {
    pub connection_name: Option<String>,
    pub environment_name: Option<String>,
    pub program: String,
    pub prefix_args: Vec<String>,
    pub model: Option<String>,
    pub context_window: Option<u64>,
    pub auto_compact_window: Option<u64>,
    pub auto_compact_percent: Option<u8>,
    pub max_output_tokens: Option<u64>,
    pub(crate) environment: Vec<LauncherEnvironment>,
    pub(crate) unset_environment: Vec<String>,
}

impl CodexLauncher {
    pub fn from_connection(connection: &AgentConnection) -> Result<Self> {
        if connection.backend != BackendKind::Codex {
            bail!(
                "connection profile `{}` selects {} rather than the Codex backend",
                connection.name.as_deref().unwrap_or("unnamed"),
                connection.backend
            );
        }
        let mut words = split_command(&connection.command)?;
        if words.is_empty() {
            bail!("Codex connection command cannot be empty");
        }
        let mut unset_environment = connection.unset_env.clone();
        let mut environment = Vec::new();
        for (name, configured) in &connection.env {
            validate_environment_name(name)?;
            let (value, redacted) = match configured {
                ConnectionEnvironmentValue::Literal(value) => {
                    (value.clone(), environment_name_is_sensitive(name))
                }
                ConnectionEnvironmentValue::FromEnvironment(reference) => {
                    validate_environment_name(&reference.from_env)?;
                    let value = std::env::var(&reference.from_env).with_context(|| {
                        format!(
                            "Codex connection profile `{}` requires environment variable `{}` for `{name}`",
                            connection.name.as_deref().unwrap_or("unnamed"),
                            reference.from_env
                        )
                    })?;
                    if reference.from_env != name.as_str()
                        && !unset_environment.contains(&reference.from_env)
                    {
                        unset_environment.push(reference.from_env.clone());
                    }
                    (value, true)
                }
            };
            environment.push(LauncherEnvironment {
                name: name.clone(),
                value,
                redacted,
            });
        }
        Ok(Self {
            connection_name: connection.name.clone(),
            environment_name: connection.environment_name.clone(),
            program: words.remove(0),
            prefix_args: words,
            model: connection.model.clone(),
            context_window: connection.context_window,
            auto_compact_window: connection.auto_compact_window,
            auto_compact_percent: connection.auto_compact_percent,
            max_output_tokens: connection.max_output_tokens,
            environment,
            unset_environment,
        })
    }

    pub(crate) fn server_command(&self, repo: &Path) -> CommandSpec {
        let mut spec =
            CommandSpec::new(&self.program, repo).args(self.prefix_args.iter().cloned().chain([
                "app-server".to_owned(),
                "--listen".to_owned(),
                "stdio://".to_owned(),
            ]));
        for name in &self.unset_environment {
            spec = spec.remove_env(name);
        }
        for variable in &self.environment {
            spec = if variable.redacted {
                spec.secret_env(&variable.name, &variable.value)
            } else {
                spec.env(&variable.name, &variable.value)
            };
        }
        spec
    }

    pub(crate) fn thread_params(
        &self,
        repo: &Path,
        permission_mode: &str,
        additional_roots: &[PathBuf],
    ) -> Result<Value> {
        let (approval_policy, approvals_reviewer, sandbox) = codex_permissions(permission_mode)?;
        let mut roots = vec![repo.to_path_buf()];
        for root in additional_roots {
            if !roots.contains(root) {
                roots.push(root.clone());
            }
        }
        let mut params = json!({
            "cwd": repo,
            "approvalPolicy": approval_policy,
            "approvalsReviewer": approvals_reviewer,
            "sandbox": sandbox,
            "runtimeWorkspaceRoots": roots,
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        if let Some(limit) = self.compaction_limit() {
            params["config"] = json!({"model_auto_compact_token_limit": limit});
        }
        Ok(params)
    }

    fn compaction_limit(&self) -> Option<u64> {
        self.auto_compact_window.or_else(|| {
            self.context_window
                .zip(self.auto_compact_percent)
                .map(|(window, percentage)| window.saturating_mul(u64::from(percentage)) / 100)
        })
    }

    fn turn_sandbox_policy(
        &self,
        repo: &Path,
        permission_mode: &str,
        additional_roots: &[PathBuf],
    ) -> Result<Value> {
        let (_, _, sandbox) = codex_permissions(permission_mode)?;
        Ok(match sandbox {
            "read-only" => json!({"type": "readOnly", "networkAccess": false}),
            "danger-full-access" => json!({"type": "dangerFullAccess"}),
            "workspace-write" => {
                let mut roots = vec![repo.to_path_buf()];
                for root in additional_roots {
                    if !roots.contains(root) {
                        roots.push(root.clone());
                    }
                }
                json!({
                    "type": "workspaceWrite",
                    "writableRoots": roots,
                    "networkAccess": false
                })
            }
            _ => unreachable!("codex_permissions returned an unknown sandbox"),
        })
    }
}

pub fn build_interactive_codex_command(
    repo: &Path,
    launcher: &CodexLauncher,
    permission_mode: &str,
    interactive_args: &[String],
    initial_prompt: Option<&str>,
) -> Result<CommandSpec> {
    let mut args = launcher.prefix_args.clone();
    args.extend(["--cd".to_owned(), repo.display().to_string()]);
    if let Some(model) = &launcher.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    match permission_mode {
        "auto" | "acceptEdits" | "default" => args.push("--approve-for-me".to_owned()),
        "dontAsk" => args.extend([
            "--sandbox".to_owned(),
            "workspace-write".to_owned(),
            "--ask-for-approval".to_owned(),
            "never".to_owned(),
        ]),
        "plan" => args.extend([
            "--sandbox".to_owned(),
            "read-only".to_owned(),
            "--ask-for-approval".to_owned(),
            "never".to_owned(),
        ]),
        "bypassPermissions" => args.push("--dangerously-bypass-approvals-and-sandbox".to_owned()),
        other => bail!(
            "Claude permission mode `{other}` has no Codex mapping; use auto, acceptEdits, default, dontAsk, plan, or bypassPermissions"
        ),
    }
    args.extend(interactive_args.iter().cloned());
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_owned());
    }
    Ok(with_launcher_environment(
        CommandSpec::new(&launcher.program, repo).args(args),
        launcher,
    ))
}

#[derive(Debug)]
struct CodexTurnError {
    message: String,
    transient: bool,
    output_limit: bool,
}

impl fmt::Display for CodexTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CodexTurnError {}

enum TurnOutcome {
    Complete(StageResult),
    Control(StreamControl, Option<String>),
}

pub struct CodexBackend<'a, U: Ui> {
    repo: &'a Path,
    launcher: &'a CodexLauncher,
    server: Arc<CodexServer>,
    permission_mode: &'a str,
    stream_filter: Option<StreamFilter>,
    max_output_retries: u32,
    max_provider_retries: u32,
    provider_retry_base_delay: Duration,
    stage_timeout: Option<Duration>,
    additional_roots: Vec<PathBuf>,
    resume_repo: Option<PathBuf>,
    persist_threads: bool,
    ui: &'a U,
}

impl<'a, U: Ui> CodexBackend<'a, U> {
    pub fn new(
        repo: &'a Path,
        launcher: &'a CodexLauncher,
        permission_mode: &'a str,
        stream_filter: Option<StreamFilter>,
        ui: &'a U,
    ) -> Self {
        Self {
            repo,
            launcher,
            server: Arc::new(CodexServer::new(repo, launcher)),
            permission_mode,
            stream_filter,
            max_output_retries: 0,
            max_provider_retries: 0,
            provider_retry_base_delay: PROVIDER_RETRY_BASE_DELAY,
            stage_timeout: None,
            additional_roots: Vec::new(),
            resume_repo: None,
            persist_threads: true,
            ui,
        }
    }

    pub(crate) fn with_server(mut self, server: Arc<CodexServer>) -> Self {
        self.server = server;
        self
    }

    pub fn with_retries(mut self, output_retries: u32, provider_retries: u32) -> Self {
        self.max_output_retries = output_retries;
        self.max_provider_retries = provider_retries;
        self
    }

    pub fn with_stage_timeout(mut self, timeout: Duration) -> Self {
        self.stage_timeout = Some(timeout);
        self
    }

    pub fn with_additional_dir(mut self, path: &Path) -> Self {
        self.additional_roots.push(path.to_path_buf());
        self
    }

    pub fn with_resume_repo(mut self, path: &Path) -> Self {
        self.resume_repo = Some(path.to_path_buf());
        self
    }

    pub fn without_session_persistence(mut self) -> Self {
        self.persist_threads = false;
        self
    }

    fn start_or_resume(&self, session: SessionMode) -> Result<SessionId> {
        let client = self.server.client(self.ui)?;
        match session {
            SessionMode::New { name, .. } => client
                .start_thread(
                    self.launcher,
                    self.repo,
                    self.permission_mode,
                    name.as_deref(),
                    &self.additional_roots,
                    !self.persist_threads,
                )
                .map(SessionId::new),
            SessionMode::Resume { id } => {
                client.ensure_thread(
                    self.launcher,
                    self.repo,
                    self.permission_mode,
                    id.as_str(),
                    &self.additional_roots,
                )?;
                Ok(id)
            }
        }
    }

    fn invoke_once(
        &self,
        session_id: &SessionId,
        prompt: &str,
        activity: &str,
        protocol: StageProtocol,
        timeout: Option<Duration>,
    ) -> Result<TurnOutcome> {
        let client = self.server.client(self.ui)?;
        self.ui.debug(&format!("Codex thread: {session_id}"));
        self.ui.debug_prompt(activity, prompt);
        let schema = serde_json::from_str(protocol.json_schema())
            .context("opsx-build stage schema is invalid JSON")?;
        let turn_id = client.start_turn(
            session_id.as_str(),
            codex_turn_input(self.repo, prompt),
            schema,
            self.launcher.turn_sandbox_policy(
                self.repo,
                self.permission_mode,
                &self.additional_roots,
            )?,
        )?;
        let started = Instant::now();
        let mut response = String::new();
        let mut context_report = None;
        let mut pending_control = None;
        self.ui.start_stream(activity);
        loop {
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                let _ = client.interrupt_turn(session_id.as_str(), &turn_id);
                self.ui.finish_stream(false, activity);
                return Err(WorkerEscalationRequested::new(format!(
                    "local worker exceeded its {} minute stage timeout",
                    timeout.unwrap().as_secs() / 60
                ))
                .into());
            }
            if let Some(event) = client.next_event(Duration::from_millis(50))? {
                if event.get("id").is_some()
                    && let Some(method) = event.get("method").and_then(Value::as_str)
                {
                    let _ = client.interrupt_turn(session_id.as_str(), &turn_id);
                    self.ui.finish_stream(false, activity);
                    bail!(
                        "Codex requested unsupported interactive client input through `{method}` during an unattended stage"
                    );
                }
                if event.pointer("/params/threadId").and_then(Value::as_str)
                    == Some(session_id.as_str())
                {
                    if event.get("method").and_then(Value::as_str) == Some("error")
                        && event.pointer("/params/turnId").and_then(Value::as_str)
                            == Some(turn_id.as_str())
                        && !event
                            .pointer("/params/willRetry")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                    {
                        let message = event
                            .pointer("/params/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("Codex turn failed")
                            .to_owned();
                        self.ui.finish_stream(false, activity);
                        return Err(classify_turn_error(message).into());
                    }
                    if let Some(report) = context_usage_report(&event) {
                        context_report = Some(report);
                    }
                    if let Some(text) = completed_agent_text(&event) {
                        response = text.to_owned();
                    }
                    if let Some(filter) = self.stream_filter {
                        for item in filter_codex_event(&event, filter) {
                            self.ui.stream_item(&item);
                        }
                    }
                    if event.get("method").and_then(Value::as_str) == Some("turn/completed")
                        && event.pointer("/params/turn/id").and_then(Value::as_str)
                            == Some(turn_id.as_str())
                    {
                        self.ui.finish_stream(pending_control.is_none(), activity);
                        if let Some(control) = pending_control {
                            return Ok(TurnOutcome::Control(control, context_report));
                        }
                        let status = event
                            .pointer("/params/turn/status")
                            .and_then(Value::as_str)
                            .unwrap_or("failed");
                        if status != "completed" {
                            let message = event
                                .pointer("/params/turn/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or("Codex turn did not complete")
                                .to_owned();
                            return Err(classify_turn_error(message).into());
                        }
                        if response.is_empty() {
                            response = event
                                .pointer("/params/turn/items")
                                .and_then(Value::as_array)
                                .and_then(|items| {
                                    items.iter().rev().find_map(|item| {
                                        (item.get("type").and_then(Value::as_str)
                                            == Some("agentMessage"))
                                        .then(|| item.get("text").and_then(Value::as_str))
                                        .flatten()
                                    })
                                })
                                .unwrap_or_default()
                                .to_owned();
                        }
                        return stage_result_from_text(
                            &response,
                            Some(session_id.to_string()),
                            "Codex",
                        )
                        .map(TurnOutcome::Complete);
                    }
                }
            }
            match self.ui.poll_stream() {
                StreamControl::None => {}
                StreamControl::Inject(message) => {
                    client.steer_turn(session_id.as_str(), &turn_id, &message)?;
                    self.ui
                        .stream_message_sent("Steering instruction sent to the active Codex turn");
                }
                control @ (StreamControl::Compact | StreamControl::Context) => {
                    if pending_control.is_some() {
                        self.ui
                            .warn("Another interrupting command is already in progress");
                        continue;
                    }
                    client.interrupt_turn(session_id.as_str(), &turn_id)?;
                    self.ui.stream_message_sent(match control {
                        StreamControl::Compact => {
                            "Codex acknowledged the compact request; waiting for active work to stop"
                        }
                        StreamControl::Context => {
                            "Codex acknowledged the context request; waiting for active work to stop"
                        }
                        _ => unreachable!(),
                    });
                    pending_control = Some(control);
                }
                StreamControl::Pause => {
                    let _ = client.interrupt_turn(session_id.as_str(), &turn_id);
                    self.ui.finish_stream(false, activity);
                    return Err(PauseRequested::new(
                        self.resume_repo
                            .clone()
                            .unwrap_or_else(|| self.repo.to_path_buf()),
                    )
                    .into());
                }
                StreamControl::Escalate => {
                    let _ = client.interrupt_turn(session_id.as_str(), &turn_id);
                    self.ui.finish_stream(false, activity);
                    return Err(WorkerEscalationRequested::new(
                        "frontier assistance requested from the terminal",
                    )
                    .into());
                }
                StreamControl::Interrupt => {
                    let _ = client.interrupt_turn(session_id.as_str(), &turn_id);
                    self.ui.finish_stream(false, activity);
                    bail!("Codex turn interrupted by user");
                }
            }
        }
    }
}

impl<U: Ui> AgentBackend for CodexBackend<'_, U> {
    fn name(&self) -> &'static str {
        "Codex"
    }

    fn invoke(
        &self,
        session: SessionMode,
        prompt: &str,
        activity: &str,
        protocol: StageProtocol,
    ) -> Result<StageResult> {
        let current_session = self.start_or_resume(session)?;
        let mut current_prompt = prompt.to_owned();
        let mut current_activity = activity.to_owned();
        let mut output_recoveries = 0;
        let mut provider_retries = 0;
        let deadline = self
            .stage_timeout
            .and_then(|timeout| Instant::now().checked_add(timeout));
        loop {
            let remaining = deadline.map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
            });
            match self.invoke_once(
                &current_session,
                &current_prompt,
                &current_activity,
                protocol,
                remaining,
            ) {
                Ok(TurnOutcome::Complete(result)) => return Ok(result),
                Ok(TurnOutcome::Control(StreamControl::Compact, _)) => {
                    self.server
                        .client(self.ui)?
                        .compact_thread(current_session.as_str())?;
                    self.ui.stream_item(&StreamItem::Lifecycle(format!(
                        "Codex compacted thread {current_session}"
                    )));
                    current_prompt = continuation_prompt(
                        protocol,
                        "The operator compacted this Codex thread while the phase was active.",
                    );
                    current_activity = format!("{activity} (after compaction)");
                }
                Ok(TurnOutcome::Control(StreamControl::Context, report)) => {
                    self.ui
                        .stream_item(&StreamItem::Lifecycle(report.unwrap_or_else(|| {
                            "Codex has not reported token usage for this turn yet".to_owned()
                        })));
                    current_prompt = continuation_prompt(
                        protocol,
                        "The operator interrupted the turn to inspect its context usage.",
                    );
                    current_activity = format!("{activity} (after context report)");
                }
                Ok(TurnOutcome::Control(_, _)) => unreachable!(),
                Err(error)
                    if error
                        .downcast_ref::<CodexTurnError>()
                        .is_some_and(|error| error.output_limit)
                        && output_recoveries < self.max_output_retries =>
                {
                    output_recoveries += 1;
                    self.ui.warn(&format!(
                        "Codex reached its output token limit; continuing the same phase ({output_recoveries}/{})",
                        self.max_output_retries
                    ));
                    current_prompt = continuation_prompt(
                        protocol,
                        "The preceding turn reached its output token limit.",
                    );
                    current_activity = format!(
                        "{activity} (output-limit continuation {output_recoveries}/{})",
                        self.max_output_retries
                    );
                }
                Err(error)
                    if error
                        .downcast_ref::<CodexTurnError>()
                        .is_some_and(|error| error.transient)
                        && provider_retries < self.max_provider_retries =>
                {
                    provider_retries += 1;
                    let delay =
                        provider_retry_delay(self.provider_retry_base_delay, provider_retries);
                    if remaining.is_some_and(|remaining| delay >= remaining) {
                        return Err(error.context(
                            "transient provider failure could not be retried before the stage timeout",
                        ));
                    }
                    self.ui.warn(&format!(
                        "Transient Codex provider failure; retrying the same thread in {} second(s) ({provider_retries}/{})",
                        delay.as_secs(),
                        self.max_provider_retries
                    ));
                    thread::sleep(delay);
                    current_prompt = continuation_prompt(
                        protocol,
                        "The preceding turn was interrupted by a transient provider or network failure.",
                    );
                    current_activity = format!(
                        "{activity} (provider recovery {provider_retries}/{})",
                        self.max_provider_retries
                    );
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn rename_session(&self, session_id: &SessionId, name: &str) -> Result<()> {
        let client = self.server.client(self.ui)?;
        client.ensure_thread(
            self.launcher,
            self.repo,
            self.permission_mode,
            session_id.as_str(),
            &self.additional_roots,
        )?;
        client.rename_thread(session_id.as_str(), name)
    }

    fn compact_session(&self, session_id: &SessionId, completed_phase: &str) -> Result<()> {
        let client = self.server.client(self.ui)?;
        client.ensure_thread(
            self.launcher,
            self.repo,
            self.permission_mode,
            session_id.as_str(),
            &self.additional_roots,
        )?;
        self.ui.info(&format!(
            "Hard-compacting Codex thread after {completed_phase}"
        ));
        client.compact_thread(session_id.as_str())?;
        self.ui
            .success(&format!("Compacted Codex thread {session_id}"));
        Ok(())
    }
}

fn codex_permissions(permission_mode: &str) -> Result<(&'static str, &'static str, &'static str)> {
    match permission_mode {
        "auto" | "acceptEdits" | "default" => Ok(("on-request", "auto_review", "workspace-write")),
        "dontAsk" => Ok(("never", "user", "workspace-write")),
        "plan" => Ok(("never", "user", "read-only")),
        "bypassPermissions" => Ok(("never", "user", "danger-full-access")),
        other => bail!(
            "Claude permission mode `{other}` has no Codex mapping; use auto, acceptEdits, default, dontAsk, plan, or bypassPermissions"
        ),
    }
}

fn completed_agent_text(event: &Value) -> Option<&str> {
    (event.get("method").and_then(Value::as_str) == Some("item/completed"))
        .then(|| event.pointer("/params/item"))
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("agentMessage"))
        .and_then(|item| item.get("text").and_then(Value::as_str))
}

fn codex_turn_input(repo: &Path, prompt: &str) -> Value {
    let trimmed = prompt.trim_start();
    let Some(after_slash) = trimmed.strip_prefix('/') else {
        return json!([{"type": "text", "text": prompt}]);
    };
    let command_end = after_slash
        .find(char::is_whitespace)
        .unwrap_or(after_slash.len());
    let name = &after_slash[..command_end];
    let remainder = after_slash[command_end..].trim_start();
    let command_path = name.replace(':', "/");
    let opencode_name = name.replace([':', '/'], "-");
    let skill_candidates = [
        repo.join(".agents/skills").join(name).join("SKILL.md"),
        repo.join(".claude/skills").join(name).join("SKILL.md"),
        repo.join(".opencode/skills").join(name).join("SKILL.md"),
    ];
    if let Some(path) = skill_candidates.into_iter().find(|path| path.is_file()) {
        return json!([
            {"type": "skill", "name": name, "path": path},
            {"type": "text", "text": remainder}
        ]);
    }
    let command_candidates = [
        repo.join(".claude/commands")
            .join(format!("{command_path}.md")),
        repo.join(".claude/commands").join(&command_path),
        repo.join(".opencode/commands")
            .join(format!("{opencode_name}.md")),
    ];
    let Some(path) = command_candidates.into_iter().find(|path| path.is_file()) else {
        return json!([{"type": "text", "text": prompt}]);
    };
    let Ok(instructions) = std::fs::read_to_string(path) else {
        return json!([{"type": "text", "text": prompt}]);
    };
    json!([{"type": "text", "text": format!(
        "Follow the installed `{name}` workflow instructions below as the authoritative procedure for this invocation.\n\n{instructions}\n\nASSIGNMENT\n\n{remainder}"
    )}])
}

fn context_usage_report(event: &Value) -> Option<String> {
    if event.get("method").and_then(Value::as_str) != Some("thread/tokenUsage/updated") {
        return None;
    }
    let usage = event.pointer("/params/tokenUsage")?;
    let input = usage.pointer("/last/inputTokens").and_then(Value::as_u64)?;
    let cached = usage
        .pointer("/last/cachedInputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .pointer("/last/outputTokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let window = usage.get("modelContextWindow").and_then(Value::as_u64);
    Some(window.map_or_else(
        || format!("Codex context: {input} input ({cached} cached), {output} output tokens"),
        |window| {
            let percentage = input.saturating_mul(100) / window.max(1);
            format!(
                "Codex context: {input}/{window} input tokens ({percentage}%, {cached} cached), {output} output tokens"
            )
        },
    ))
}

fn filter_codex_event(event: &Value, filter: StreamFilter) -> Vec<StreamItem> {
    if filter == StreamFilter::Raw {
        return vec![StreamItem::Raw(event.to_string())];
    }
    let method = event
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("event");
    match method {
        "item/completed" => {
            let Some(item) = event.pointer("/params/item") else {
                return Vec::new();
            };
            match item.get("type").and_then(Value::as_str) {
                Some("agentMessage") => item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| vec![StreamItem::Codex(text.to_owned())])
                    .unwrap_or_default(),
                Some("commandExecution") => {
                    let command = item
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("command");
                    let mut items = vec![StreamItem::Tool(format!("Shell: {}", one_line(command)))];
                    if filter == StreamFilter::Full
                        && let Some(output) = item.get("aggregatedOutput").and_then(Value::as_str)
                        && !output.trim().is_empty()
                    {
                        items.push(StreamItem::ToolResult(output.to_owned()));
                    }
                    items
                }
                Some("fileChange") => vec![StreamItem::Tool("Applied file changes".to_owned())],
                Some(kind) if filter == StreamFilter::Full => {
                    vec![StreamItem::Lifecycle(format!("Codex item: {kind}"))]
                }
                _ => Vec::new(),
            }
        }
        "thread/compacted" => vec![StreamItem::Lifecycle("Codex context compacted".to_owned())],
        "turn/completed" if filter == StreamFilter::Full => {
            let status = event
                .pointer("/params/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            vec![StreamItem::Lifecycle(format!("Codex turn: {status}"))]
        }
        _ => Vec::new(),
    }
}

fn one_line(value: &str) -> String {
    const LIMIT: usize = 240;
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= LIMIT {
        return compact;
    }
    let prefix = compact.chars().take(LIMIT - 1).collect::<String>();
    format!("{prefix}…")
}

fn continuation_prompt(protocol: StageProtocol, reason: &str) -> String {
    format!(
        "{reason} Continue the same opsx-build phase in this Codex thread from its durable repository and OpenSpec state. Preserve correct partial work, do not restart completed tasks or create a duplicate change, and finish the outstanding work now. Return the required structured result with `opsx_status` set to one of: {}.",
        protocol.terminal_values()
    )
}

fn classify_turn_error(message: String) -> CodexTurnError {
    let lowercase = message.to_ascii_lowercase();
    let output_limit = [
        "max_output_tokens",
        "max output tokens",
        "output token limit",
        "maximum output tokens",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker));
    let transient = [
        "rate limit",
        "rate_limit",
        "overloaded",
        "temporarily unavailable",
        "timeout",
        "timed out",
        "connection reset",
        "network error",
        "429",
        "500",
        "502",
        "503",
        "504",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker));
    CodexTurnError {
        message: format!("Codex turn failed: {message}"),
        transient,
        output_limit,
    }
}

fn provider_retry_delay(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(PROVIDER_RETRY_MAX_DELAY)
}

fn with_launcher_environment(spec: CommandSpec, launcher: &CodexLauncher) -> CommandSpec {
    let spec = launcher
        .unset_environment
        .iter()
        .fold(spec, |spec, name| spec.remove_env(name));
    launcher.environment.iter().fold(spec, |spec, variable| {
        if variable.redacted {
            spec.secret_env(&variable.name, &variable.value)
        } else {
            spec.env(&variable.name, &variable.value)
        }
    })
}

fn validate_environment_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid_start = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    if !valid_start
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        bail!("invalid environment variable name `{name}` in Codex connection profile");
    }
    Ok(())
}

fn environment_name_is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["TOKEN", "KEY", "SECRET", "PASSWORD"]
        .iter()
        .any(|marker| name.contains(marker))
}

fn split_command(command: &str) -> Result<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut started = false;
    for character in command.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match (quote, character) {
            (Quote::None | Quote::Double, '\\') => {
                escaped = true;
                started = true;
            }
            (Quote::None, '\'') => {
                quote = Quote::Single;
                started = true;
            }
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::None, '"') => {
                quote = Quote::Double;
                started = true;
            }
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, character) if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (_, character) => {
                word.push(character);
                started = true;
            }
        }
    }
    if escaped {
        bail!("Codex connection command ends with an incomplete escape");
    }
    if quote != Quote::None {
        bail!("Codex connection command contains an unterminated quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn resolves_codex_connection_and_command() {
        let connection = AgentConnection {
            name: Some("codex-worker".to_owned()),
            backend: BackendKind::Codex,
            environment_name: None,
            command: "codex --profile local".to_owned(),
            model: Some("gpt-5.6-codex".to_owned()),
            context_window: Some(200_000),
            auto_compact_window: None,
            auto_compact_percent: Some(75),
            max_output_tokens: None,
            env: BTreeMap::new(),
            isolate: true,
            unset_env: Vec::new(),
        };
        let launcher = CodexLauncher::from_connection(&connection).unwrap();
        assert_eq!(launcher.program, "codex");
        assert_eq!(launcher.prefix_args, ["--profile", "local"]);
        assert_eq!(launcher.compaction_limit(), Some(150_000));
        assert_eq!(
            launcher
                .turn_sandbox_policy(Path::new("/repo"), "auto", &[])
                .unwrap()["writableRoots"],
            json!(["/repo"])
        );
        assert_eq!(
            launcher.server_command(Path::new("/repo")).args,
            ["--profile", "local", "app-server", "--listen", "stdio://"]
        );
        let interactive = build_interactive_codex_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            &[],
            Some("hello"),
        )
        .unwrap();
        assert!(interactive.args.iter().any(|arg| arg == "--approve-for-me"));
    }

    #[test]
    fn maps_codex_permission_modes_conservatively() {
        assert_eq!(
            codex_permissions("auto").unwrap(),
            ("on-request", "auto_review", "workspace-write")
        );
        assert_eq!(
            codex_permissions("dontAsk").unwrap(),
            ("never", "user", "workspace-write")
        );
        assert!(codex_permissions("mystery").is_err());
    }

    #[test]
    fn parses_structured_codex_stage_result() {
        let result = stage_result_from_text(
            r#"{"opsx_status":"READY","summary":"done"}"#,
            Some("thread-1".to_owned()),
            "Codex",
        )
        .unwrap();
        assert_eq!(result.text, "done");
        assert_eq!(result.session_id.as_deref(), Some("thread-1"));
    }

    #[test]
    fn activity_ellipsizes_long_codex_tool_commands() {
        let command = format!("echo first\n{}", "x".repeat(300));
        let event = json!({
            "method": "item/completed",
            "params": {
                "item": {
                    "type": "commandExecution",
                    "command": command,
                    "aggregatedOutput": "full output remains hidden in activity mode"
                }
            }
        });

        let items = filter_codex_event(&event, StreamFilter::Activity);
        let [StreamItem::Tool(summary)] = items.as_slice() else {
            panic!("expected one tool summary, got {items:?}");
        };
        assert!(summary.starts_with("Shell: echo first "));
        assert!(summary.ends_with('…'));
        assert_eq!(summary.chars().count(), "Shell: ".chars().count() + 240);
        assert!(!summary.contains('\n'));
        assert!(!summary.contains("full output remains hidden"));
    }

    #[test]
    fn turns_a_project_slash_skill_into_explicit_codex_input() {
        let repo = std::env::temp_dir().join(format!("opsx-codex-skill-{}", uuid::Uuid::new_v4()));
        let skill = repo.join(".claude/skills/propose-unattended/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "---\nname: propose-unattended\n---\n").unwrap();
        let input = codex_turn_input(&repo, "/propose-unattended plan slice\n\nprotocol");
        assert_eq!(input[0]["type"], "skill");
        assert_eq!(input[0]["name"], "propose-unattended");
        assert_eq!(input[1]["text"], "plan slice\n\nprotocol");
        std::fs::remove_dir_all(repo).unwrap();
    }
}
