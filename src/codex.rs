use std::{
    collections::{BTreeMap, HashSet},
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
        AgentBackend, SessionId, SessionMode, StageProtocol, StageResult,
        is_missing_terminal_result, stage_result_from_text,
    },
    cli::{AgentConnection, BackendKind, ConnectionEnvironmentValue},
    codex_home::IsolatedCodexHome,
    codex_server::{CodexRequestError, CodexServer},
    process::{CommandSpec, PauseRequested, WorkerEscalationRequested},
    stream::{StreamControl, StreamFilter, StreamItem},
    ui::Ui,
};

const PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_secs(5);
const PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const MAX_INCOMPLETE_STAGE_RECOVERIES: u32 = 1;

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
    pub permission_profile: Option<String>,
    pub structured_output: bool,
    pub context_window: Option<u64>,
    pub auto_compact_window: Option<u64>,
    pub auto_compact_percent: Option<u8>,
    pub max_output_tokens: Option<u64>,
    pub(crate) environment: Vec<LauncherEnvironment>,
    pub(crate) unset_environment: Vec<String>,
    pub(crate) isolated_home: Option<IsolatedCodexHome>,
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
        let explicit_home = environment
            .iter()
            .any(|variable| variable.name == "CODEX_HOME")
            || (!unset_environment.iter().any(|name| name == "CODEX_HOME")
                && std::env::var_os("CODEX_HOME").is_some());
        let isolated_home = if explicit_home {
            None
        } else {
            Some(IsolatedCodexHome::new(
                std::env::home_dir()
                    .context("cannot locate home directory for isolated Codex state")?
                    .join(".codex"),
            ))
        };
        Ok(Self {
            connection_name: connection.name.clone(),
            environment_name: connection.environment_name.clone(),
            program: words.remove(0),
            prefix_args: words,
            model: connection.model.clone(),
            permission_profile: connection.permission_profile.clone(),
            structured_output: connection.structured_output,
            context_window: connection.context_window,
            auto_compact_window: connection.auto_compact_window,
            auto_compact_percent: connection.auto_compact_percent,
            max_output_tokens: connection.max_output_tokens,
            environment,
            unset_environment,
            isolated_home,
        })
    }

    pub(crate) fn server_command(&self, repo: &Path) -> CommandSpec {
        let mut spec = CommandSpec::new(&self.program, repo).args(self.prefix_args.iter().cloned());
        if let Some(home) = &self.isolated_home {
            // CLI overrides also defeat sqlite_home/log_dir in the shared config.
            spec = spec.args([
                "-c".to_owned(),
                format!(
                    "sqlite_home={}",
                    toml::Value::String(home.path.display().to_string())
                ),
                "-c".to_owned(),
                format!(
                    "log_dir={}",
                    toml::Value::String(home.path.join("log").display().to_string())
                ),
            ]);
        }
        spec = spec.args(["app-server", "--listen", "stdio://"]);
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
        if let Some(home) = &self.isolated_home {
            spec = spec
                .env("CODEX_HOME", home.path.to_string_lossy())
                .remove_env("CODEX_SQLITE_HOME");
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
            "runtimeWorkspaceRoots": roots,
        });
        if !self.uses_permission_profile(permission_mode) {
            params["sandbox"] = json!(sandbox);
        }
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        let mut config = serde_json::Map::new();
        if let Some(profile) = self
            .permission_profile
            .as_ref()
            .filter(|_| self.uses_permission_profile(permission_mode))
        {
            config.insert("default_permissions".to_owned(), json!(profile));
        }
        if let Some(limit) = self.compaction_limit() {
            config.insert("model_auto_compact_token_limit".to_owned(), json!(limit));
        }
        if !config.is_empty() {
            params["config"] = Value::Object(config);
        }
        Ok(params)
    }

    fn uses_permission_profile(&self, permission_mode: &str) -> bool {
        self.permission_profile.is_some()
            && matches!(
                permission_mode,
                "auto" | "acceptEdits" | "default" | "dontAsk"
            )
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
    ) -> Result<Option<Value>> {
        if self.uses_permission_profile(permission_mode) {
            return Ok(None);
        }
        let (_, _, sandbox) = codex_permissions(permission_mode)?;
        Ok(Some(match sandbox {
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
        }))
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
    if launcher.uses_permission_profile(permission_mode) {
        let (approval_policy, approvals_reviewer, _) = codex_permissions(permission_mode)?;
        let profile = launcher
            .permission_profile
            .as_deref()
            .expect("permission profile was checked above");
        for (key, value) in [
            ("default_permissions", profile),
            ("approval_policy", approval_policy),
            ("approvals_reviewer", approvals_reviewer),
        ] {
            args.extend(["-c".to_owned(), format!("{key}={}", json!(value))]);
        }
    } else {
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
            "bypassPermissions" => {
                args.push("--dangerously-bypass-approvals-and-sandbox".to_owned())
            }
            other => bail!(
                "Claude permission mode `{other}` has no Codex mapping; use auto, acceptEdits, default, dontAsk, plan, or bypassPermissions"
            ),
        }
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
    requires_user_turn: bool,
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
        let schema = self
            .launcher
            .structured_output
            .then(|| serde_json::from_str(protocol.json_schema()))
            .transpose()
            .context("opsx-build stage schema is invalid JSON")?;
        let text_protocol = (!self.launcher.structured_output).then(|| {
            format!(
                "{prompt}\n\nReturn the final stage result as a bare JSON object, without Markdown fences, matching this schema: {}. If JSON is unavailable, finish with exactly one `OPSX_STATUS: <value>` line using an allowed status. A plain prose response without a stage status is not sufficient.",
                protocol.json_schema()
            )
        });
        let prompt = text_protocol.as_deref().unwrap_or(prompt);
        self.ui.debug_prompt(activity, prompt);
        let turn_id = client.start_turn(
            session_id.as_str(),
            codex_turn_input(self.repo, prompt),
            schema,
            self.launcher.turn_sandbox_policy(
                self.repo,
                self.permission_mode,
                &self.additional_roots,
            )?,
            self.stream_filter
                .is_some_and(StreamFilter::includes_reasoning)
                .then_some("auto"),
        )?;
        let started = Instant::now();
        let mut response = String::new();
        let mut context_report = None;
        let mut pending_control = None;
        let mut stream = CodexStream::default();
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
                    && event
                        .pointer("/params/turnId")
                        .or_else(|| event.pointer("/params/turn/id"))
                        .and_then(Value::as_str)
                        .is_none_or(|id| id == turn_id)
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
                        for item in stream.filter_event(&event, filter) {
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

    fn prepare_session(&self, session: SessionMode) -> Result<SessionMode> {
        let id = match self.start_or_resume(session.clone()) {
            Ok(id) => id,
            Err(error)
                if matches!(&session, SessionMode::Resume { id }
                    if error.downcast_ref::<CodexRequestError>()
                        .is_some_and(|error| error.is_missing_thread(id.as_str()))) =>
            {
                self.ui.warn(&format!(
                    "Codex planning thread `{}` has no saved rollout; continuing the same phase in a fresh thread from repository state",
                    session.id()
                ));
                self.start_or_resume(SessionMode::New {
                    id: session.id().clone(),
                    name: Some("opsx-build-planning".to_owned()),
                })?
            }
            Err(error) => return Err(error),
        };
        Ok(SessionMode::Resume { id })
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
        let mut incomplete_stage_recoveries = 0;
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
                    if matches!(protocol, StageProtocol::Worker | StageProtocol::Verify)
                        && is_missing_terminal_result(&error)
                        && incomplete_stage_recoveries < MAX_INCOMPLETE_STAGE_RECOVERIES =>
                {
                    incomplete_stage_recoveries += 1;
                    self.ui.warn(
                        "Codex ended the phase without a terminal result; continuing the same session once",
                    );
                    current_prompt = incomplete_stage_continuation_prompt(protocol);
                    current_activity = format!(
                        "{activity} (incomplete-turn recovery {incomplete_stage_recoveries}/{MAX_INCOMPLETE_STAGE_RECOVERIES})"
                    );
                }
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
                        .is_some_and(|error| error.requires_user_turn)
                        && provider_retries < self.max_provider_retries =>
                {
                    provider_retries += 1;
                    self.ui.warn(&format!(
                        "Codex provider requires a user turn; continuing the same thread ({provider_retries}/{})",
                        self.max_provider_retries
                    ));
                    current_prompt = continuation_prompt(
                        protocol,
                        "The provider rejected a request ending with an assistant message. This user message supplies the required continuation.",
                    );
                    current_activity = format!(
                        "{activity} (provider recovery {provider_retries}/{})",
                        self.max_provider_retries
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

const MAX_REASONING_SUMMARY_CHARS: usize = 600;

#[derive(Default)]
struct ReasoningSummary {
    text: String,
    emitted: usize,
    truncated: bool,
}

impl ReasoningSummary {
    fn append(&mut self, delta: &str) -> Vec<StreamItem> {
        if self.truncated {
            return Vec::new();
        }
        let remaining = MAX_REASONING_SUMMARY_CHARS.saturating_sub(self.text.chars().count());
        self.text.extend(delta.chars().take(remaining));
        if delta.chars().count() > remaining {
            self.text.push('…');
            self.truncated = true;
        }
        self.drain(self.truncated)
    }

    fn complete(&mut self, text: &str) -> Vec<StreamItem> {
        // Completed items repeat the deltas, but may include a missing final suffix.
        let mut items = if text.starts_with(&self.text) {
            self.append(&text[self.text.len()..])
        } else {
            Vec::new()
        };
        items.extend(self.drain(true));
        items
    }

    fn drain(&mut self, complete: bool) -> Vec<StreamItem> {
        let mut items = Vec::new();
        loop {
            let pending = &self.text[self.emitted..];
            let (end, consumed) = match pending.find("\n\n") {
                Some(end) => (end, end + 2),
                None if complete && !pending.is_empty() => (pending.len(), pending.len()),
                _ => break,
            };
            let paragraph = pending[..end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            self.emitted += consumed;
            if !paragraph.is_empty() {
                items.push(StreamItem::Reasoning(paragraph));
            }
        }
        items
    }
}

#[derive(Default)]
struct CodexStream {
    summaries: BTreeMap<(String, u64), ReasoningSummary>,
    started_commands: HashSet<String>,
}

impl CodexStream {
    fn finish_summaries(&mut self, item_id: Option<&str>) -> Vec<StreamItem> {
        self.summaries
            .iter_mut()
            .filter(|((id, _), _)| item_id.is_none_or(|wanted| id == wanted))
            .flat_map(|(_, summary)| summary.drain(true))
            .collect()
    }

    fn filter_event(&mut self, event: &Value, filter: StreamFilter) -> Vec<StreamItem> {
        if filter == StreamFilter::Raw {
            return vec![StreamItem::Raw(event.to_string())];
        }
        let method = event
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("event");
        let mut items = Vec::new();
        if filter.includes_reasoning() {
            match method {
                "item/reasoning/summaryTextDelta" => {
                    if let (Some(id), Some(index), Some(delta)) = (
                        event.pointer("/params/itemId").and_then(Value::as_str),
                        event
                            .pointer("/params/summaryIndex")
                            .and_then(Value::as_u64),
                        event.pointer("/params/delta").and_then(Value::as_str),
                    ) {
                        return self
                            .summaries
                            .entry((id.to_owned(), index))
                            .or_default()
                            .append(delta);
                    }
                }
                "item/reasoning/summaryPartAdded" => {
                    if let Some(id) = event.pointer("/params/itemId").and_then(Value::as_str) {
                        return self.finish_summaries(Some(id));
                    }
                }
                "item/started" | "item/completed"
                    if event.pointer("/params/item/type").and_then(Value::as_str)
                        != Some("reasoning") =>
                {
                    items.extend(self.finish_summaries(None));
                }
                "turn/completed" => items.extend(self.finish_summaries(None)),
                _ => {}
            }
        }
        match method {
            "item/started" | "item/completed" => {
                let Some(item) = event.pointer("/params/item") else {
                    return items;
                };
                let completed = method == "item/completed";
                match item.get("type").and_then(Value::as_str) {
                    Some("commandExecution") => {
                        let id = item.get("id").and_then(Value::as_str);
                        let already_started =
                            id.is_some_and(|id| self.started_commands.contains(id));
                        if !already_started {
                            let command = item
                                .get("command")
                                .and_then(Value::as_str)
                                .unwrap_or("command");
                            items.push(StreamItem::Tool(format!("Shell: {}", one_line(command))));
                            if !completed && let Some(id) = id {
                                self.started_commands.insert(id.to_owned());
                            }
                        }
                        if completed {
                            let exit = item.get("exitCode").and_then(Value::as_i64);
                            let status = item.get("status").and_then(Value::as_str);
                            if exit.is_some_and(|code| code != 0) {
                                items.push(StreamItem::Tool(format!(
                                    "Shell exited with code {}",
                                    exit.unwrap()
                                )));
                            } else if matches!(status, Some("failed" | "declined" | "interrupted"))
                            {
                                items.push(StreamItem::Tool(format!("Shell {}", status.unwrap())));
                            }
                            if filter == StreamFilter::Full
                                && let Some(output) =
                                    item.get("aggregatedOutput").and_then(Value::as_str)
                                && !output.trim().is_empty()
                            {
                                items.push(StreamItem::ToolResult(output.to_owned()));
                            }
                        }
                    }
                    Some("reasoning") if completed && filter.includes_reasoning() => {
                        if let Some(id) = item.get("id").and_then(Value::as_str) {
                            if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                                for (index, text) in summary.iter().enumerate() {
                                    if let Some(text) = text.as_str() {
                                        items.extend(
                                            self.summaries
                                                .entry((id.to_owned(), index as u64))
                                                .or_default()
                                                .complete(text),
                                        );
                                    }
                                }
                            }
                            items.extend(self.finish_summaries(Some(id)));
                        }
                    }
                    Some("reasoning") => {}
                    Some("agentMessage") if completed => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            items.push(StreamItem::Codex(text.to_owned()));
                        }
                    }
                    Some("fileChange") if completed => {
                        items.push(StreamItem::Tool("Applied file changes".to_owned()))
                    }
                    Some(kind) if completed && filter == StreamFilter::Full => {
                        items.push(StreamItem::Lifecycle(format!("Codex item: {kind}")))
                    }
                    _ => {}
                }
            }
            "thread/compacted" => {
                items.push(StreamItem::Lifecycle("Codex context compacted".to_owned()))
            }
            "turn/completed" if filter == StreamFilter::Full => {
                let status = event
                    .pointer("/params/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                items.push(StreamItem::Lifecycle(format!("Codex turn: {status}")));
            }
            _ => {}
        }
        items
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

fn incomplete_stage_continuation_prompt(protocol: StageProtocol) -> String {
    let instruction = match protocol {
        StageProtocol::Worker => {
            "The preceding turn ended without returning a terminal result. Inspect the durable OpenSpec and working-tree state, then continue from the first incomplete task. Perform outstanding work with actual tools; do not merely describe what you intend to do. Do not stop at a progress update."
        }
        StageProtocol::Verify => {
            "The preceding verification turn ended without returning a terminal result. Continue verification only: do not repair or modify agendas, OpenSpec artifacts, implementation, or tests. If you created temporary diagnostic artifacts, remove only those artifacts where safe. If you found a concrete correctable issue, return RETRY with the exact finding and required repair; otherwise finish verification and return VERIFIED or BLOCKED as appropriate."
        }
        _ => unreachable!("incomplete-turn recovery is only used for worker and verify stages"),
    };
    continuation_prompt(protocol, instruction)
}

fn classify_turn_error(message: String) -> CodexTurnError {
    let lowercase = message.to_ascii_lowercase();
    let requires_user_turn =
        lowercase.contains("requests ending with a model turn are not supported");
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
        requires_user_turn,
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
            permission_profile: None,
            structured_output: true,
            context_window: Some(200_000),
            auto_compact_window: None,
            auto_compact_percent: Some(75),
            max_output_tokens: None,
            env: BTreeMap::new(),
            isolate: true,
            unset_env: vec!["CODEX_HOME".to_owned()],
        };
        let launcher = CodexLauncher::from_connection(&connection).unwrap();
        assert_eq!(launcher.program, "codex");
        assert_eq!(launcher.prefix_args, ["--profile", "local"]);
        assert_eq!(launcher.compaction_limit(), Some(150_000));
        assert_eq!(
            launcher
                .turn_sandbox_policy(Path::new("/repo"), "auto", &[])
                .unwrap()
                .unwrap()["writableRoots"],
            json!(["/repo"])
        );
        let server = launcher.server_command(Path::new("/repo"));
        let home = launcher.isolated_home.as_ref().unwrap();
        assert!(
            server
                .args
                .starts_with(&["--profile".to_owned(), "local".to_owned()])
        );
        assert!(server.args.ends_with(&[
            "app-server".to_owned(),
            "--listen".to_owned(),
            "stdio://".to_owned()
        ]));
        assert!(
            server
                .env
                .contains(&("CODEX_HOME".to_owned(), home.path.display().to_string()))
        );
        assert!(server.env_remove.contains(&"CODEX_SQLITE_HOME".to_owned()));
        assert!(
            server
                .args
                .iter()
                .any(|arg| arg.starts_with("sqlite_home="))
        );
        assert!(server.args.iter().any(|arg| arg.starts_with("log_dir=")));
        let interactive = build_interactive_codex_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            &[],
            Some("hello"),
        )
        .unwrap();
        assert!(interactive.args.iter().any(|arg| arg == "--approve-for-me"));
        assert!(!interactive.env.iter().any(|(name, _)| name == "CODEX_HOME"));
        assert!(
            !interactive
                .args
                .iter()
                .any(|arg| arg.starts_with("sqlite_home="))
        );

        let mut connection = connection;
        connection.env.insert(
            "CODEX_HOME".to_owned(),
            ConnectionEnvironmentValue::Literal("/custom/codex".to_owned()),
        );
        let launcher = CodexLauncher::from_connection(&connection).unwrap();
        assert!(launcher.isolated_home.is_none());
        let server = launcher.server_command(Path::new("/repo"));
        assert!(
            server
                .env
                .contains(&("CODEX_HOME".to_owned(), "/custom/codex".to_owned()))
        );
        assert!(
            !server
                .args
                .iter()
                .any(|arg| arg.starts_with("sqlite_home="))
        );
    }

    #[test]
    fn named_permission_profile_replaces_inline_codex_sandbox() {
        let connection = AgentConnection {
            name: Some("codex-worker".to_owned()),
            backend: BackendKind::Codex,
            environment_name: None,
            command: "codex".to_owned(),
            model: None,
            permission_profile: Some("opsx-build".to_owned()),
            structured_output: true,
            context_window: None,
            auto_compact_window: Some(150_000),
            auto_compact_percent: None,
            max_output_tokens: None,
            env: BTreeMap::new(),
            isolate: true,
            unset_env: Vec::new(),
        };
        let launcher = CodexLauncher::from_connection(&connection).unwrap();
        let params = launcher
            .thread_params(Path::new("/repo"), "auto", &[])
            .unwrap();
        assert!(params.get("sandbox").is_none());
        assert_eq!(params["config"]["default_permissions"], "opsx-build");
        assert_eq!(params["config"]["model_auto_compact_token_limit"], 150_000);
        assert!(
            launcher
                .turn_sandbox_policy(Path::new("/repo"), "auto", &[])
                .unwrap()
                .is_none()
        );

        let interactive =
            build_interactive_codex_command(Path::new("/repo"), &launcher, "auto", &[], None)
                .unwrap();
        assert!(
            interactive
                .args
                .windows(2)
                .any(|pair| pair == ["-c", "default_permissions=\"opsx-build\""])
        );
        assert!(!interactive.args.iter().any(|arg| arg == "--sandbox"));
        assert!(!interactive.args.iter().any(|arg| arg == "--approve-for-me"));
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

        let items = CodexStream::default().filter_event(&event, StreamFilter::Activity);
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
    fn reasoning_stream_emits_paragraphs_once_and_finishes_partial_summaries() {
        let mut stream = CodexStream::default();
        let delta = |id: &str, index: u64, text: &str| {
            json!({
                "method": "item/reasoning/summaryTextDelta",
                "params": {"itemId": id, "summaryIndex": index, "delta": text}
            })
        };
        let filter = StreamFilter::Reasoning;
        assert!(
            stream
                .filter_event(&delta("r1", 0, "Checking retry"), filter)
                .is_empty()
        );
        assert_eq!(
            stream.filter_event(&delta("r1", 0, " reset.\n\nSuccess resets"), filter),
            [StreamItem::Reasoning("Checking retry reset.".to_owned())]
        );
        let completed = json!({"method":"item/completed", "params":{"item":{
            "id":"r1", "type":"reasoning",
            "summary":["Checking retry reset.\n\nSuccess resets the counter.", "Adding coverage."],
            "content":["Raw reasoning must not appear"]
        }}});
        assert_eq!(
            stream.filter_event(&completed, filter),
            [
                StreamItem::Reasoning("Success resets the counter.".to_owned()),
                StreamItem::Reasoning("Adding coverage.".to_owned()),
            ]
        );
        assert!(stream.filter_event(&completed, filter).is_empty());

        assert!(
            stream
                .filter_event(&delta("r2", 0, "Checking cancellation."), filter)
                .is_empty()
        );
        assert_eq!(stream.filter_event(&json!({
            "method":"item/reasoning/summaryPartAdded", "params":{"itemId":"r2", "summaryIndex":1}
        }), filter), [StreamItem::Reasoning("Checking cancellation.".to_owned())]);
        assert!(
            stream
                .filter_event(&delta("r2", 1, "Cancellation is separate."), filter)
                .is_empty()
        );
        assert_eq!(
            stream.filter_event(&json!({"method":"turn/completed"}), filter),
            [StreamItem::Reasoning(
                "Cancellation is separate.".to_owned()
            )]
        );
    }

    #[test]
    fn reasoning_stream_is_opt_in_bounded_and_ignores_raw_reasoning() {
        let event = json!({"method":"item/completed", "params":{"item":{
            "id":"r1", "type":"reasoning", "summary":["Public summary."], "content":["Raw text"]
        }}});
        assert!(
            CodexStream::default()
                .filter_event(&event, StreamFilter::Activity)
                .is_empty()
        );
        assert_eq!(
            CodexStream::default().filter_event(&event, StreamFilter::Raw),
            [StreamItem::Raw(event.to_string())]
        );
        for filter in [StreamFilter::Reasoning, StreamFilter::Full] {
            let mut stream = CodexStream::default();
            assert_eq!(
                stream.filter_event(&event, filter),
                [StreamItem::Reasoning("Public summary.".to_owned())]
            );
            assert!(stream.filter_event(&json!({
                "method":"item/reasoning/textDelta", "params":{"itemId":"r1", "delta":"Raw text"}
            }), filter).is_empty());
            let long = "é".repeat(900);
            let delta = json!({"method":"item/reasoning/summaryTextDelta", "params":{
                "itemId":"long", "summaryIndex":0, "delta":long
            }});
            let items = stream.filter_event(&delta, filter);
            assert_eq!(
                items,
                [StreamItem::Reasoning(format!(
                    "{}…",
                    "é".repeat(MAX_REASONING_SUMMARY_CHARS)
                ))]
            );
            assert!(stream.filter_event(&delta, filter).is_empty());
            assert!(
                stream
                    .filter_event(
                        &json!({"method":"item/completed", "params":{"item":{
                            "id":"long", "type":"reasoning", "summary":[long]
                        }}}),
                        filter
                    )
                    .is_empty()
            );
            assert!(
                stream.summaries[&("long".to_owned(), 0)].text.len()
                    <= MAX_REASONING_SUMMARY_CHARS * 4 + 3
            );
        }
    }

    #[test]
    fn command_starts_follow_pending_context_without_duplicate_completion_chatter() {
        for filter in [
            StreamFilter::Activity,
            StreamFilter::Reasoning,
            StreamFilter::Full,
        ] {
            let mut stream = CodexStream::default();
            stream.filter_event(
                &json!({"method":"item/reasoning/summaryTextDelta", "params":{
                    "itemId":"r1", "summaryIndex":0, "delta":"Running the integration checks."
                }}),
                filter,
            );
            let started = json!({"method":"item/started", "params":{"item":{
                "type":"commandExecution", "id":"cmd1", "command":"cargo test"
            }}});
            let mut expected = Vec::new();
            if filter.includes_reasoning() {
                expected.push(StreamItem::Reasoning(
                    "Running the integration checks.".to_owned(),
                ));
            }
            expected.push(StreamItem::Tool("Shell: cargo test".to_owned()));
            assert_eq!(stream.filter_event(&started, filter), expected);
            assert!(stream.filter_event(&started, filter).is_empty());
            let completed = json!({"method":"item/completed", "params":{"item":{
                "type":"commandExecution", "id":"cmd1", "command":"cargo test", "exitCode":1,
                "status":"completed", "aggregatedOutput":"a test failed"
            }}});
            let mut expected = vec![StreamItem::Tool("Shell exited with code 1".to_owned())];
            if filter == StreamFilter::Full {
                expected.push(StreamItem::ToolResult("a test failed".to_owned()));
            }
            assert_eq!(stream.filter_event(&completed, filter), expected);
        }
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
