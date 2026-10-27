use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{
    backend::{
        AgentBackend, MAX_INCOMPLETE_STAGE_RECOVERIES, MissingTerminalResult, SessionId,
        SessionMode, StageProtocol, StageResult, StageSignal, parse_terminal_signal,
    },
    claude::{
        CONNECTION_TEST_MARKER, ConnectionTestMode, incomplete_stage_continuation_prompt,
        stage_prompt,
    },
    cli::{AgentConnection, BackendKind, ConnectionEnvironmentValue},
    opencode_server::{OpenCodeServer, OpenCodeServerClient},
    process::{
        BackendStreamControlRequested, CommandSpec, PauseRequested, ProcessOutput,
        WorkerEscalationRequested, diagnostic_text,
    },
    stream::{StreamControl, StreamFilter, StreamItem},
    ui::Ui,
    usage::{Attempt, Identity},
};

const CONNECTION_TOOL_MARKER: &str = "OPSX_TOOL_ROUNDTRIP_OK";
const PROVIDER_RETRY_BASE_DELAY: Duration = Duration::from_secs(5);
const PROVIDER_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct OpenCodeApiError {
    message: String,
    transient: bool,
    session_id: Option<SessionId>,
    retry_after: Option<Duration>,
}

impl fmt::Display for OpenCodeApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for OpenCodeApiError {}

enum OpenCodeAttempt {
    Complete(StageResult),
    OutputLimit(SessionId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LauncherEnvironment {
    name: String,
    value: String,
    redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeLauncher {
    pub connection_name: Option<String>,
    pub environment_name: Option<String>,
    pub program: String,
    pub prefix_args: Vec<String>,
    pub model: Option<String>,
    pub context_window: Option<u64>,
    environment: Vec<LauncherEnvironment>,
    unset_environment: Vec<String>,
}

impl OpenCodeLauncher {
    pub fn from_connection(connection: &AgentConnection) -> Result<Self> {
        if connection.backend != BackendKind::OpenCode {
            bail!(
                "connection profile `{}` selects {} rather than the OpenCode backend",
                connection.name.as_deref().unwrap_or("unnamed"),
                connection.backend
            );
        }
        let mut words = split_command(&connection.command)?;
        if words.is_empty() {
            bail!("OpenCode connection command cannot be empty");
        }
        if let Some(model) = connection.model.as_deref()
            && !model.contains('/')
        {
            bail!(
                "OpenCode model `{model}` must use provider/model form (for example `openrouter/qwen/qwen3.6-35b-a3b`)"
            );
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
                            "OpenCode connection profile `{}` requires environment variable `{}` for `{name}`",
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
            environment,
            unset_environment,
        })
    }
}

pub(crate) fn build_opencode_server_command(
    repo: &Path,
    launcher: &OpenCodeLauncher,
) -> CommandSpec {
    let mut args = launcher.prefix_args.clone();
    args.extend(["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"].map(str::to_owned));
    with_launcher_environment(
        CommandSpec::new(&launcher.program, repo).args(args),
        launcher,
    )
    .remove_env("OPENCODE_PASSWORD")
    .remove_env("OPENCODE_SERVER_PASSWORD")
    .remove_env("OPENCODE_SERVER_USERNAME")
}

pub fn build_opencode_command(
    repo: &Path,
    launcher: &OpenCodeLauncher,
    session: &SessionMode,
    prompt: &str,
) -> CommandSpec {
    let invocation = resolve_invocation(repo, prompt);
    let mut args = launcher.prefix_args.clone();
    args.extend(["run", "--standalone", "--format", "json"].map(str::to_owned));
    args.push("--auto".to_owned());
    if let Some(model) = &launcher.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    match session {
        SessionMode::New { name, .. } => {
            if let Some(name) = name {
                args.extend(["--title".to_owned(), name.clone()]);
            }
        }
        SessionMode::Resume { id } => {
            args.extend(["--session".to_owned(), id.to_string()]);
        }
    }
    // Workflow command callbacks are dispatched through the V2 HTTP API.
    // This CLI form is only a preview for those requests.
    let message = match invocation.command {
        Some(command) => format!("/{command} {}", invocation.message),
        None => match invocation.skill {
            Some(skill) => format!(
                "Use the skill tool to load and follow the `{skill}` skill for this assignment.\n\n{}",
                invocation.message
            ),
            None => invocation.message,
        },
    };
    if !message.is_empty() {
        args.push(message);
    }
    with_launcher_environment(
        CommandSpec::new(&launcher.program, repo).args(args),
        launcher,
    )
}

pub fn build_connection_test_command(
    repo: &Path,
    launcher: &OpenCodeLauncher,
    mode: ConnectionTestMode,
) -> CommandSpec {
    let prompt = match mode {
        ConnectionTestMode::Basic => format!(
            "Connectivity test only. Do not inspect files, invoke tools, or perform any other work. Reply with exactly {CONNECTION_TEST_MARKER}."
        ),
        ConnectionTestMode::Agentic => format!(
            "OpenCode provider compatibility test. Invoke the shell tool exactly once with the command `printf '{CONNECTION_TOOL_MARKER}\\n'`. After receiving that tool result, reply with exactly {CONNECTION_TEST_MARKER}. Do not inspect files or perform any other work."
        ),
    };
    build_opencode_command(
        repo,
        launcher,
        &SessionMode::New {
            id: SessionId::new("connection-test"),
            name: Some("opsx-build-connection-test".to_owned()),
        },
        &prompt,
    )
}

pub fn build_interactive_opencode_command(
    repo: &Path,
    launcher: &OpenCodeLauncher,
    auto_approve: bool,
    extra_args: &[String],
    initial_prompt: Option<&str>,
) -> CommandSpec {
    let mut args = launcher.prefix_args.clone();
    if let Some(model) = &launcher.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    if auto_approve {
        args.push("--auto".to_owned());
    }
    args.extend(extra_args.iter().cloned());
    if let Some(prompt) = initial_prompt {
        args.extend(["--prompt".to_owned(), prompt.to_owned()]);
    }
    with_launcher_environment(
        CommandSpec::new(&launcher.program, repo).args(args),
        launcher,
    )
}

pub fn parse_connection_test_output(
    output: &ProcessOutput,
    mode: ConnectionTestMode,
) -> Result<String> {
    if !output.success {
        return Err(opencode_stage_failure(output));
    }
    let events = output
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    if let Some(error) = events.iter().rev().find_map(opencode_event_error) {
        bail!("OpenCode connection test failed: {error}");
    }
    if mode == ConnectionTestMode::Agentic {
        let tool_events = events
            .iter()
            .filter(|event| event.get("type").and_then(Value::as_str) == Some("tool_use"))
            .collect::<Vec<_>>();
        let invoked = tool_events.iter().any(|event| {
            event
                .pointer("/part/state/input/command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains(CONNECTION_TOOL_MARKER))
        });
        let observed = tool_events.iter().any(|event| {
            event
                .pointer("/part/state/output")
                .is_some_and(|output| output.to_string().contains(CONNECTION_TOOL_MARKER))
        });
        if !invoked {
            bail!(
                "OpenCode transport succeeded, but the model did not invoke the required shell compatibility probe"
            );
        }
        if !observed {
            bail!("OpenCode invoked the compatibility probe, but no tool result reached the agent");
        }
    }
    let response = events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|event| event.pointer("/part/text"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .context("OpenCode connection test returned an empty model response")?;
    Ok(response.to_owned())
}

pub struct OpenCodeBackend<'a, U: Ui> {
    repo: &'a Path,
    launcher: &'a OpenCodeLauncher,
    server: Arc<OpenCodeServer>,
    stream_transport: bool,
    stream_filter: Option<StreamFilter>,
    max_output_retries: u32,
    max_provider_retries: u32,
    provider_retry_base_delay: Duration,
    stage_timeout: Option<Duration>,
    resume_repo: Option<PathBuf>,
    ui: &'a U,
}

impl<'a, U: Ui> OpenCodeBackend<'a, U> {
    pub fn new(
        repo: &'a Path,
        launcher: &'a OpenCodeLauncher,
        stream_transport: bool,
        stream_filter: Option<StreamFilter>,
        ui: &'a U,
    ) -> Self {
        Self {
            repo,
            launcher,
            server: Arc::new(OpenCodeServer::new(repo, launcher)),
            stream_transport: stream_transport || stream_filter.is_some(),
            stream_filter,
            max_output_retries: 0,
            max_provider_retries: 0,
            provider_retry_base_delay: PROVIDER_RETRY_BASE_DELAY,
            stage_timeout: None,
            resume_repo: None,
            ui,
        }
    }

    pub(crate) fn with_server(mut self, server: Arc<OpenCodeServer>) -> Self {
        self.server = server;
        self
    }

    pub fn with_stage_timeout(mut self, timeout: Duration) -> Self {
        self.stream_transport = true;
        self.stage_timeout = Some(timeout);
        self
    }

    pub fn with_retries(mut self, output_retries: u32, provider_retries: u32) -> Self {
        self.max_output_retries = output_retries;
        self.max_provider_retries = provider_retries;
        self
    }

    pub fn with_resume_repo(mut self, repo: &Path) -> Self {
        self.resume_repo = Some(repo.to_path_buf());
        self
    }

    fn observed_compaction(&self, session: &SessionId, limit: Option<Duration>) -> Result<()> {
        let started = Instant::now();
        if limit.is_some_and(|limit| limit.is_zero()) {
            return Err(WorkerEscalationRequested::new(
                "OpenCode stage deadline expired before compaction",
            )
            .into());
        }
        let invocation = uuid::Uuid::new_v4().to_string();
        let mut usage = Attempt::new(
            self.ui,
            self.repo,
            Identity {
                backend: "opencode",
                connection: self.launcher.connection_name.as_deref(),
                model: self.launcher.model.as_deref(),
                session: session.as_str(),
                invocation: &invocation,
                attempt: 1,
                fresh: false,
            },
            "Compacting OpenCode session",
        );
        let client = self.server.client(self.ui)?;
        client.subscribe()?;
        let id = new_input_id();
        usage.before_request()?;
        let mut result = (|| {
            client.admit_compaction(session.as_str(), &id)?;
            let output = self.wait_for_turn(
                &client,
                session,
                &id,
                true,
                Some(
                    limit
                        .unwrap_or(Duration::MAX)
                        .min(self.stage_timeout.unwrap_or(Duration::from_secs(3600)))
                        .saturating_sub(started.elapsed()),
                ),
                &mut usage,
            )?;
            if !output.success {
                return Err(opencode_stage_failure(&output));
            }
            Ok(output)
        })();
        if let Err(error) = &result
            && let Err(cleanup) = self.cancel_failed_turn(&client, session, error)
        {
            result = Err(cleanup);
        }
        usage.finish(match &result {
            Ok(_) => "completed",
            Err(error) => crate::usage::error_outcome(error),
        });
        result.map(|_| ())
    }

    fn apply_manual_control(
        &self,
        client: &OpenCodeServerClient,
        session: &SessionId,
        control: &StreamControl,
        limit: Option<Duration>,
    ) -> Result<()> {
        match control {
            StreamControl::Compact => {
                self.observed_compaction(session, limit)?;
                self.ui.stream_item(&StreamItem::Lifecycle(format!(
                    "OpenCode compacted session {session}"
                )));
            }
            StreamControl::Context => self.ui.stream_item(&StreamItem::Lifecycle(
                client.context_report(session.as_str())?,
            )),
            StreamControl::Inject(_) => {}
            _ => unreachable!("only manual restart controls are applied"),
        }
        Ok(())
    }

    fn cancel_failed_turn(
        &self,
        client: &OpenCodeServerClient,
        session: &SessionId,
        error: &anyhow::Error,
    ) -> Result<()> {
        if let Err(cleanup) = client.abort_session(session.as_str()) {
            if let Some(guard) = self.ui.operating_policy() {
                guard.cancellation_unconfirmed(cleanup.to_string());
            }
            // Do not preserve a retry/escalation type when cancellation is unknown.
            bail!(
                "OpenCode cancellation is unconfirmed after {error:#}: {cleanup:#}. Automatic continuation is refused; inspect session {session} before resuming. The private server will be stopped"
            );
        }
        Ok(())
    }

    fn invoke_once(
        &self,
        session_id: &SessionId,
        prompt: &str,
        activity: &str,
        timeout: Option<Duration>,
        usage: &mut Attempt<'_, U>,
    ) -> Result<OpenCodeAttempt> {
        let started = Instant::now();
        self.ui.debug(&format!("OpenCode V2 session: {session_id}"));
        self.ui.debug_prompt(activity, prompt);
        let client = self.server.client(self.ui)?;
        client.prepare_session(session_id.as_str())?;
        client.subscribe()?;
        let mut invocation = resolve_invocation(self.repo, prompt);
        invocation.message.push_str("\n\nThis OpenCode request supplies no structured result schema. Return the required final OPSX_STATUS line. For verification, emit the OPSX_REVIEW JSON object before that line.");
        let id = new_input_id();
        usage.before_request()?;
        if self.stream_transport {
            self.ui.start_stream(activity);
        }
        let mut result = (|| {
            let admitted = client.prompt(
                session_id.as_str(),
                &id,
                &invocation.message,
                invocation.skill.as_deref(),
                invocation.command.as_deref(),
            )?;
            let output = self.wait_for_turn(
                &client,
                session_id,
                &admitted,
                false,
                timeout.map(|limit| limit.saturating_sub(started.elapsed())),
                usage,
            )?;
            if output_hit_token_limit(&output) {
                return Ok(OpenCodeAttempt::OutputLimit(session_id.clone()));
            }
            if !output.success {
                return Err(opencode_stage_failure(&output));
            }
            let mut result = parse_opencode_output(&output.stdout)?;
            result.session_id = Some(session_id.to_string());
            Ok(OpenCodeAttempt::Complete(result))
        })();
        if let Err(error) = &result
            && let Err(cleanup) = self.cancel_failed_turn(&client, session_id, error)
        {
            result = Err(cleanup);
        }
        if self.stream_transport {
            self.ui.finish_stream(result.is_ok(), activity);
        }
        result
    }

    fn wait_for_turn(
        &self,
        client: &OpenCodeServerClient,
        session: &SessionId,
        input: &str,
        compact: bool,
        timeout: Option<Duration>,
        usage: &mut Attempt<'_, U>,
    ) -> Result<ProcessOutput> {
        let started = Instant::now();
        let mut input = input.to_owned();
        let mut enqueued = false;
        let mut delivered = false;
        let mut compacted = false;
        let mut failed = false;
        let mut output = String::new();
        let mut tools = BTreeMap::new();
        loop {
            if let Some(guard) = self.ui.operating_policy() {
                guard.poll()?;
            }
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                return Err(WorkerEscalationRequested::new(
                    "OpenCode exceeded its stage timeout before confirmed completion",
                )
                .into());
            }
            if !enqueued && started.elapsed() >= Duration::from_secs(10) {
                bail!(
                    "OpenCode V2 did not enqueue the submitted input; no phase completion is confirmed. Inspect the session before resuming"
                );
            }
            if self.stream_transport {
                match self.ui.poll_stream() {
                    StreamControl::None => {}
                    StreamControl::Pause => {
                        return Err(PauseRequested::new(
                            self.resume_repo
                                .clone()
                                .unwrap_or_else(|| self.repo.to_path_buf()),
                        )
                        .into());
                    }
                    StreamControl::Escalate => {
                        return Err(WorkerEscalationRequested::new(
                            "operator requested frontier escalation",
                        )
                        .into());
                    }
                    StreamControl::Interrupt => bail!(
                        "OpenCode turn interrupted by the operator; resume the preserved checkpoint to continue"
                    ),
                    control => return Err(BackendStreamControlRequested::new(control).into()),
                }
            }
            let Some(event) = client.next_event(Duration::from_millis(50))? else {
                continue;
            };
            let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
            let data = &event["data"];
            if kind == "permission.asked" {
                let owner = data["sessionID"]
                    .as_str()
                    .context("OpenCode permission omitted its session")?;
                if client.owns_session(owner, session.as_str())? {
                    let id = data["id"]
                        .as_str()
                        .context("OpenCode permission omitted its ID")?;
                    client.approve_permission(owner, id)?;
                }
                continue;
            }
            if kind == "form.created" {
                let form = &data["form"];
                let owner = form["sessionID"]
                    .as_str()
                    .context("OpenCode form omitted its session")?;
                if owner == "global" || client.owns_session(owner, session.as_str())? {
                    bail!(
                        "OpenCode requires external input for form {} in session {owner}: {}. Review the request and resume with --direction when resolved",
                        form["id"],
                        form
                    );
                }
                continue;
            }
            if data.get("sessionID").and_then(Value::as_str) != Some(session.as_str()) {
                continue;
            }
            if kind == "session.inbox.enqueued" {
                let id = data["inboxID"]
                    .as_str()
                    .context("OpenCode enqueue event omitted inboxID")?;
                // Command callbacks generate their own input IDs.
                if input.is_empty() {
                    input = id.to_owned();
                }
                if id == input {
                    enqueued = true;
                }
            }
            if kind == "session.inbox.delivered" && data["inboxID"].as_str() == Some(input.as_str())
            {
                delivered = true;
                enqueued = true;
            }
            if kind == "session.inbox.cancelled" && data["inboxID"].as_str() == Some(input.as_str())
            {
                bail!(
                    "OpenCode cancelled submitted input {input}; no phase completion is confirmed. Inspect session {session} before resuming"
                );
            }
            if compact
                && kind == "session.compaction.started"
                && data["inputID"].as_str() == Some(input.as_str())
            {
                delivered = true;
                enqueued = true;
            }
            // Old executions and child sessions cannot complete this input.
            if !enqueued {
                continue;
            }
            // Steering may be admitted while an older step is still finishing.
            // Its content and usage cannot satisfy or be charged to this input.
            if !delivered
                && !matches!(
                    kind,
                    "session.execution.failed" | "session.execution.interrupted"
                )
            {
                continue;
            }
            if kind == "session.retry.scheduled" {
                let note = format!(
                    "OpenCode native retry: type={}, status={}, retryAfterMs={}",
                    data["error"]["type"], data["error"]["status"], data["error"]["retryAfterMs"]
                );
                usage.note(&note);
                if self.stream_filter.is_some() {
                    self.ui.stream_item(&StreamItem::Lifecycle(format!(
                        "{note}; attempt {}",
                        data["attempt"]
                    )));
                }
            }
            if let Some(normalized) = normalize_v2_event(&event, &mut tools) {
                let line = normalized.to_string();
                usage.line(&line);
                if let Some(filter) = self.stream_filter {
                    let items = if filter == StreamFilter::Raw {
                        vec![StreamItem::Raw(event.to_string())]
                    } else {
                        filter_opencode_line(&line, filter)
                    };
                    for item in items {
                        self.ui.stream_item(&item);
                    }
                }
                output.push_str(&line);
                output.push('\n');
            } else if self.stream_filter == Some(StreamFilter::Raw) {
                self.ui.stream_item(&StreamItem::Raw(event.to_string()));
            }
            match kind {
                "session.step.failed"
                | "session.execution.failed"
                | "session.compaction.failed" => {
                    // Wait for execution settlement, retaining the original error.
                    failed = true;
                    if kind == "session.execution.failed" || kind == "session.compaction.failed" {
                        return Ok(ProcessOutput {
                            success: false,
                            code: Some(1),
                            stdout: output,
                            stderr: String::new(),
                        });
                    }
                }
                "session.compaction.ended" if compact && delivered => compacted = true,
                "session.execution.interrupted" => bail!(
                    "OpenCode session {session} was interrupted ({}) before this input completed; inspect and resume the preserved checkpoint",
                    data["reason"]
                ),
                "session.execution.succeeded" if delivered && (!compact || compacted) => {
                    return Ok(ProcessOutput {
                        success: !failed,
                        code: Some(if failed { 1 } else { 0 }),
                        stdout: output,
                        stderr: String::new(),
                    });
                }
                _ => {}
            }
        }
    }
}

impl<U: Ui> AgentBackend for OpenCodeBackend<'_, U> {
    fn name(&self) -> &'static str {
        "OpenCode"
    }

    fn prepare_session(&self, session: SessionMode) -> Result<SessionMode> {
        let client = self.server.client(self.ui)?;
        client.check_version()?;
        let id = match session {
            SessionMode::New { name, .. } => {
                SessionId::new(client.create_session(name.as_deref())?)
            }
            SessionMode::Resume { id } => {
                client.prepare_session(id.as_str())?;
                id
            }
        };
        // Planning checkpoints must retain this ID even if the first turn fails.
        Ok(SessionMode::Resume { id })
    }

    fn invoke(
        &self,
        session: SessionMode,
        prompt: &str,
        activity: &str,
        protocol: StageProtocol,
    ) -> Result<StageResult> {
        let mut current_session = self.prepare_session(session)?.id().clone();
        let client = self.server.client(self.ui)?;
        let mut current_prompt = prompt.to_owned();
        let mut current_activity = activity.to_owned();
        let mut output_recoveries = 0;
        let mut incomplete_stage_recoveries = 0;
        let mut provider_retries = 0;
        let invocation = uuid::Uuid::new_v4().to_string();
        let mut attempt_number = 0;
        let deadline = self
            .stage_timeout
            .and_then(|timeout| Instant::now().checked_add(timeout));

        loop {
            let remaining = deadline.map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO)
            });
            if remaining.is_some_and(|remaining| remaining.is_zero()) {
                return Err(WorkerEscalationRequested::new(
                    "OpenCode stage deadline expired before another request could be dispatched",
                )
                .into());
            }
            attempt_number += 1;
            let mut usage = Attempt::new(
                self.ui,
                self.repo,
                Identity {
                    backend: "opencode",
                    connection: self.launcher.connection_name.as_deref(),
                    model: self.launcher.model.as_deref(),
                    session: current_session.as_str(),
                    invocation: &invocation,
                    attempt: attempt_number,
                    fresh: false,
                },
                &current_activity,
            );
            let attempt = self.invoke_once(
                &current_session,
                &current_prompt,
                &current_activity,
                remaining,
                &mut usage,
            );
            let outcome = match &attempt {
                Ok(OpenCodeAttempt::Complete(result)) => {
                    format!("{:?}", result.signal).to_lowercase()
                }
                Ok(OpenCodeAttempt::OutputLimit(_)) => "output_limit".to_owned(),
                Err(error)
                    if error
                        .downcast_ref::<OpenCodeApiError>()
                        .is_some_and(|e| e.transient) =>
                {
                    "provider_error".to_owned()
                }
                Err(error) => crate::usage::error_outcome(error).to_owned(),
            };
            usage.finish(&outcome);
            match attempt {
                Err(error)
                    if error
                        .downcast_ref::<BackendStreamControlRequested>()
                        .is_some() =>
                {
                    let control = error
                        .downcast_ref::<BackendStreamControlRequested>()
                        .expect("guard established backend stream control")
                        .control()
                        .clone();
                    self.apply_manual_control(
                        &client,
                        &current_session,
                        &control,
                        deadline.map(|d| d.saturating_duration_since(Instant::now())),
                    )?;
                    current_prompt = manual_control_continuation_prompt(protocol, &control);
                    current_activity =
                        format!("{activity} (after {})", manual_control_label(&control));
                }
                Err(error)
                    if error
                        .downcast_ref::<OpenCodeApiError>()
                        .is_some_and(|error| error.transient)
                        && provider_retries < self.max_provider_retries =>
                {
                    provider_retries += 1;
                    let delay =
                        provider_retry_delay(self.provider_retry_base_delay, provider_retries).max(
                            error
                                .downcast_ref::<OpenCodeApiError>()
                                .and_then(|e| e.retry_after)
                                .unwrap_or_default(),
                        );
                    let allowance = deadline
                        .map(|d| d.saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::MAX)
                        .min(
                            self.ui
                                .operating_policy()
                                .map(|g| g.remaining_request_time())
                                .unwrap_or(PROVIDER_RETRY_MAX_DELAY),
                        );
                    if delay >= allowance {
                        return Err(error.context(
                            "provider retry delay exceeds the remaining automatic time allowance; inspect provider status and operating policy before resuming",
                        ));
                    }
                    let session_id = error
                        .downcast_ref::<OpenCodeApiError>()
                        .and_then(|error| error.session_id.clone())
                        .unwrap_or_else(|| current_session.clone());
                    self.ui.warn(&format!(
                        "Transient OpenCode provider failure; retrying the same session in {} second(s) ({provider_retries}/{})",
                        delay.as_secs(),
                        self.max_provider_retries
                    ));
                    let retry_at = Instant::now().checked_add(delay).context("OpenCode provider retry timing exceeds the supported clock range; retry manually after the provider limit clears")?;
                    let mut pending_controls = Vec::new();
                    while Instant::now() < retry_at {
                        if let Some(guard) = self.ui.operating_policy() {
                            guard.poll()?;
                        }
                        if deadline.is_some_and(|d| Instant::now() >= d) {
                            return Err(error.context(
                                "OpenCode stage deadline expired during provider retry backoff",
                            ));
                        }
                        if self.stream_transport {
                            match self.ui.poll_stream() {
                                StreamControl::None => {},
                                StreamControl::Pause => return Err(PauseRequested::new(self.resume_repo.clone().unwrap_or_else(||self.repo.to_path_buf())).into()),
                                StreamControl::Escalate => return Err(WorkerEscalationRequested::new("operator requested frontier escalation during provider backoff").into()),
                                StreamControl::Interrupt => bail!("OpenCode provider retry interrupted; resume the preserved checkpoint to continue"),
                                control => { pending_controls.push(control); self.ui.stream_message_sent("Queued until the provider retry delay ends"); },
                            }
                        }
                        thread::sleep(
                            retry_at
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(50)),
                        );
                    }
                    current_session = session_id;
                    current_prompt = transient_provider_continuation_prompt(protocol);
                    for control in pending_controls {
                        self.apply_manual_control(
                            &client,
                            &current_session,
                            &control,
                            deadline.map(|d| d.saturating_duration_since(Instant::now())),
                        )?;
                        if let StreamControl::Inject(message) = control {
                            current_prompt.push_str(&format!(
                                "\n\nOperator direction while waiting for the provider:\n{message}"
                            ));
                        }
                    }
                    current_activity = format!(
                        "{activity} (provider recovery {provider_retries}/{})",
                        self.max_provider_retries
                    );
                }
                Err(error)
                    if error
                        .downcast_ref::<OpenCodeApiError>()
                        .is_some_and(|error| error.transient) =>
                {
                    return Err(error.context(format!(
                        "transient provider failure persisted after {} recovery attempt(s)",
                        self.max_provider_retries
                    )));
                }
                Err(error) => return Err(error),
                Ok(OpenCodeAttempt::OutputLimit(session_id))
                    if output_recoveries < self.max_output_retries =>
                {
                    output_recoveries += 1;
                    self.ui.warn(&format!(
                        "OpenCode reached its output token limit; continuing the same phase ({output_recoveries}/{})",
                        self.max_output_retries
                    ));
                    current_session = session_id;
                    current_prompt = output_limit_continuation_prompt(protocol);
                    current_activity = format!(
                        "{activity} (output-limit continuation {output_recoveries}/{})",
                        self.max_output_retries
                    );
                }
                Ok(OpenCodeAttempt::OutputLimit(_)) => {
                    return Err(WorkerEscalationRequested::new(format!(
                        "local worker exhausted {} output-limit recovery attempt(s)",
                        self.max_output_retries
                    ))
                    .into());
                }
                Ok(OpenCodeAttempt::Complete(result))
                    if result.signal == StageSignal::Incomplete =>
                {
                    if let Some(guard) = self.ui.operating_policy() {
                        guard.incomplete_stage(&result)?;
                    }
                    if incomplete_stage_recoveries >= MAX_INCOMPLETE_STAGE_RECOVERIES {
                        if protocol == StageProtocol::Verify {
                            return Ok(result);
                        }
                        if self.ui.operating_policy().is_none() {
                            bail!(
                                "OpenCode phase remains INCOMPLETE after one continuation without durable operating-policy accounting. Remaining work: {}",
                                result.text
                            );
                        }
                    }
                    incomplete_stage_recoveries += 1;
                    self.ui.warn(
                        "OpenCode reported unfinished work; continuing the same session within the remaining stage allowance",
                    );
                    current_prompt = format!(
                        "{}{}",
                        incomplete_stage_continuation_prompt(protocol),
                        result.continuation_context()
                    );
                    if let Some(id) = result.session_id {
                        current_session = SessionId::new(id);
                    }
                    current_activity = format!(
                        "{activity} (unfinished-phase continuation {incomplete_stage_recoveries})"
                    );
                }
                Ok(OpenCodeAttempt::Complete(result)) => return Ok(result),
            }
        }
    }

    fn rename_session(&self, session_id: &SessionId, name: &str) -> Result<()> {
        self.server
            .client(self.ui)?
            .rename_session(session_id.as_str(), name)
    }

    fn compact_session(&self, session_id: &SessionId, completed_phase: &str) -> Result<()> {
        self.ui.info(&format!(
            "Hard-compacting OpenCode session after {completed_phase}"
        ));
        self.observed_compaction(session_id, None)?;
        self.ui
            .success(&format!("Compacted OpenCode session {session_id}"));
        Ok(())
    }
}

fn new_input_id() -> String {
    format!("msg_{}", uuid::Uuid::new_v4().simple())
}

// Normalize V2 events into the existing backend-neutral usage/display input.
// Durable event IDs retain deduplication; provider details stay intact.
fn normalize_v2_event(event: &Value, tools: &mut BTreeMap<String, Value>) -> Option<Value> {
    let data = &event["data"];
    let session = data.get("sessionID")?;
    let kind = event.get("type")?.as_str()?;
    let mut part = serde_json::json!({"id":event["id"], "modelID":data.pointer("/model/id")});
    let normalized = match kind {
        "session.step.started" => "step_start",
        "session.text.ended" | "session.reasoning.ended" => {
            part["text"] = data["text"].clone();
            if kind == "session.text.ended" {
                "text"
            } else {
                "reasoning"
            }
        }
        "session.step.ended" | "session.compaction.ended" => {
            part["tokens"] = data["tokens"].clone();
            part["cost"] = data["cost"].clone();
            part["reason"] = data["finish"].clone();
            "step_finish"
        }
        "session.step.failed" | "session.execution.failed" | "session.compaction.failed" => {
            return Some(
                serde_json::json!({"type":"error", "sessionID":session, "error":data["error"]}),
            );
        }
        "session.tool.input.started"
        | "session.tool.called"
        | "session.tool.success"
        | "session.tool.failed" => {
            let id = data.get("id")?.as_str()?;
            let key = format!("{}:{id}", data.get("assistantMessageID")?.as_str()?);
            let tool = tools
                .entry(key.clone())
                .or_insert_with(|| serde_json::json!({"id":id, "tool":"tool", "state":{}}));
            if let Some(name) = data.get("name") {
                tool["tool"] = name.clone();
            }
            if let Some(input) = data.get("input") {
                tool["state"]["input"] = input.clone();
            }
            tool["state"]["status"] = match kind {
                "session.tool.success" => "completed",
                "session.tool.failed" => "error",
                _ => "running",
            }
            .into();
            if let Some(content) = data.get("content").and_then(Value::as_array) {
                tool["state"]["output"] = content
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into();
            }
            if let Some(metadata) = data.get("metadata") {
                tool["state"]["metadata"] = metadata.clone();
            }
            if let Some(error) = data.get("error") {
                tool["state"]["error"] = error["message"].clone();
            }
            part = tool.clone();
            if matches!(kind, "session.tool.success" | "session.tool.failed") {
                tools.remove(&key);
            }
            "tool_use"
        }
        _ => return None,
    };
    Some(serde_json::json!({"type":normalized, "sessionID":session, "part":part}))
}

#[derive(Debug)]
struct Invocation {
    command: Option<String>,
    skill: Option<String>,
    message: String,
}

fn resolve_invocation(repo: &Path, prompt: &str) -> Invocation {
    let trimmed = prompt.trim_start();
    let Some(after_slash) = trimmed.strip_prefix('/') else {
        return Invocation {
            command: None,
            skill: None,
            message: prompt.to_owned(),
        };
    };
    let command_end = after_slash
        .find(char::is_whitespace)
        .unwrap_or(after_slash.len());
    let name = &after_slash[..command_end];
    let arguments = after_slash[command_end..].trim_start();
    if matches!(name, "explore-unattended" | "propose-unattended") || skill_exists(repo, name) {
        Invocation {
            command: None,
            skill: Some(name.to_owned()),
            message: arguments.to_owned(),
        }
    } else {
        Invocation {
            command: Some(name.to_owned()),
            skill: None,
            message: arguments.to_owned(),
        }
    }
}

fn skill_exists(repo: &Path, name: &str) -> bool {
    [
        repo.join(".opencode/skills").join(name).join("SKILL.md"),
        repo.join(".opencode/skill").join(name).join("SKILL.md"),
        repo.join(".claude/skills").join(name).join("SKILL.md"),
        repo.join(".agents/skills").join(name).join("SKILL.md"),
    ]
    .iter()
    .any(|path| path.is_file())
}

fn parse_opencode_output(stdout: &str) -> Result<StageResult> {
    let events = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    if let Some(error) = events.iter().rev().find_map(opencode_api_error) {
        return Err(error.into());
    }
    if let Some(error) = events.iter().rev().find_map(opencode_event_error) {
        bail!("OpenCode stage failed: {error}");
    }
    let session_id = events
        .iter()
        .rev()
        .find_map(|event| event.get("sessionID").and_then(Value::as_str))
        .map(str::to_owned);
    let text = events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|event| event.pointer("/part/text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let signal = parse_terminal_signal(&text).ok_or_else(|| {
        MissingTerminalResult::new(
            "OpenCode response did not contain an `OPSX_STATUS` terminal marker; rerun with --stream-agent=raw to inspect it",
            Some(&text),
        )
    })?;
    Ok(StageResult {
        review: crate::review::Report::from_text(&text)?,
        text,
        session_id,
        signal,
    }
    .normalize_review())
}

fn opencode_stage_failure(output: &ProcessOutput) -> anyhow::Error {
    let events = output
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    if let Some(error) = events.iter().rev().find_map(opencode_api_error) {
        return error.into();
    }
    if let Some(error) = events.iter().rev().find_map(opencode_event_error) {
        anyhow::anyhow!("OpenCode stage failed: {error}")
    } else {
        anyhow::anyhow!("OpenCode stage failed: {}", diagnostic_text(output))
    }
}

fn opencode_api_error(event: &Value) -> Option<OpenCodeApiError> {
    if event.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    let error = event.get("error")?;
    let message = error.get("message").and_then(Value::as_str)?;
    let status = error.get("status").and_then(Value::as_u64);
    let kind = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let transient = match kind {
        "provider.rate-limit" | "provider.transport" | "provider.internal" | "provider.timeout" => {
            true
        }
        "provider.quota"
        | "provider.free-limit"
        | "provider.budget-limit"
        | "provider.model-budget-limit"
        | "provider.auth"
        | "provider.content-filter"
        | "provider.invalid-request"
        | "provider.unsupported-operation"
        | "provider.no-route"
        | "provider.unknown" => false,
        _ => matches!(status, Some(408 | 425 | 429 | 500..=599)),
    };
    Some(OpenCodeApiError {
        message: format!("OpenCode {kind}: {message}; provider details: {error}"),
        transient,
        session_id: event
            .get("sessionID")
            .and_then(Value::as_str)
            .map(SessionId::new),
        retry_after: error
            .get("retryAfterMs")
            .and_then(Value::as_u64)
            .map(Duration::from_millis),
    })
}

fn output_hit_token_limit(output: &ProcessOutput) -> bool {
    let event_limit = output
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|event| {
            event.pointer("/error/name").and_then(Value::as_str) == Some("MessageOutputLengthError")
                || (event.get("type").and_then(Value::as_str) == Some("step_finish")
                    && event.pointer("/part/reason").and_then(Value::as_str) == Some("length"))
        });
    event_limit || text_mentions_output_limit(&format!("{}\n{}", output.stdout, output.stderr))
}

fn text_mentions_output_limit(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("messageoutputlengtherror")
        || text.contains("max output tokens")
        || text.contains("maximum output tokens")
        || text.contains("output token limit")
}

fn provider_retry_delay(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1_u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    base.saturating_mul(multiplier)
        .min(PROVIDER_RETRY_MAX_DELAY)
}

fn output_limit_continuation_prompt(protocol: StageProtocol) -> String {
    stage_prompt(
        "",
        "The preceding agent turn reached its output token limit before this opsx-build phase produced a terminal result. Continue the same phase from the existing OpenCode session and durable repository state. Preserve correct completed work, do not restart the phase, and finish the outstanding work now.",
        protocol,
    )
}

fn transient_provider_continuation_prompt(protocol: StageProtocol) -> String {
    stage_prompt(
        "",
        "The preceding model request was interrupted by a transient provider or network failure. Continue the same opsx-build phase in this OpenCode session from its durable repository and OpenSpec state. Preserve correct partial work, do not create a duplicate change or restart completed tasks, and finish the outstanding work now.",
        protocol,
    )
}

fn manual_control_label(control: &StreamControl) -> &'static str {
    match control {
        StreamControl::Compact => "manual compaction",
        StreamControl::Context => "context inspection",
        StreamControl::Inject(_) => "operator steering",
        _ => "live control",
    }
}

fn manual_control_continuation_prompt(protocol: StageProtocol, control: &StreamControl) -> String {
    let instruction = match control {
        StreamControl::Compact => {
            "The operator interrupted the preceding turn and compacted this OpenCode session. Continue the same opsx-build phase from the compacted session and durable repository state. Preserve correct partial work, do not restart completed tasks, and finish the outstanding phase."
                .to_owned()
        }
        StreamControl::Context => {
            "The operator interrupted the preceding turn to inspect this OpenCode session's context usage. Continue the same opsx-build phase from its durable repository state. Preserve correct partial work, do not restart completed tasks, and finish the outstanding phase."
                .to_owned()
        }
        StreamControl::Inject(message) => format!(
            "The operator interrupted the preceding turn with this steering direction:\n\n{message}\n\nApply that direction to the current opsx-build phase. Preserve correct partial work and continue until the phase reaches its required terminal result."
        ),
        _ => unreachable!("only manual restart controls need continuation prompts"),
    };
    stage_prompt("", &instruction, protocol)
}

fn opencode_event_error(event: &Value) -> Option<String> {
    if event.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    event
        .pointer("/error/data/message")
        .or_else(|| event.pointer("/error/message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| event.get("error").map(Value::to_string))
}

fn filter_opencode_line(line: &str, filter: StreamFilter) -> Vec<StreamItem> {
    if filter == StreamFilter::Raw {
        return vec![StreamItem::Raw(line.to_owned())];
    }
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return vec![StreamItem::Lifecycle(format!(
            "Unparsed OpenCode output: {line}"
        ))];
    };
    match event.get("type").and_then(Value::as_str) {
        Some("text") => event
            .pointer("/part/text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| vec![StreamItem::OpenCode(text.to_owned())])
            .unwrap_or_default(),
        Some("reasoning") if filter.includes_reasoning() => event
            .pointer("/part/text")
            .and_then(Value::as_str)
            .map(|text| vec![StreamItem::Reasoning(text.to_owned())])
            .unwrap_or_default(),
        Some("tool_use") => {
            let tool = event
                .pointer("/part/tool")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let summary = event
                .pointer("/part/state/input")
                .and_then(tool_input_summary)
                .unwrap_or_default();
            let text = if summary.is_empty() {
                tool.to_owned()
            } else {
                format!("{tool}: {summary}")
            };
            vec![StreamItem::Tool(text)]
        }
        Some(kind) if filter == StreamFilter::Full => {
            vec![StreamItem::Lifecycle(format!("OpenCode event: {kind}"))]
        }
        _ => Vec::new(),
    }
}

fn tool_input_summary(input: &Value) -> Option<String> {
    for key in ["command", "filePath", "path", "pattern", "query", "name"] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return Some(one_line(value));
        }
    }
    input
        .as_object()
        .filter(|object| !object.is_empty())
        .map(|_| one_line(&input.to_string()))
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

fn with_launcher_environment(spec: CommandSpec, launcher: &OpenCodeLauncher) -> CommandSpec {
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
        bail!("invalid environment variable name `{name}` in OpenCode connection profile");
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
        bail!("OpenCode connection command ends with an incomplete escape");
    }
    if quote != Quote::None {
        bail!("OpenCode connection command contains an unterminated quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{backend::StageSignal, cli::BackendKind};
    #[cfg(unix)]
    use crate::{
        stream::{StreamControl, StreamItem},
        ui::Ui,
    };
    use uuid::Uuid;

    #[cfg(unix)]
    #[derive(Default)]
    struct QuietUi {
        controls: std::sync::Mutex<std::collections::VecDeque<StreamControl>>,
        after_stream: std::sync::Mutex<Option<StreamControl>>,
    }

    #[cfg(unix)]
    impl Ui for QuietUi {
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
            self.controls
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(StreamControl::None)
        }
        fn stream_message_sent(&self, _: &str) {}
        fn stream_item(&self, _: &StreamItem) {}
        fn finish_stream(&self, _: bool, _: &str) {
            if let Some(control) = self.after_stream.lock().unwrap().take() {
                self.controls.lock().unwrap().push_back(control);
            }
        }
        fn finish_dashboard(&self) {}
        fn output(&self, _: &str, _: &str) {}
        fn start_activity(&self, _: &str) -> Option<indicatif::ProgressBar> {
            None
        }
        fn finish_activity(&self, _: Option<indicatif::ProgressBar>, _: bool, _: &str) {}
    }

    fn launcher() -> OpenCodeLauncher {
        OpenCodeLauncher {
            connection_name: Some("open".to_owned()),
            environment_name: None,
            program: "opencode".to_owned(),
            prefix_args: Vec::new(),
            model: Some("openrouter/qwen/qwen3.6-35b-a3b".to_owned()),
            context_window: Some(262_144),
            environment: Vec::new(),
            unset_environment: Vec::new(),
        }
    }

    #[test]
    fn constructs_new_and_resumed_run_commands() {
        let repo = Path::new("/repo");
        let fresh = build_opencode_command(
            repo,
            &launcher(),
            &SessionMode::New {
                id: SessionId::new("provisional"),
                name: Some("slice-apply".to_owned()),
            },
            "do the work",
        );
        assert_eq!(
            fresh.args,
            [
                "run",
                "--standalone",
                "--format",
                "json",
                "--auto",
                "--model",
                "openrouter/qwen/qwen3.6-35b-a3b",
                "--title",
                "slice-apply",
                "do the work"
            ]
        );

        let resumed = build_opencode_command(
            repo,
            &launcher(),
            &SessionMode::Resume {
                id: SessionId::new("ses_123"),
            },
            "continue",
        );
        assert!(
            resumed
                .args
                .windows(2)
                .any(|pair| pair == ["--session", "ses_123"])
        );
    }

    #[test]
    fn constructs_private_v2_server_with_a_stdin_lease_and_redacted_password() {
        let mut launcher = launcher();
        launcher.environment.push(LauncherEnvironment {
            name: "OPENCODE_PASSWORD".into(),
            value: "inherited-secret".into(),
            redacted: true,
        });
        let server = build_opencode_server_command(Path::new("/repo"), &launcher)
            .secret_env("OPENCODE_PASSWORD", "lease-secret");
        assert_eq!(
            server.args,
            ["serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0"]
        );
        assert!(!server.display().contains("lease-secret"));
        assert!(!server.display().contains("inherited-secret"));
        assert!(server.display().contains("OPENCODE_PASSWORD=<redacted>"));
        assert!(
            server
                .env_remove
                .iter()
                .any(|n| n == "OPENCODE_SERVER_PASSWORD")
        );
    }

    #[test]
    fn constructs_interactive_command_from_the_same_profile() {
        let command = build_interactive_opencode_command(
            Path::new("/repo"),
            &launcher(),
            true,
            &["--mini".to_owned()],
            Some("inspect this project"),
        );

        assert_eq!(
            command.args,
            [
                "--model",
                "openrouter/qwen/qwen3.6-35b-a3b",
                "--auto",
                "--mini",
                "--prompt",
                "inspect this project"
            ]
        );
    }

    #[test]
    fn turns_a_visible_skill_invocation_into_a_skill_tool_instruction() {
        let repo = std::env::temp_dir().join(format!("opsx-opencode-skill-{}", Uuid::new_v4()));
        let skill = repo.join(".claude/skills/propose-unattended/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "---\nname: propose-unattended\n---\n").unwrap();

        let command = build_opencode_command(
            &repo,
            &launcher(),
            &SessionMode::New {
                id: SessionId::new("provisional"),
                name: None,
            },
            "/propose-unattended slice one\n\nOPSX_STATUS required",
        );
        assert!(!command.args.iter().any(|arg| arg == "--command"));
        assert!(command.args.last().unwrap().contains("skill tool"));
        assert!(command.args.last().unwrap().contains("slice one"));
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn dry_run_recognizes_bundled_skills_before_installation() {
        let command = build_opencode_command(
            Path::new("/repo-without-installed-skills"),
            &launcher(),
            &SessionMode::New {
                id: SessionId::new("provisional"),
                name: None,
            },
            "/explore-unattended inspect it",
        );

        assert!(!command.args.iter().any(|arg| arg == "--command"));
        assert!(command.args.last().unwrap().contains("skill tool"));
    }

    #[test]
    fn resolves_native_skills_and_command_callbacks_separately() {
        let skill = resolve_invocation(Path::new("/repo"), "/propose-unattended assigned work");
        assert_eq!(skill.skill.as_deref(), Some("propose-unattended"));
        assert!(skill.command.is_none());
        assert_eq!(skill.message, "assigned work");
        let command = resolve_invocation(Path::new("/repo"), "/opsx:apply change-name");
        assert_eq!(command.command.as_deref(), Some("opsx:apply"));
        assert!(command.skill.is_none());
        assert_eq!(command.message, "change-name");
    }

    #[test]
    fn opencode_marker_output_preserves_review_evidence() {
        let report: Value =
            serde_json::from_str(include_str!("../tests/fixtures/review-report.json")).unwrap();
        let event = serde_json::json!({"type":"text", "sessionID":"review-session", "part":{"text":format!("OPSX_REVIEW: {report}\nOPSX_STATUS: VERIFIED")}});
        let result = parse_opencode_output(&event.to_string()).unwrap();
        assert_eq!(result.signal, StageSignal::Verified);
        assert_eq!(result.review.unwrap().areas[0].id, "required-outcome");
    }

    #[test]
    fn parses_terminal_text_and_real_session_id() {
        let result = parse_opencode_output(
            r#"{"type":"step_start","sessionID":"ses_abc","part":{"type":"step-start"}}
{"type":"text","sessionID":"ses_abc","part":{"type":"text","text":"Implemented it.\nOPSX_STATUS: READY"}}"#,
        )
        .unwrap();
        assert_eq!(result.session_id.as_deref(), Some("ses_abc"));
        assert_eq!(result.signal, StageSignal::Ready);
        assert!(result.text.contains("Implemented it."));
    }

    #[test]
    fn labels_streamed_model_text_as_opencode() {
        let items = filter_opencode_line(
            r#"{"type":"text","sessionID":"ses_abc","part":{"type":"text","text":"Working on it."}}"#,
            StreamFilter::Activity,
        );

        assert_eq!(items, [StreamItem::OpenCode("Working on it.".to_owned())]);
    }

    #[test]
    fn validates_an_agentic_connection_tool_round_trip() {
        let output = ProcessOutput {
            success: true,
            code: Some(0),
            stdout: format!(
                concat!(
                    r#"{{"type":"tool_use","sessionID":"ses_test","part":{{"type":"tool","tool":"bash","state":{{"input":{{"command":"printf '{tool}\\n'"}},"output":"{tool}"}}}}}}"#,
                    "\n",
                    r#"{{"type":"text","sessionID":"ses_test","part":{{"type":"text","text":"{result}"}}}}"#
                ),
                tool = CONNECTION_TOOL_MARKER,
                result = CONNECTION_TEST_MARKER,
            ),
            stderr: String::new(),
        };

        assert_eq!(
            parse_connection_test_output(&output, ConnectionTestMode::Agentic).unwrap(),
            CONNECTION_TEST_MARKER
        );
    }

    #[test]
    fn classifies_typed_provider_and_output_limit_events() {
        let transient: Value = serde_json::from_str(
            r#"{"type":"error","sessionID":"ses_retry","error":{"type":"provider.rate-limit","message":"busy","status":429,"retryAfterMs":1}}"#,
        )
        .unwrap();
        let error = opencode_api_error(&transient).unwrap();
        assert!(error.transient);
        assert_eq!(
            error.session_id.as_ref().map(SessionId::as_str),
            Some("ses_retry")
        );

        let output = ProcessOutput {
            success: false,
            code: Some(1),
            stdout: r#"{"type":"error","sessionID":"ses_limit","error":{"name":"MessageOutputLengthError","data":{}}}"#.to_owned(),
            stderr: String::new(),
        };
        assert!(output_hit_token_limit(&output));
        assert_eq!(error.retry_after, Some(Duration::from_millis(1)));
    }

    fn test_backend<'a, U: Ui>(
        repo: &'a Path,
        launcher: &'a OpenCodeLauncher,
        ui: &'a U,
        server: &crate::opencode_server::tests::FakeServer,
    ) -> OpenCodeBackend<'a, U> {
        OpenCodeBackend::new(repo, launcher, true, None, ui).with_server(Arc::new(
            OpenCodeServer::external(repo, launcher, server.url.clone()),
        ))
    }

    fn new_session() -> SessionMode {
        SessionMode::New {
            id: SessionId::new("provisional"),
            name: None,
        }
    }

    #[test]
    fn explicit_incomplete_reuses_the_session_and_has_a_separate_bounded_recovery() {
        use crate::opencode_server::tests::{FakeServer, turn};
        for (protocol, first, second, count, expected) in [
            (
                StageProtocol::Worker,
                "INCOMPLETE",
                "READY",
                2,
                Some(StageSignal::Ready),
            ),
            (StageProtocol::Worker, "INCOMPLETE", "INCOMPLETE", 2, None),
            (
                StageProtocol::Worker,
                "BLOCKED",
                "READY",
                1,
                Some(StageSignal::Blocked),
            ),
            (
                StageProtocol::Verify,
                "INCOMPLETE",
                "INCOMPLETE",
                2,
                Some(StageSignal::Incomplete),
            ),
        ] {
            let repo = std::env::temp_dir().join(format!("opsx-v2-{}", Uuid::new_v4()));
            fs::create_dir_all(&repo).unwrap();
            let server = FakeServer::start(
                &repo,
                vec![
                    turn(&format!("OPSX_STATUS: {first}")),
                    turn(&format!("OPSX_STATUS: {second}")),
                ],
            );
            let launcher = launcher();
            let ui = crate::usage::tests::RecordingUi::default();
            let backend = test_backend(&repo, &launcher, &ui, &server);
            let result = backend.invoke(new_session(), "finish work", "test phase", protocol);
            match expected {
                Some(signal) => assert_eq!(result.unwrap().signal, signal),
                None => assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("remains INCOMPLETE after one continuation")
                ),
            }
            let requests = server.requests.lock().unwrap();
            assert_eq!(requests.iter().filter(|(line,_,_)|line.starts_with("POST /api/session/ses_created/prompt ")).count(),count);
            assert_eq!(
                requests
                    .iter()
                    .filter(|(line, _, _)| line.starts_with("POST /api/session "))
                    .count(),
                1
            );
            assert_eq!(ui.records.lock().unwrap().len(), count);
            fs::remove_dir_all(repo).unwrap();
        }
    }

    #[test]
    fn prepared_planning_session_survives_a_failed_first_turn() {
        use crate::opencode_server::tests::{FakeServer, event, turn};
        let server = FakeServer::start(
            Path::new("/repo"),
            vec![
                vec![event(
                    "session.execution.failed",
                    serde_json::json!({"error":{"type":"provider.rate-limit","status":429,"message":"busy"}}),
                )],
                turn("OPSX_STATUS: READY"),
            ],
        );
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server);
        let prepared = backend.prepare_session(new_session()).unwrap();
        assert_eq!(prepared.id().as_str(), "ses_created");
        assert!(
            backend
                .invoke(prepared.clone(), "plan", "Propose", StageProtocol::Propose)
                .is_err()
        );
        assert_eq!(
            backend
                .invoke(
                    prepared,
                    "continue planning",
                    "Propose",
                    StageProtocol::Propose
                )
                .unwrap()
                .signal,
            StageSignal::Ready
        );
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _, _)| p == "POST /api/session HTTP/1.1")
                .count(),
            1
        );
    }

    #[test]
    fn v2_provider_failures_retain_details_and_retry_only_within_allowance() {
        use crate::opencode_server::tests::{FakeServer, event, turn};
        for (kind, tries, success) in [
            ("provider.rate-limit", 1, true),
            ("provider.rate-limit", 0, false),
            ("provider.quota", 2, false),
        ] {
            let server = FakeServer::start(
                Path::new("/repo"),
                vec![
                    vec![event(
                        "session.execution.failed",
                        serde_json::json!({"error":{"type":kind,"status":429,"message":"provider rejected request","response":{"body":"limit-specific evidence"},"retryAfterMs":1}}),
                    )],
                    turn("OPSX_STATUS: READY"),
                ],
            );
            let launcher = launcher();
            let ui = crate::usage::tests::RecordingUi::default();
            let mut backend =
                test_backend(Path::new("/repo"), &launcher, &ui, &server).with_retries(0, tries);
            backend.provider_retry_base_delay = Duration::ZERO;
            let result =
                backend.invoke(new_session(), "finish work", "Apply", StageProtocol::Worker);
            if success {
                assert_eq!(result.unwrap().signal, StageSignal::Ready);
            } else {
                let error = format!("{:#}", result.unwrap_err());
                assert!(
                    error.contains("limit-specific evidence") && error.contains("429"),
                    "{error}"
                );
            }
            assert_eq!(
                ui.records.lock().unwrap().len(),
                if success { 2 } else { 1 }
            );
        }
    }

    #[test]
    fn output_limit_recovery_and_usage_use_native_step_events() {
        use crate::opencode_server::tests::{FakeServer, turn};
        let mut limited = turn("partial output");
        limited[1]["data"]["finish"] = "length".into();
        let server = FakeServer::start(
            Path::new("/repo"),
            vec![limited, turn("OPSX_STATUS: READY")],
        );
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server).with_retries(1, 0);
        assert_eq!(
            backend
                .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
                .unwrap()
                .signal,
            StageSignal::Ready
        );
        let records = ui.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].outcome, "output_limit");
        assert_eq!(records[1].tokens.input, Some(15));
        assert_eq!(records[1].tokens.output, Some(5));
        assert_eq!(records[1].session_id, "ses_created");
    }

    #[test]
    fn v2_submits_native_skill_attachments_and_preserves_command_callbacks() {
        use crate::opencode_server::tests::{FakeServer, turn};
        let server = FakeServer::start(
            Path::new("/repo"),
            vec![turn("OPSX_STATUS: READY"), turn("OPSX_STATUS: READY")],
        );
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server);
        backend
            .invoke(
                new_session(),
                "/propose-unattended assigned work",
                "Propose",
                StageProtocol::Propose,
            )
            .unwrap();
        backend
            .invoke(
                SessionMode::Resume {
                    id: SessionId::new("ses_created"),
                },
                "/custom-command assigned work",
                "Apply",
                StageProtocol::Worker,
            )
            .unwrap();
        let requests = server.requests.lock().unwrap();
        let prompt = &requests
            .iter()
            .find(|(p, _, _)| p.starts_with("POST /api/session/ses_created/prompt "))
            .unwrap()
            .1;
        assert_eq!(
            prompt["skills"],
            serde_json::json!([{"id":"propose-unattended"}])
        );
        assert!(
            prompt["text"]
                .as_str()
                .unwrap()
                .contains("no structured result schema")
        );
        let command = &requests
            .iter()
            .find(|(p, _, _)| p.starts_with("POST /api/session/ses_created/command "))
            .unwrap()
            .1;
        assert_eq!(command["name"], "custom-command");
        assert!(command.get("command").is_none());
    }

    #[test]
    fn stream_loss_and_execution_failure_do_not_accept_a_ready_marker() {
        use crate::opencode_server::tests::{FakeServer, event, turn};
        for last in [
            serde_json::json!({"_disconnect":true}),
            event(
                "session.execution.failed",
                serde_json::json!({"error":{"type":"unknown","message":"failed after text"}}),
            ),
        ] {
            let mut events = turn("OPSX_STATUS: READY");
            events.truncate(1);
            events.push(last);
            let server = FakeServer::start(Path::new("/repo"), vec![events]);
            let launcher = launcher();
            let ui = crate::usage::tests::RecordingUi::default();
            let backend =
                test_backend(Path::new("/repo"), &launcher, &ui, &server).with_retries(0, 2);
            assert!(
                backend
                    .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
                    .is_err()
            );
            assert_eq!(ui.records.lock().unwrap().len(), 1);
            assert!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(p, _, _)| p.contains("/interrupt "))
            );
        }
    }

    #[test]
    fn stale_text_before_delivery_cannot_complete_the_new_input() {
        use crate::opencode_server::tests::{FakeServer, event};
        let server = FakeServer::start(
            Path::new("/repo"),
            vec![vec![event(
                "session.execution.succeeded",
                serde_json::json!({}),
            )]],
        );
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let error = test_backend(Path::new("/repo"), &launcher, &ui, &server)
            .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
            .unwrap_err();
        assert!(error.downcast_ref::<MissingTerminalResult>().is_some());
    }

    #[test]
    fn unconfirmed_cancellation_refuses_automatic_retry() {
        use crate::opencode_server::tests::{FakeServer, event};
        let mut failure = event(
            "session.execution.failed",
            serde_json::json!({"error":{"type":"provider.rate-limit","status":429,"message":"busy"}}),
        );
        failure["_interrupt_error"] = true.into();
        let server = FakeServer::start(Path::new("/repo"), vec![vec![failure]]);
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server).with_retries(0, 2);
        let error = backend
            .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
            .unwrap_err();
        assert!(error.to_string().contains("cancellation is unconfirmed"));
        assert!(error.downcast_ref::<OpenCodeApiError>().is_none());
        assert_eq!(ui.records.lock().unwrap().len(), 1);
    }

    #[test]
    fn child_sessions_cannot_supply_the_parent_result() {
        use crate::opencode_server::tests::{FakeServer, event, turn};
        let mut events = vec![
            event(
                "session.text.ended",
                serde_json::json!({"sessionID":"ses_child","text":"OPSX_STATUS: BLOCKED"}),
            ),
            event(
                "session.execution.succeeded",
                serde_json::json!({"sessionID":"ses_child"}),
            ),
        ];
        events.extend(turn("OPSX_STATUS: READY"));
        let server = FakeServer::start(Path::new("/repo"), vec![events]);
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        assert_eq!(
            test_backend(Path::new("/repo"), &launcher, &ui, &server)
                .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
                .unwrap()
                .signal,
            StageSignal::Ready
        );
    }

    #[test]
    fn compaction_waits_for_completion_and_records_actual_usage() {
        use crate::opencode_server::tests::{FakeServer, event};
        let mut ended = event(
            "session.compaction.ended",
            serde_json::json!({"tokens":{"input":10,"output":3,"reasoning":2,"cache":{"read":4,"write":1}},"cost":0.01}),
        );
        ended["_delay_ms"] = 60.into();
        let server = FakeServer::start(
            Path::new("/repo"),
            vec![vec![
                ended,
                event("session.execution.succeeded", serde_json::json!({})),
            ]],
        );
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server);
        let start = Instant::now();
        backend
            .observed_compaction(&SessionId::new("ses_created"), None)
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(60));
        assert_eq!(ui.records.lock().unwrap()[0].tokens.input, Some(15));
    }

    #[test]
    fn an_expired_stage_deadline_cannot_dispatch_another_input() {
        use crate::opencode_server::tests::FakeServer;
        let server = FakeServer::start(Path::new("/repo"), vec![]);
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let error = test_backend(Path::new("/repo"), &launcher, &ui, &server)
            .with_stage_timeout(Duration::ZERO)
            .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
            .unwrap_err();
        assert!(error.downcast_ref::<WorkerEscalationRequested>().is_some());
        assert!(
            !server
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _, _)| p.contains("/prompt "))
        );
        assert!(ui.records.lock().unwrap().is_empty());
    }

    #[test]
    fn compaction_admission_alone_cannot_complete_and_deadlines_interrupt() {
        use crate::opencode_server::tests::FakeServer;
        let server = FakeServer::start(Path::new("/repo"), vec![vec![]]);
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server)
            .with_stage_timeout(Duration::from_millis(20));
        let error = backend
            .observed_compaction(&SessionId::new("ses_created"), None)
            .unwrap_err();
        assert!(error.downcast_ref::<WorkerEscalationRequested>().is_some());
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _, _)| p.contains("/interrupt "))
        );
        assert_eq!(ui.records.lock().unwrap()[0].outcome, "escalated");
    }

    #[test]
    fn v2_review_repair_and_resume_keep_the_common_result_contract() {
        use crate::opencode_server::tests::{FakeServer, turn};
        let report: Value =
            serde_json::from_str(include_str!("../tests/fixtures/review-report.json")).unwrap();
        let server = FakeServer::start(
            Path::new("/repo"),
            vec![
                turn("OPSX_STATUS: READY"),
                turn("OPSX_STATUS: RETRY"),
                turn("OPSX_STATUS: READY"),
                turn(&format!("OPSX_REVIEW: {report}\nOPSX_STATUS: VERIFIED")),
                turn("OPSX_STATUS: READY"),
            ],
        );
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server);
        for (protocol, signal) in [
            (StageProtocol::Worker, StageSignal::Ready),
            (StageProtocol::Verify, StageSignal::Retry),
            (StageProtocol::Worker, StageSignal::Ready),
            (StageProtocol::Verify, StageSignal::Verified),
            (StageProtocol::Ready, StageSignal::Ready),
        ] {
            let result = backend
                .invoke(
                    SessionMode::Resume {
                        id: SessionId::new("ses_created"),
                    },
                    "continue assigned phase",
                    "phase",
                    protocol,
                )
                .unwrap();
            assert_eq!(result.signal, signal);
            if signal == StageSignal::Verified {
                assert_eq!(result.review.unwrap().areas[0].id, "required-outcome");
            }
        }
        assert_eq!(ui.records.lock().unwrap().len(), 5);
        assert!(
            !server
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _, _)| p == "POST /api/session HTTP/1.1")
        );
    }

    #[cfg(unix)]
    #[test]
    fn durable_request_limit_stops_before_another_v2_admission() {
        use crate::{
            opencode_server::tests::{FakeServer, turn},
            operating_policy::{Policy, Run},
        };
        let repo = std::env::temp_dir().join(format!("opsx-v2-policy-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let ui = crate::usage::tests::RecordingUi::default();
        let run = Run::new(
            &repo,
            Policy {
                max_requests_per_stage: 1,
                ..Policy::default()
            },
            &ui,
        )
        .unwrap();
        ui.set_operating_policy(Arc::new(run.open(None).unwrap()))
            .unwrap();
        let server = FakeServer::start(&repo, vec![turn("OPSX_STATUS: INCOMPLETE")]);
        let launcher = launcher();
        let backend = test_backend(&repo, &launcher, &ui, &server);
        let error = backend
            .invoke(
                new_session(),
                "finish assigned work",
                "Apply",
                StageProtocol::Worker,
            )
            .unwrap_err();
        assert!(crate::operating_policy::is_stop(&error), "{error:#}");
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(p, _, _)| p.contains("/prompt "))
                .count(),
            1
        );
        drop(backend);
        drop(ui);
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn permissions_are_approved_once_and_forms_stop_with_actionable_evidence() {
        use crate::opencode_server::tests::{FakeServer, event, turn};
        let mut events = vec![event("permission.asked", serde_json::json!({"id":"ask_1"}))];
        events.extend(turn("OPSX_STATUS: READY"));
        let server = FakeServer::start(
            Path::new("/repo"),
            vec![
                events,
                vec![event(
                    "form.created",
                    serde_json::json!({"form":{"sessionID":"ses_created","id":"form_1","title":"Need an authoritative choice"}}),
                )],
            ],
        );
        let launcher = launcher();
        let ui = crate::usage::tests::RecordingUi::default();
        let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server);
        backend
            .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
            .unwrap();
        let error = backend
            .invoke(
                SessionMode::Resume {
                    id: SessionId::new("ses_created"),
                },
                "finish",
                "Apply",
                StageProtocol::Worker,
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("form_1") && error.contains("--direction"),
            "{error}"
        );
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(p, b, _)| p.contains("/ask_1/reply ") && b["decision"] == "once")
        );
    }

    #[cfg(unix)]
    #[test]
    fn provider_backoff_remains_pausable_and_keeps_queued_direction() {
        use crate::opencode_server::tests::{FakeServer, event, turn};
        for control in [
            StreamControl::Pause,
            StreamControl::Inject("preserve the public API".into()),
        ] {
            let server = FakeServer::start(
                Path::new("/repo"),
                vec![
                    vec![event(
                        "session.execution.failed",
                        serde_json::json!({"error":{"type":"provider.rate-limit","status":429,"message":"busy","retryAfterMs":50}}),
                    )],
                    turn("OPSX_STATUS: READY"),
                ],
            );
            let launcher = launcher();
            let ui = QuietUi::default();
            *ui.after_stream.lock().unwrap() = Some(control.clone());
            let mut backend =
                test_backend(Path::new("/repo"), &launcher, &ui, &server).with_retries(0, 1);
            backend.provider_retry_base_delay = Duration::ZERO;
            let result = backend.invoke(new_session(), "finish", "Apply", StageProtocol::Worker);
            if control == StreamControl::Pause {
                assert!(
                    result
                        .unwrap_err()
                        .downcast_ref::<PauseRequested>()
                        .is_some()
                );
                assert_eq!(
                    server
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|(p, _, _)| p.contains("/prompt "))
                        .count(),
                    1
                );
            } else {
                assert_eq!(result.unwrap().signal, StageSignal::Ready);
                assert!(
                    server
                        .requests
                        .lock()
                        .unwrap()
                        .iter()
                        .rfind(|(p, _, _)| p.contains("/prompt "))
                        .unwrap()
                        .1["text"]
                        .as_str()
                        .unwrap()
                        .contains("preserve the public API")
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn pause_and_escalation_confirm_interrupt_before_unwinding() {
        use crate::opencode_server::tests::FakeServer;
        for control in [
            StreamControl::Pause,
            StreamControl::Escalate,
            StreamControl::Interrupt,
        ] {
            let server = FakeServer::start(Path::new("/repo"), vec![vec![]]);
            let launcher = launcher();
            let ui = QuietUi::default();
            ui.controls.lock().unwrap().push_back(control.clone());
            let backend = test_backend(Path::new("/repo"), &launcher, &ui, &server);
            let error = backend
                .invoke(new_session(), "finish", "Apply", StageProtocol::Worker)
                .unwrap_err();
            if control == StreamControl::Pause {
                assert!(error.downcast_ref::<PauseRequested>().is_some());
            }
            if control == StreamControl::Escalate {
                assert!(error.downcast_ref::<WorkerEscalationRequested>().is_some());
            }
            assert!(
                server
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(p, _, _)| p.contains("/interrupt "))
            );
        }
    }

    fn isolated_native_launcher(config: Value) -> (PathBuf, OpenCodeLauncher) {
        let binary = std::env::var("OPSX_TEST_OPENCODE").expect("set OPSX_TEST_OPENCODE");
        let repo = std::env::temp_dir().join(format!("opsx-native-opencode-{}", Uuid::new_v4()));
        fs::create_dir_all(repo.join("config")).unwrap();
        fs::write(repo.join("config/opencode.json"), config.to_string()).unwrap();
        let mut launcher = launcher();
        launcher.program = binary;
        launcher.model = None;
        launcher.environment = [
            ("OPENCODE_CONFIG_DIR", "config"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("OPENCODE_DB", "native.sqlite"),
        ]
        .into_iter()
        .map(|(name, path)| LauncherEnvironment {
            name: name.into(),
            value: repo.join(path).display().to_string(),
            redacted: false,
        })
        .collect();
        launcher.unset_environment =
            vec!["OPENCODE_CONFIG".into(), "OPENCODE_CONFIG_CONTENT".into()];
        launcher.environment.push(LauncherEnvironment {
            name: "OPENCODE_DISABLE_FREE_MODELS".into(),
            value: "1".into(),
            redacted: false,
        });
        launcher
            .unset_environment
            .extend(std::env::vars().map(|(name, _)| name).filter(|name| {
                name.ends_with("_API_KEY") || name.ends_with("_TOKEN") || name.ends_with("_SECRET")
            }));
        (repo, launcher)
    }

    #[test]
    #[ignore = "requires OPSX_TEST_OPENCODE pointing to an OpenCode V2 2.2+ executable"]
    fn native_v2_private_server_lifecycle() {
        let (repo, launcher) = isolated_native_launcher(serde_json::json!({"update":"disable"}));
        let ui = crate::usage::tests::RecordingUi::default();
        {
            let server = OpenCodeServer::new(&repo, &launcher);
            let client = server.client(&ui).unwrap();
            let session = client
                .create_session(Some("opsx-build isolated V2 API smoke test"))
                .unwrap();
            client.prepare_session(&session).unwrap();
            client
                .rename_session(&session, "renamed isolated API smoke test")
                .unwrap();
            client.subscribe().unwrap();
            client.abort_session(&session).unwrap();
            assert!(
                client
                    .context_report(&session)
                    .unwrap()
                    .contains("unavailable")
            );
        }
        assert!(
            ui.records.lock().unwrap().is_empty(),
            "API smoke test must not make model calls"
        );
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    #[ignore = "requires OPSX_TEST_OPENCODE; all inference is served by a local fixture"]
    fn native_v2_turn_with_local_provider() {
        use crate::opencode_server::tests::FakeServer;
        let provider = FakeServer::start(Path::new("/fixture"), vec![]);
        let config = serde_json::json!({
            "update":"disable", "enabled_providers":["fixture"],
            "model":"fixture/fixture", "small_model":"fixture/fixture",
            "providers":{"fixture":{
                "name":"isolated fixture provider", "package":"aisdk:@ai-sdk/openai-compatible",
                "settings":{"baseURL":format!("{}/v1",provider.url),"apiKey":"fixture-only"},
                "models":{"fixture":{"name":"fixture","limit":{"context":128000,"output":4096}}}
            }}
        });
        let (repo, mut launcher) = isolated_native_launcher(config);
        launcher.model = Some("fixture/fixture".into());
        let ui = crate::usage::tests::RecordingUi::default();
        {
            let backend = OpenCodeBackend::new(&repo, &launcher, true, None, &ui)
                .with_stage_timeout(Duration::from_secs(30));
            let result = backend
                .invoke(
                    new_session(),
                    "OPSX_NATIVE_TOOL_PROBE: run the isolated shell compatibility probe. Do no repository work. Finish with OPSX_STATUS: READY.",
                    "isolated native V2 turn",
                    StageProtocol::Ready,
                )
                .unwrap();
            assert_eq!(result.signal, StageSignal::Ready);
            let session = result.session_id.unwrap();
            assert!(session.starts_with("ses"));
            backend
                .observed_compaction(&SessionId::new(&session), None)
                .unwrap();
            assert_eq!(
                backend
                    .invoke(
                        SessionMode::Resume {
                            id: SessionId::new(session)
                        },
                        "Continue without repository work. Finish with OPSX_STATUS: READY.",
                        "isolated native V2 resume",
                        StageProtocol::Ready
                    )
                    .unwrap()
                    .signal,
                StageSignal::Ready
            );
        }
        assert!(
            provider
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(p, _, _)| p.contains("/v1/chat/completions"))
        );
        assert_eq!(ui.records.lock().unwrap().len(), 3);
        let records = ui.records.lock().unwrap();
        assert_eq!(records[0].tokens.input, Some(20));
        assert_eq!(records[0].tools.len(), 1);
        assert_eq!(records[0].tools[0].failed, Some(false));
        assert!(records[0].tools[0].result_chars.is_some_and(|n| n > 0));
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn parses_open_code_profile_and_rejects_claude_model_spelling() {
        let connection = AgentConnection {
            name: Some("open".to_owned()),
            backend: BackendKind::OpenCode,
            environment_name: None,
            command: "wrapper opencode".to_owned(),
            model: Some("provider/model".to_owned()),
            permission_profile: None,
            structured_output: true,
            context_window: None,
            auto_compact_window: None,
            auto_compact_percent: None,
            max_output_tokens: None,
            env: Default::default(),
            isolate: true,
            unset_env: Vec::new(),
        };
        let launcher = OpenCodeLauncher::from_connection(&connection).unwrap();
        assert_eq!(launcher.program, "wrapper");
        assert_eq!(launcher.prefix_args, ["opencode"]);

        let mut invalid = connection;
        invalid.model = Some("model-without-provider".to_owned());
        assert!(
            OpenCodeLauncher::from_connection(&invalid)
                .unwrap_err()
                .to_string()
                .contains("provider/model")
        );
    }
}
