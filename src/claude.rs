use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    cli::{ClaudeConnection, Cli, ConnectionEnvironmentValue},
    process::{
        CommandSpec, PauseRequested, ProcessOutput, ProcessRunner, WorkerEscalationRequested,
        diagnostic_text, stream_user_message,
    },
    stream::{StreamFilter, context_report_items, filter_line},
    ui::Ui,
};

const AUTO_COMPACT_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
const AUTO_COMPACT_PERCENT_ENV: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";
const MAX_OUTPUT_TOKENS_ENV: &str = "CLAUDE_CODE_MAX_OUTPUT_TOKENS";
pub const CONNECTION_TEST_MARKER: &str = "OPSX_CONNECTION_OK";
const CLAUDE_CONNECTION_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AWS_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_BEDROCK_MANTLE_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_FOUNDRY_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LauncherEnvironment {
    pub name: String,
    pub value: String,
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeLauncher {
    pub connection_name: Option<String>,
    pub environment_name: Option<String>,
    pub program: String,
    pub prefix_args: Vec<String>,
    pub model: Option<String>,
    pub auto_compact_window: Option<u64>,
    pub auto_compact_percent: Option<u8>,
    pub max_output_tokens: Option<u64>,
    pub environment: Vec<LauncherEnvironment>,
    pub unset_environment: Vec<String>,
}

impl ClaudeLauncher {
    pub fn parse(
        command: &str,
        model: Option<String>,
        auto_compact_window: Option<u64>,
        auto_compact_percent: Option<u8>,
        max_output_tokens: Option<u64>,
    ) -> Result<Self> {
        let mut words = split_command(command)?;
        if words.is_empty() {
            bail!("--claude-command cannot be empty");
        }
        Ok(Self {
            connection_name: None,
            environment_name: None,
            program: words.remove(0),
            prefix_args: words,
            model,
            auto_compact_window,
            auto_compact_percent,
            max_output_tokens,
            environment: Vec::new(),
            unset_environment: Vec::new(),
        })
    }

    pub fn from_connection(connection: &ClaudeConnection) -> Result<Self> {
        let mut launcher = Self::parse(
            &connection.command,
            connection.model.clone(),
            connection.auto_compact_window,
            connection.auto_compact_percent,
            connection.max_output_tokens,
        )?;
        launcher.connection_name = connection.name.clone();
        launcher.environment_name = connection.environment_name.clone();
        if connection.isolate {
            launcher
                .unset_environment
                .extend(CLAUDE_CONNECTION_ENV.iter().map(|name| (*name).to_owned()));
        }
        for name in &connection.unset_env {
            validate_environment_name(name)?;
            if !launcher.unset_environment.contains(name) {
                launcher.unset_environment.push(name.clone());
            }
        }
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
                            "Claude connection profile `{}` requires environment variable `{}` for `{name}`",
                            connection.name.as_deref().unwrap_or("unnamed"),
                            reference.from_env
                        )
                    })?;
                    if reference.from_env != name.as_str()
                        && !launcher.unset_environment.contains(&reference.from_env)
                    {
                        launcher.unset_environment.push(reference.from_env.clone());
                    }
                    (value, true)
                }
            };
            launcher.environment.push(LauncherEnvironment {
                name: name.clone(),
                value,
                redacted,
            });
        }
        Ok(launcher)
    }
}

fn validate_environment_name(name: &str) -> Result<()> {
    let mut characters = name.chars();
    let valid_start = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic());
    if !valid_start
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        bail!("invalid environment variable name `{name}` in Claude connection profile");
    }
    Ok(())
}

fn environment_name_is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["TOKEN", "KEY", "SECRET", "PASSWORD"]
        .iter()
        .any(|marker| name.contains(marker))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommands {
    pub explore: String,
    pub propose: String,
    pub apply: String,
    pub verify: String,
    pub archive: String,
}

impl SkillCommands {
    pub fn discover(repo: &Path, cli: &Cli) -> Result<Self> {
        Ok(Self {
            explore: resolve_bundled_command(cli.explore_command.as_deref(), "explore-unattended"),
            propose: resolve_bundled_command(cli.propose_command.as_deref(), "propose-unattended"),
            apply: resolve_command(
                repo,
                cli.apply_command.as_deref(),
                &[
                    SkillCandidate::skill("openspec-apply-change"),
                    SkillCandidate::command("opsx/apply.md", "opsx:apply"),
                    SkillCandidate::command("opsx/apply", "opsx:apply"),
                ],
                "apply",
            )?,
            verify: resolve_command(
                repo,
                cli.verify_command.as_deref(),
                &[
                    SkillCandidate::skill("openspec-verify-change"),
                    SkillCandidate::command("opsx/verify.md", "opsx:verify"),
                    SkillCandidate::command("opsx/verify", "opsx:verify"),
                ],
                "verify",
            )?,
            archive: resolve_command(
                repo,
                cli.archive_command.as_deref(),
                &[
                    SkillCandidate::skill("openspec-archive-change"),
                    SkillCandidate::command("opsx/archive.md", "opsx:archive"),
                    SkillCandidate::command("opsx/archive", "opsx:archive"),
                ],
                "archive",
            )?,
        })
    }
}

fn resolve_bundled_command(explicit: Option<&str>, bundled_name: &str) -> String {
    explicit
        .map(normalize_command)
        .unwrap_or_else(|| format!("/{bundled_name}"))
}

#[derive(Debug, Clone)]
struct SkillCandidate {
    relative_path: PathBuf,
    invocation: String,
}

impl SkillCandidate {
    fn skill(name: &str) -> Self {
        Self {
            relative_path: PathBuf::from(format!(".claude/skills/{name}/SKILL.md")),
            invocation: format!("/{name}"),
        }
    }

    fn command(path: &str, invocation: &str) -> Self {
        Self {
            relative_path: PathBuf::from(format!(".claude/commands/{path}")),
            invocation: format!("/{invocation}"),
        }
    }
}

fn resolve_command(
    repo: &Path,
    explicit: Option<&str>,
    candidates: &[SkillCandidate],
    stage: &str,
) -> Result<String> {
    if let Some(command) = explicit {
        return Ok(normalize_command(command));
    }

    if let Some(candidate) = candidates
        .iter()
        .find(|candidate| repo.join(&candidate.relative_path).exists())
    {
        return Ok(candidate.invocation.clone());
    }

    bail!(
        "could not discover the Claude {stage} skill in `.claude/skills` or `.claude/commands`; pass `--{stage}-command /your-command`"
    )
}

fn normalize_command(command: &str) -> String {
    if command.starts_with('/') {
        command.to_owned()
    } else {
        format!("/{command}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageSignal {
    Ready,
    Done,
    TooLarge,
    Verified,
    Retry,
    Blocked,
    Replanned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageProtocol {
    Ready,
    Worker,
    Propose,
    Verify,
    Frontier,
}

impl StageProtocol {
    fn terminal_values(self) -> &'static str {
        match self {
            Self::Ready => "READY or BLOCKED",
            Self::Worker => "READY, TOO_LARGE, or BLOCKED",
            Self::Propose => "READY, DONE, TOO_LARGE, or BLOCKED",
            Self::Verify => "VERIFIED, RETRY, or BLOCKED",
            Self::Frontier => "REPLANNED or BLOCKED",
        }
    }

    pub(crate) fn json_schema(self) -> &'static str {
        match self {
            Self::Ready => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","BLOCKED"]},"summary":{"type":"string","description":"Concise stage result. For BLOCKED, include the exact blocker and evidence needed for a human decision."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Worker => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","TOO_LARGE","BLOCKED"]},"summary":{"type":"string","description":"Concise worker-stage result. TOO_LARGE means the assigned slice cannot reliably fit one bounded worker-model change; explain why and propose an ordered decomposition. For BLOCKED, include the exact external decision or unavailable input."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Propose => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["READY","DONE","TOO_LARGE","BLOCKED"]},"summary":{"type":"string","description":"Concise proposal result. DONE means the requested objective is already satisfied. TOO_LARGE means the assigned slice requires decomposition before a local worker can reliably implement it. Neither outcome may create or modify OpenSpec artifacts. Include decomposition advice for TOO_LARGE and the exact external blocker for BLOCKED."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Verify => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["VERIFIED","RETRY","BLOCKED"]},"summary":{"type":"string","description":"Concise stage result. For RETRY, include every actionable verification finding needed by the repair stage. For BLOCKED, include the exact blocker."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
            Self::Frontier => {
                r#"{"type":"object","properties":{"opsx_status":{"type":"string","enum":["REPLANNED","BLOCKED"]},"summary":{"type":"string","description":"Concise frontier-planning result. REPLANNED means the oversized agenda slice was replaced by a smaller first slice plus one or more ordered hierarchical descendants and committed. BLOCKED means safe subdivision requires a genuine external decision."}},"required":["opsx_status","summary"],"additionalProperties":false}"#
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClaudeResult {
    pub text: String,
    pub session_id: Option<String>,
    pub signal: StageSignal,
}

#[derive(Debug, Clone)]
pub enum SessionMode {
    New { id: Uuid, name: Option<String> },
    Resume { id: Uuid },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeOutputFormat {
    Text,
    Json,
    StreamJson,
}

enum ClaudeAttempt {
    Complete(ClaudeResult),
    OutputLimit,
}

pub fn build_claude_command(
    repo: &Path,
    launcher: &ClaudeLauncher,
    permission_mode: &str,
    session: &SessionMode,
    prompt: &str,
    output_format: ClaudeOutputFormat,
    json_schema: Option<&str>,
) -> CommandSpec {
    let mut args = launcher.prefix_args.clone();
    if let Some(model) = &launcher.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    args.push("--print".to_owned());
    match output_format {
        ClaudeOutputFormat::Text => {}
        ClaudeOutputFormat::Json => {
            args.extend(["--output-format".to_owned(), "json".to_owned()]);
        }
        ClaudeOutputFormat::StreamJson => {
            args.extend(["--input-format".to_owned(), "stream-json".to_owned()]);
            args.extend(["--output-format".to_owned(), "stream-json".to_owned()]);
            args.push("--verbose".to_owned());
            args.push("--forward-subagent-text".to_owned());
        }
    }
    if let Some(schema) = json_schema {
        args.extend(["--json-schema".to_owned(), schema.to_owned()]);
    }
    args.extend(["--permission-mode".to_owned(), permission_mode.to_owned()]);
    match session {
        SessionMode::New { id, name } => {
            args.extend(["--session-id".to_owned(), id.to_string()]);
            if let Some(name) = name {
                args.extend(["--name".to_owned(), name.clone()]);
            }
        }
        SessionMode::Resume { id } => {
            args.extend(["--resume".to_owned(), id.to_string()]);
        }
    }
    if output_format != ClaudeOutputFormat::StreamJson {
        args.push(prompt.to_owned());
    }
    let spec = CommandSpec::new(&launcher.program, repo).args(args);
    let spec = if output_format == ClaudeOutputFormat::StreamJson {
        spec.stream_input(stream_user_message(prompt))
    } else {
        spec
    };
    with_launcher_environment(spec, launcher)
}

pub fn build_interactive_claude_command(
    repo: &Path,
    launcher: &ClaudeLauncher,
    permission_mode: &str,
    extra_args: &[String],
    initial_prompt: Option<&str>,
) -> CommandSpec {
    let mut args = launcher.prefix_args.clone();
    if let Some(model) = &launcher.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    args.extend(["--permission-mode".to_owned(), permission_mode.to_owned()]);
    args.extend(extra_args.iter().cloned());
    if let Some(prompt) = initial_prompt {
        args.push(prompt.to_owned());
    }
    with_launcher_environment(
        CommandSpec::new(&launcher.program, repo).args(args),
        launcher,
    )
}

pub fn build_connection_test_command(
    repo: &Path,
    launcher: &ClaudeLauncher,
    permission_mode: &str,
) -> CommandSpec {
    let mut args = launcher.prefix_args.clone();
    if let Some(model) = &launcher.model {
        args.extend(["--model".to_owned(), model.clone()]);
    }
    args.extend([
        "--print".to_owned(),
        "--output-format".to_owned(),
        "json".to_owned(),
        "--no-session-persistence".to_owned(),
        "--permission-mode".to_owned(),
        permission_mode.to_owned(),
        format!(
            "Connectivity test only. Do not inspect files, invoke tools, or perform any other work. Reply with exactly {CONNECTION_TEST_MARKER}."
        ),
    ]);
    with_launcher_environment(
        CommandSpec::new(&launcher.program, repo).args(args),
        launcher,
    )
}

pub fn parse_connection_test_output(output: &ProcessOutput) -> Result<String> {
    if !output.success {
        bail!(
            "Claude connection test failed (exit {}): {}",
            output
                .code
                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
            diagnostic_text(output)
        );
    }
    let result = parse_output_values(&output.stdout)
        .into_iter()
        .rev()
        .find(|value| {
            value.get("type").is_none()
                || value.get("type").and_then(Value::as_str) == Some("result")
        })
        .with_context(|| "Claude connection test returned no machine-readable result")?;
    let text = result
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("Claude connection test reported an error: {text}");
    }
    if text.is_empty() {
        bail!("Claude connection test returned an empty model response");
    }
    Ok(text)
}

fn with_launcher_environment(spec: CommandSpec, launcher: &ClaudeLauncher) -> CommandSpec {
    let spec = launcher
        .unset_environment
        .iter()
        .fold(spec, |spec, name| spec.remove_env(name));
    let spec = launcher.environment.iter().fold(spec, |spec, variable| {
        if variable.redacted {
            spec.secret_env(&variable.name, &variable.value)
        } else {
            spec.env(&variable.name, &variable.value)
        }
    });
    let spec = match launcher.auto_compact_window {
        Some(window) => spec.env(AUTO_COMPACT_ENV, window.to_string()),
        None => spec,
    };
    let spec = match launcher.auto_compact_percent {
        Some(percentage) => spec.env(AUTO_COMPACT_PERCENT_ENV, percentage.to_string()),
        None => spec,
    };
    match launcher.max_output_tokens {
        Some(tokens) => spec.env(MAX_OUTPUT_TOKENS_ENV, tokens.to_string()),
        None => spec,
    }
}

pub struct ClaudeClient<'a, U: Ui> {
    repo: &'a Path,
    launcher: &'a ClaudeLauncher,
    permission_mode: &'a str,
    stream_transport: bool,
    stream_filter: Option<StreamFilter>,
    max_output_retries: u32,
    stage_timeout: Option<Duration>,
    ui: &'a U,
    runner: ProcessRunner<'a, U>,
}

impl<'a, U: Ui> ClaudeClient<'a, U> {
    pub fn new(
        repo: &'a Path,
        launcher: &'a ClaudeLauncher,
        permission_mode: &'a str,
        stream_transport: bool,
        stream_filter: Option<StreamFilter>,
        max_output_retries: u32,
        ui: &'a U,
    ) -> Self {
        Self {
            repo,
            launcher,
            permission_mode,
            stream_transport: stream_transport || stream_filter.is_some(),
            stream_filter,
            max_output_retries,
            stage_timeout: None,
            ui,
            runner: ProcessRunner::new(ui),
        }
    }

    pub fn with_stage_timeout(mut self, timeout: Duration) -> Self {
        self.stream_transport = true;
        self.stage_timeout = Some(timeout);
        self
    }

    pub fn invoke(
        &self,
        session: SessionMode,
        prompt: &str,
        activity: &str,
        protocol: StageProtocol,
    ) -> Result<ClaudeResult> {
        let session_id = session_id(&session);
        let mut current_session = session;
        let mut current_prompt = prompt.to_owned();
        let mut current_activity = activity.to_owned();
        let mut recoveries = 0;
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
            )? {
                ClaudeAttempt::Complete(result) => return Ok(result),
                ClaudeAttempt::OutputLimit if recoveries < self.max_output_retries => {
                    recoveries += 1;
                    self.ui.warn(&format!(
                        "Claude reached its output token limit; compacting and continuing the same phase ({recoveries}/{})",
                        self.max_output_retries
                    ));
                    self.compact_session(session_id, "an output-limit interruption")?;
                    current_session = SessionMode::Resume { id: session_id };
                    current_prompt = output_limit_continuation_prompt(protocol);
                    current_activity = format!(
                        "{activity} (output-limit continuation {recoveries}/{})",
                        self.max_output_retries
                    );
                }
                ClaudeAttempt::OutputLimit => {
                    return Err(WorkerEscalationRequested::new(format!(
                        "local worker exhausted {} output-limit recovery attempt(s)",
                        self.max_output_retries
                    ))
                    .into());
                }
            }
        }
    }

    fn invoke_once(
        &self,
        session: &SessionMode,
        prompt: &str,
        activity: &str,
        protocol: StageProtocol,
        timeout: Option<Duration>,
    ) -> Result<ClaudeAttempt> {
        self.ui
            .debug(&format!("Claude session: {}", session_description(session)));
        self.ui.debug_prompt(activity, prompt);
        let mut output_format = if self.stream_transport {
            ClaudeOutputFormat::StreamJson
        } else {
            ClaudeOutputFormat::Json
        };
        let mut use_schema = true;
        let mut first_attempt = true;
        let output = loop {
            let schema = use_schema.then(|| protocol.json_schema());
            let spec = build_claude_command(
                self.repo,
                self.launcher,
                self.permission_mode,
                session,
                prompt,
                output_format,
                schema,
            );
            let output = if first_attempt && output_format == ClaudeOutputFormat::StreamJson {
                self.run_stage_command(&spec, activity, timeout)?
            } else {
                self.runner.run(
                    &spec,
                    if first_attempt {
                        activity
                    } else {
                        "Retrying Claude protocol"
                    },
                )?
            };
            first_attempt = false;

            if output.success {
                if output_hit_token_limit(&output) {
                    return Ok(ClaudeAttempt::OutputLimit);
                }
                break output;
            }
            if output_hit_token_limit(&output) {
                return Ok(ClaudeAttempt::OutputLimit);
            }
            if use_schema && unsupported_schema_option(&output) {
                use_schema = false;
                self.ui.warn(
                    "Claude rejected --json-schema; using the terminal-marker compatibility protocol",
                );
                continue;
            }
            if unsupported_json_option(&output) {
                let previous = output_format;
                output_format = match output_format {
                    ClaudeOutputFormat::StreamJson => ClaudeOutputFormat::Json,
                    ClaudeOutputFormat::Json | ClaudeOutputFormat::Text => ClaudeOutputFormat::Text,
                };
                if output_format == previous {
                    bail!("Claude stage failed: {}", diagnostic_text(&output));
                }
                if output_format == ClaudeOutputFormat::Text {
                    use_schema = false;
                }
                self.ui.warn(&format!(
                    "Claude rejected {}; retrying with {}",
                    format_name(previous),
                    format_name(output_format)
                ));
                continue;
            }
            bail!("Claude stage failed: {}", diagnostic_text(&output));
        };

        let result = match output_format {
            ClaudeOutputFormat::StreamJson => parse_claude_stream_output(&output.stdout),
            ClaudeOutputFormat::Json | ClaudeOutputFormat::Text => {
                parse_claude_output(&output.stdout)
            }
        }?;
        Ok(ClaudeAttempt::Complete(result))
    }

    fn run_stage_command(
        &self,
        spec: &CommandSpec,
        activity: &str,
        timeout: Option<Duration>,
    ) -> Result<ProcessOutput> {
        self.runner
            .run_streaming_with_timeout(spec, activity, timeout, |line| {
                if let Some(filter) = self.stream_filter {
                    for item in filter_line(line, filter) {
                        self.ui.stream_item(&item);
                    }
                } else {
                    for item in context_report_items(line) {
                        self.ui.stream_item(&item);
                    }
                }
            })
    }

    pub fn rename_session(&self, session_id: Uuid, name: &str) -> Result<()> {
        let prompt = format!("/rename {name}");
        self.ui.debug(&format!(
            "Claude session: resume {session_id} for best-effort rename"
        ));
        self.ui.debug_prompt("Naming Claude session", &prompt);
        let spec = build_claude_command(
            self.repo,
            self.launcher,
            self.permission_mode,
            &SessionMode::Resume { id: session_id },
            &prompt,
            ClaudeOutputFormat::Json,
            None,
        );
        let output = self.runner.run(&spec, "Naming Claude session")?;
        if !output.success {
            bail!("session rename failed: {}", diagnostic_text(&output));
        }
        Ok(())
    }

    pub fn compact_session(&self, session_id: Uuid, completed_phase: &str) -> Result<()> {
        let activity = format!("Hard-compacting context after {completed_phase}");
        self.ui.debug(&format!(
            "Claude session: resume {session_id} for explicit /compact"
        ));
        self.ui.debug_prompt(&activity, "/compact");
        let output_format = if self.stream_transport {
            ClaudeOutputFormat::StreamJson
        } else {
            ClaudeOutputFormat::Text
        };
        let spec = build_compact_command(
            self.repo,
            self.launcher,
            self.permission_mode,
            session_id,
            output_format,
        );
        let result = if self.stream_transport {
            self.run_stage_command(&spec, &activity, None)
        } else {
            self.runner.run(&spec, &activity)
        };
        match result {
            Ok(output) if output.success => Ok(()),
            Ok(output) => {
                self.ui.warn(&format!(
                    "Could not compact Claude context after {completed_phase}; continuing with the existing session: {}",
                    diagnostic_text(&output)
                ));
                Ok(())
            }
            Err(error) if error.downcast_ref::<PauseRequested>().is_some() => Err(error),
            Err(error) => {
                self.ui.warn(&format!(
                    "Could not compact Claude context after {completed_phase}; continuing with the existing session: {error}"
                ));
                Ok(())
            }
        }
    }
}

fn build_compact_command(
    repo: &Path,
    launcher: &ClaudeLauncher,
    permission_mode: &str,
    session_id: Uuid,
    output_format: ClaudeOutputFormat,
) -> CommandSpec {
    build_claude_command(
        repo,
        launcher,
        permission_mode,
        &SessionMode::Resume { id: session_id },
        "/compact",
        output_format,
        None,
    )
    .disable_stream_messages()
}

fn format_name(format: ClaudeOutputFormat) -> &'static str {
    match format {
        ClaudeOutputFormat::Text => "text output",
        ClaudeOutputFormat::Json => "JSON output",
        ClaudeOutputFormat::StreamJson => "streaming JSON output",
    }
}

fn session_description(session: &SessionMode) -> String {
    match session {
        SessionMode::New { id, name } => match name {
            Some(name) => format!("new {id}, name `{name}`"),
            None => format!("new {id}"),
        },
        SessionMode::Resume { id } => format!("resume {id}"),
    }
}

fn session_id(session: &SessionMode) -> Uuid {
    match session {
        SessionMode::New { id, .. } | SessionMode::Resume { id } => *id,
    }
}

fn output_limit_continuation_prompt(protocol: StageProtocol) -> String {
    stage_prompt(
        "",
        "The preceding Claude turn reached its output token limit before this opsx-build phase produced a terminal result. Continue the same phase from the existing Claude session and durable repository state. Preserve correct completed work, do not restart the phase, and finish the outstanding work now.",
        protocol,
    )
}

fn output_hit_token_limit(output: &ProcessOutput) -> bool {
    let values = parse_output_values(&output.stdout);
    let last_limit = values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| value_hit_token_limit(value).then_some(index))
        .next_back();
    let last_terminal = values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| value_has_stage_terminal(value).then_some(index))
        .next_back();

    if let Some(limit) = last_limit {
        return last_terminal.is_none_or(|terminal| terminal < limit);
    }

    !output.success && text_mentions_output_limit(&format!("{}\n{}", output.stdout, output.stderr))
}

fn parse_output_values(stdout: &str) -> Vec<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(stdout) {
        return vec![value];
    }
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn value_hit_token_limit(value: &Value) -> bool {
    value.get("stop_reason").and_then(Value::as_str) == Some("max_tokens")
        || value
            .get("message")
            .and_then(|message| message.get("stop_reason"))
            .and_then(Value::as_str)
            == Some("max_tokens")
        || value
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(text_mentions_output_limit)
        || value
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| {
                errors
                    .iter()
                    .any(|error| error.as_str().is_some_and(text_mentions_output_limit))
            })
        || (value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && value
                .get("result")
                .and_then(Value::as_str)
                .is_some_and(text_mentions_output_limit))
}

fn value_has_stage_terminal(value: &Value) -> bool {
    let result_event =
        value.get("type").is_none() || value.get("type").and_then(Value::as_str) == Some("result");
    if !result_event {
        return false;
    }
    let structured = value.get("structured_output").and_then(|output| {
        if let Some(text) = output.as_str() {
            serde_json::from_str::<Value>(text).ok()
        } else {
            Some(output.clone())
        }
    });
    structured
        .as_ref()
        .and_then(|output| {
            output
                .get("opsx_status")
                .or_else(|| output.get("ospx_status"))
        })
        .and_then(Value::as_str)
        .and_then(parse_status_value)
        .is_some()
        || value
            .get("result")
            .and_then(Value::as_str)
            .and_then(parse_signal)
            .is_some()
}

fn text_mentions_output_limit(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("max_output_tokens")
        || text.contains("max output tokens")
        || text.contains("maximum output tokens")
        || text.contains("output token limit")
        || text.contains("output token maximum")
}

fn parse_claude_output(stdout: &str) -> Result<ClaudeResult> {
    let parsed = serde_json::from_str::<Value>(stdout).ok();
    let text = parsed
        .as_ref()
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        .unwrap_or(stdout)
        .to_owned();
    let session_id = parsed
        .as_ref()
        .and_then(|value| value.get("session_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    if parsed
        .as_ref()
        .and_then(|value| value.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("Claude reported an error: {text}");
    }

    let structured = parsed
        .as_ref()
        .map(parse_structured_output)
        .transpose()?
        .flatten();
    let signal = match structured {
        Some((signal, summary)) => {
            return Ok(ClaudeResult {
                text: summary,
                session_id,
                signal,
            });
        }
        None => parse_signal(&text).with_context(|| {
            "Claude response contained neither structured `opsx_status` output nor an `OPSX_STATUS` terminal marker; rerun with --verbose to inspect it"
        })?,
    };

    Ok(ClaudeResult {
        text,
        session_id,
        signal,
    })
}

fn parse_claude_stream_output(stdout: &str) -> Result<ClaudeResult> {
    let results = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("result"))
        .collect::<Vec<_>>();
    if results.is_empty() {
        bail!("Claude stream ended without a result event");
    }

    for result in results.iter().rev() {
        let text = result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let session_id = result
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some((signal, summary)) = parse_structured_output(result)? {
            return Ok(ClaudeResult {
                text: summary,
                session_id,
                signal,
            });
        }
        if let Some(signal) = parse_signal(&text) {
            return Ok(ClaudeResult {
                text,
                session_id,
                signal,
            });
        }
    }

    if let Some(result) = results.iter().rev().find(|result| {
        result
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }) {
        let text = result
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or_default();
        bail!("Claude reported an error: {text}");
    }
    bail!(
        "Claude stream results contained neither structured `opsx_status` output nor an `OPSX_STATUS` terminal marker; rerun with --stream-claude=raw to inspect them"
    )
}

pub fn parse_signal(text: &str) -> Option<StageSignal> {
    text.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix("OPSX_STATUS:")
            .or_else(|| line.trim().strip_prefix("OSPX_STATUS:"))
            .and_then(parse_status_value)
    })
}

fn parse_structured_output(result: &Value) -> Result<Option<(StageSignal, String)>> {
    let Some(output) = result.get("structured_output") else {
        return Ok(None);
    };
    if output.is_null() {
        return Ok(None);
    }
    let decoded;
    let output = if let Some(text) = output.as_str() {
        decoded = serde_json::from_str::<Value>(text)
            .context("Claude returned invalid JSON in `structured_output`")?;
        &decoded
    } else {
        output
    };
    let status = output
        .get("opsx_status")
        .or_else(|| output.get("ospx_status"))
        .and_then(Value::as_str)
        .context("Claude structured output omitted string field `opsx_status`")?;
    let signal = parse_status_value(status)
        .with_context(|| format!("Claude returned unknown structured status `{status}`"))?;
    let summary = output
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Some((signal, summary)))
}

fn parse_status_value(status: &str) -> Option<StageSignal> {
    match status.trim().to_ascii_uppercase().as_str() {
        "READY" => Some(StageSignal::Ready),
        "DONE" => Some(StageSignal::Done),
        "TOO_LARGE" => Some(StageSignal::TooLarge),
        "VERIFIED" => Some(StageSignal::Verified),
        "RETRY" => Some(StageSignal::Retry),
        "BLOCKED" => Some(StageSignal::Blocked),
        "REPLANNED" => Some(StageSignal::Replanned),
        _ => None,
    }
}

fn unsupported_json_option(output: &ProcessOutput) -> bool {
    let diagnostic = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    unsupported_option(&diagnostic)
        && (diagnostic.contains("output-format") || diagnostic.contains("input-format"))
}

fn unsupported_schema_option(output: &ProcessOutput) -> bool {
    let diagnostic = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    unsupported_option(&diagnostic) && diagnostic.contains("json-schema")
}

fn unsupported_option(diagnostic: &str) -> bool {
    diagnostic.contains("unknown option")
        || diagnostic.contains("unrecognized option")
        || diagnostic.contains("unexpected argument")
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
        bail!("--claude-command ends with an incomplete escape");
    }
    if quote != Quote::None {
        bail!("--claude-command contains an unterminated quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

pub fn stage_prompt(command: &str, subject: &str, protocol: StageProtocol) -> String {
    let task = if command.is_empty() {
        subject.to_owned()
    } else {
        format!("{command} {subject}")
    };
    let terminal_values = protocol.terminal_values();
    format!(
        "{task}\n\nThis invocation is controlled by opsx-build. Operate autonomously. Use BLOCKED only when progress genuinely requires a human decision or unavailable external input.\n\nTERMINAL PROTOCOL — MANDATORY\n\nReturn the supplied structured output with `opsx_status` set to exactly one of: {terminal_values}. Include a concise `summary`. If structured output is unavailable, you MUST NOT finish this invocation without emitting exactly one final line in the form `OPSX_STATUS: <value>` using the same allowed values. This obligation belongs to this outermost invocation even if a nested skill or OpenSpec command already reported success. Do not omit or paraphrase the fallback marker, wrap it in Markdown, or place text after it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use crate::{
        stream::{StreamControl, StreamItem},
        ui::Ui,
    };

    #[cfg(unix)]
    struct QuietUi;

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

    #[test]
    fn constructs_new_and_resumed_session_commands() {
        let id = Uuid::nil();
        let launcher = ClaudeLauncher::parse(
            "omlx launch claude",
            Some("qwen3.6-35b-a3b".to_owned()),
            Some(196_608),
            Some(75),
            Some(8_192),
        )
        .unwrap();
        let new = build_claude_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            &SessionMode::New {
                id,
                name: Some("slice-apply".to_owned()),
            },
            "/apply slice",
            ClaudeOutputFormat::Json,
            None,
        );
        assert_eq!(new.program, "omlx");
        assert_eq!(
            new.env,
            [
                (AUTO_COMPACT_ENV.to_owned(), "196608".to_owned()),
                (AUTO_COMPACT_PERCENT_ENV.to_owned(), "75".to_owned()),
                (MAX_OUTPUT_TOKENS_ENV.to_owned(), "8192".to_owned())
            ]
        );
        assert_eq!(
            new.args,
            vec![
                "launch",
                "claude",
                "--model",
                "qwen3.6-35b-a3b",
                "--print",
                "--output-format",
                "json",
                "--permission-mode",
                "auto",
                "--session-id",
                "00000000-0000-0000-0000-000000000000",
                "--name",
                "slice-apply",
                "/apply slice",
            ]
        );

        let resumed = build_claude_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            &SessionMode::Resume { id },
            "/propose slice",
            ClaudeOutputFormat::Json,
            None,
        );
        assert!(
            resumed
                .args
                .windows(2)
                .any(|args| { args == ["--resume", "00000000-0000-0000-0000-000000000000"] })
        );

        let compact = build_compact_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            id,
            ClaudeOutputFormat::Text,
        );
        assert_eq!(compact.args.last().map(String::as_str), Some("/compact"));
        assert!(
            compact
                .args
                .windows(2)
                .any(|args| { args == ["--resume", "00000000-0000-0000-0000-000000000000"] })
        );
        assert!(!compact.args.iter().any(|arg| arg == "--output-format"));
        assert_eq!(
            compact.env,
            [
                (AUTO_COMPACT_ENV.to_owned(), "196608".to_owned()),
                (AUTO_COMPACT_PERCENT_ENV.to_owned(), "75".to_owned()),
                (MAX_OUTPUT_TOKENS_ENV.to_owned(), "8192".to_owned())
            ]
        );

        let streamed_compact = build_compact_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            id,
            ClaudeOutputFormat::StreamJson,
        );
        assert!(
            streamed_compact
                .args
                .windows(2)
                .any(|args| { args == ["--output-format", "stream-json"] })
        );
        assert!(!streamed_compact.accepts_stream_messages);

        let streamed = build_claude_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            &SessionMode::Resume { id },
            "/verify slice",
            ClaudeOutputFormat::StreamJson,
            None,
        );
        assert!(
            streamed
                .args
                .windows(2)
                .any(|args| { args == ["--output-format", "stream-json"] })
        );
        assert!(
            streamed
                .args
                .windows(2)
                .any(|args| { args == ["--input-format", "stream-json"] })
        );
        assert!(!streamed.args.iter().any(|arg| arg == "/verify slice"));
        let input = streamed.initial_stdin.as_deref().unwrap().trim();
        let input: Value = serde_json::from_str(input).unwrap();
        assert_eq!(input["type"], "user");
        assert_eq!(input["message"]["content"][0]["text"], "/verify slice");
        assert!(streamed.accepts_stream_messages);
        assert!(streamed.args.iter().any(|arg| arg == "--verbose"));
        assert!(
            streamed
                .args
                .iter()
                .any(|arg| arg == "--forward-subagent-text")
        );
    }

    #[test]
    fn parses_quoted_launcher_without_a_shell() {
        let launcher = ClaudeLauncher::parse(
            r#"'/Applications/oMLX Preview.app/omlx' launch "claude code""#,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(launcher.program, "/Applications/oMLX Preview.app/omlx");
        assert_eq!(launcher.prefix_args, ["launch", "claude code"]);
        assert!(ClaudeLauncher::parse("omlx 'unterminated", None, None, None, None).is_err());
    }

    #[test]
    fn resolves_an_isolated_named_connection_into_commands() {
        let connection = ClaudeConnection {
            name: Some("hosted".to_owned()),
            environment_name: Some("provider".to_owned()),
            command: "claude".to_owned(),
            model: Some("provider/model".to_owned()),
            auto_compact_window: None,
            auto_compact_percent: None,
            max_output_tokens: Some(4_096),
            env: [
                (
                    "ANTHROPIC_BASE_URL".to_owned(),
                    ConnectionEnvironmentValue::Literal(
                        "https://provider.example/anthropic".to_owned(),
                    ),
                ),
                (
                    "ANTHROPIC_AUTH_TOKEN".to_owned(),
                    ConnectionEnvironmentValue::Literal("secret-token".to_owned()),
                ),
            ]
            .into_iter()
            .collect(),
            isolate: true,
            unset_env: vec!["HTTP_PROXY".to_owned()],
        };
        let launcher = ClaudeLauncher::from_connection(&connection).unwrap();
        let command = build_claude_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            &SessionMode::New {
                id: Uuid::nil(),
                name: None,
            },
            "plan",
            ClaudeOutputFormat::Json,
            None,
        );

        assert_eq!(launcher.connection_name.as_deref(), Some("hosted"));
        assert_eq!(launcher.environment_name.as_deref(), Some("provider"));
        assert!(
            launcher
                .unset_environment
                .iter()
                .any(|name| name == "ANTHROPIC_API_KEY")
        );
        assert!(
            launcher
                .unset_environment
                .iter()
                .any(|name| name == "HTTP_PROXY")
        );
        assert!(
            command
                .display()
                .contains("ANTHROPIC_AUTH_TOKEN=<redacted>")
        );
        assert!(!command.display().contains("secret-token"));
        assert!(
            command
                .args
                .windows(2)
                .any(|args| args == ["--model", "provider/model"])
        );
    }

    #[test]
    fn reports_a_missing_referenced_connection_environment_variable() {
        let connection = ClaudeConnection {
            name: Some("hosted".to_owned()),
            environment_name: Some("provider".to_owned()),
            command: "claude".to_owned(),
            model: None,
            auto_compact_window: None,
            auto_compact_percent: None,
            max_output_tokens: None,
            env: [(
                "ANTHROPIC_AUTH_TOKEN".to_owned(),
                ConnectionEnvironmentValue::FromEnvironment(crate::cli::EnvironmentReference {
                    from_env: "OPSX_BUILD_TEST_DEFINITELY_MISSING_PROFILE_SECRET_7F9C".to_owned(),
                }),
            )]
            .into_iter()
            .collect(),
            isolate: true,
            unset_env: Vec::new(),
        };
        let error = ClaudeLauncher::from_connection(&connection).unwrap_err();

        assert!(error.to_string().contains("connection profile `hosted`"));
        assert!(
            error
                .to_string()
                .contains("OPSX_BUILD_TEST_DEFINITELY_MISSING_PROFILE_SECRET_7F9C")
        );
    }

    #[test]
    fn bundled_commands_have_stable_defaults_and_allow_overrides() {
        assert_eq!(
            resolve_bundled_command(None, "explore-unattended"),
            "/explore-unattended"
        );
        assert_eq!(
            resolve_bundled_command(Some("custom-propose"), "propose-unattended"),
            "/custom-propose"
        );
    }

    #[test]
    fn constructs_interactive_command_without_print_mode() {
        let launcher = ClaudeLauncher::parse(
            "omlx launch claude",
            Some("local-model".to_owned()),
            Some(196_608),
            Some(75),
            Some(8_192),
        )
        .unwrap();
        let command = build_interactive_claude_command(
            Path::new("/repo"),
            &launcher,
            "manual",
            &["--effort".to_owned(), "xhigh".to_owned()],
            Some("inspect this project"),
        );

        assert_eq!(
            command.args,
            [
                "launch",
                "claude",
                "--model",
                "local-model",
                "--permission-mode",
                "manual",
                "--effort",
                "xhigh",
                "inspect this project",
            ]
        );
        assert!(!command.args.iter().any(|arg| arg == "--print"));
        assert_eq!(
            command.env,
            [
                (AUTO_COMPACT_ENV.to_owned(), "196608".to_owned()),
                (AUTO_COMPACT_PERCENT_ENV.to_owned(), "75".to_owned()),
                (MAX_OUTPUT_TOKENS_ENV.to_owned(), "8192".to_owned())
            ]
        );
    }

    #[test]
    fn constructs_a_minimal_non_persistent_connection_test() {
        let launcher = ClaudeLauncher::parse(
            "omlx launch claude",
            Some("test-model".to_owned()),
            None,
            None,
            None,
        )
        .unwrap();
        let command = build_connection_test_command(Path::new("/repo"), &launcher, "auto");

        assert_eq!(command.program, "omlx");
        assert!(
            command
                .args
                .windows(2)
                .any(|args| args == ["--model", "test-model"])
        );
        assert!(command.args.iter().any(|arg| arg == "--print"));
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg == "--no-session-persistence")
        );
        assert!(
            command
                .args
                .last()
                .is_some_and(|prompt| prompt.contains(CONNECTION_TEST_MARKER))
        );
        assert!(!command.args.iter().any(|arg| arg == "--session-id"));
        assert!(!command.accepts_stream_messages);
    }

    #[test]
    fn parses_a_machine_readable_connection_test_response() {
        let output = ProcessOutput {
            success: true,
            code: Some(0),
            stdout: format!(r#"{{"is_error":false,"result":"{CONNECTION_TEST_MARKER}"}}"#),
            stderr: String::new(),
        };
        assert_eq!(
            parse_connection_test_output(&output).unwrap(),
            CONNECTION_TEST_MARKER
        );

        let error = ProcessOutput {
            success: true,
            code: Some(0),
            stdout: r#"{"is_error":true,"result":"authentication failed"}"#.to_owned(),
            stderr: String::new(),
        };
        assert!(
            parse_connection_test_output(&error)
                .unwrap_err()
                .to_string()
                .contains("authentication failed")
        );
    }

    #[test]
    fn adds_stage_schema_to_unattended_command() {
        let launcher = ClaudeLauncher::parse("claude", None, None, None, None).unwrap();
        let schema = StageProtocol::Verify.json_schema();
        let command = build_claude_command(
            Path::new("/repo"),
            &launcher,
            "auto",
            &SessionMode::New {
                id: Uuid::nil(),
                name: None,
            },
            "verify",
            ClaudeOutputFormat::Json,
            Some(schema),
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|args| args == ["--json-schema", schema])
        );
    }

    #[test]
    fn worker_protocol_accepts_too_large_without_weakening_strict_stages() {
        let schema = StageProtocol::Worker.json_schema();
        assert!(schema.contains(r#"["READY","TOO_LARGE","BLOCKED"]"#));
        assert!(!StageProtocol::Ready.json_schema().contains("TOO_LARGE"));
        assert!(!StageProtocol::Verify.json_schema().contains("TOO_LARGE"));
        let prompt = stage_prompt("/explore-unattended", "inspect it", StageProtocol::Worker);
        assert!(prompt.contains("READY, TOO_LARGE, or BLOCKED"));
    }

    #[test]
    fn proposal_protocol_accepts_done_and_too_large_without_weakening_other_stages() {
        let schema = StageProtocol::Propose.json_schema();
        assert!(schema.contains(r#"["READY","DONE","TOO_LARGE","BLOCKED"]"#));
        assert!(!StageProtocol::Ready.json_schema().contains("DONE"));
        let prompt = stage_prompt("/propose-unattended", "finish it", StageProtocol::Propose);
        assert!(prompt.contains("READY, DONE, TOO_LARGE, or BLOCKED"));
    }

    #[test]
    fn frontier_protocol_accepts_only_replanned_or_blocked() {
        let schema = StageProtocol::Frontier.json_schema();
        assert!(schema.contains(r#"["REPLANNED","BLOCKED"]"#));
        assert!(!schema.contains("READY"));
        assert_eq!(
            parse_status_value("replanned"),
            Some(StageSignal::Replanned)
        );
        let prompt = stage_prompt("", "subdivide it", StageProtocol::Frontier);
        assert!(prompt.contains("REPLANNED or BLOCKED"));
    }

    #[test]
    fn parses_terminal_markers_from_last_matching_line() {
        assert_eq!(
            parse_signal("details\nOPSX_STATUS: RETRY"),
            Some(StageSignal::Retry)
        );
        assert_eq!(
            parse_signal("OPSX_STATUS: READY\nmore\nOPSX_STATUS: BLOCKED"),
            Some(StageSignal::Blocked)
        );
        assert_eq!(parse_signal("ordinary prose"), None);
        assert_eq!(
            parse_signal("No work remains\nOPSX_STATUS: DONE"),
            Some(StageSignal::Done)
        );
        assert_eq!(
            parse_signal("Needs decomposition\nOPSX_STATUS: TOO_LARGE"),
            Some(StageSignal::TooLarge)
        );
        assert_eq!(parse_signal("OPSX_STATUS: NOT_DONE"), None);
        assert_eq!(parse_signal("`OPSX_STATUS: DONE`"), None);
        assert_eq!(
            parse_signal("OSPX_STATUS: READY"),
            Some(StageSignal::Ready),
            "legacy status spelling remains readable"
        );
    }

    #[test]
    fn parses_terminal_result_from_a_jsonl_stream() {
        let stdout = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Working\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-1\",\"result\":\"Done\\nOPSX_STATUS: VERIFIED\"}\n"
        );
        let parsed = parse_claude_stream_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Verified);
        assert_eq!(parsed.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn parses_structured_result_without_terminal_marker() {
        let stdout = r#"{"type":"result","subtype":"success","session_id":"session-2","result":"{\"opsx_status\":\"READY\",\"summary\":\"Proposal complete\"}","structured_output":{"opsx_status":"READY","summary":"Proposal complete"}}"#;
        let parsed = parse_claude_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Ready);
        assert_eq!(parsed.text, "Proposal complete");
        assert_eq!(parsed.session_id.as_deref(), Some("session-2"));
    }

    #[test]
    fn parses_structured_done_result() {
        let stdout = r#"{"type":"result","subtype":"success","session_id":"session-done","result":"structured response","structured_output":{"opsx_status":"DONE","summary":"No remaining slice"}}"#;
        let parsed = parse_claude_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Done);
        assert_eq!(parsed.text, "No remaining slice");
    }

    #[test]
    fn parses_structured_too_large_result() {
        let stdout = r#"{"type":"result","subtype":"success","session_id":"session-large","result":"structured response","structured_output":{"opsx_status":"TOO_LARGE","summary":"Split parser and backend work"}}"#;
        let parsed = parse_claude_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::TooLarge);
        assert_eq!(parsed.text, "Split parser and backend work");
    }

    #[test]
    fn parses_legacy_structured_status_spelling() {
        let stdout = r#"{"type":"result","subtype":"success","session_id":"legacy","result":"structured response","structured_output":{"ospx_status":"READY","summary":"Compatible"}}"#;
        let parsed = parse_claude_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Ready);
        assert_eq!(parsed.text, "Compatible");
    }

    #[test]
    fn parses_structured_result_from_jsonl_stream() {
        let stdout = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-3\",\"result\":\"structured response\",\"structured_output\":{\"opsx_status\":\"VERIFIED\",\"summary\":\"Checks passed\"}}\n"
        );
        let parsed = parse_claude_stream_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Verified);
        assert_eq!(parsed.text, "Checks passed");
        assert_eq!(parsed.session_id.as_deref(), Some("session-3"));
    }

    #[test]
    fn ignores_a_later_compact_result_when_parsing_the_stage_result() {
        let stdout = concat!(
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-4\",\"result\":\"stage response\",\"structured_output\":{\"opsx_status\":\"READY\",\"summary\":\"Proposal complete\"}}\n",
            "{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compact_metadata\":{\"trigger\":\"manual\",\"pre_tokens\":24000}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-4\",\"result\":\"Compacted\",\"structured_output\":null}\n"
        );
        let parsed = parse_claude_stream_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Ready);
        assert_eq!(parsed.text, "Proposal complete");
        assert_eq!(parsed.session_id.as_deref(), Some("session-4"));
    }

    #[test]
    fn detects_success_result_that_stopped_at_the_output_limit() {
        let output = ProcessOutput {
            success: true,
            code: Some(0),
            stdout: r#"{"type":"result","subtype":"success","stop_reason":"max_tokens","session_id":"session-5","result":"unfinished"}"#.to_owned(),
            stderr: String::new(),
        };
        assert!(output_hit_token_limit(&output));
    }

    #[test]
    fn detects_max_output_token_api_failures() {
        let output = ProcessOutput {
            success: false,
            code: Some(1),
            stdout: r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["max_output_tokens"],"session_id":"session-6"}"#.to_owned(),
            stderr: String::new(),
        };
        assert!(output_hit_token_limit(&output));
    }

    #[test]
    fn detects_claude_codes_configured_output_maximum_error() {
        let output = ProcessOutput {
            success: false,
            code: Some(1),
            stdout: r#"{"is_error":true,"stop_reason":"stop_sequence","subtype":"success","result":"API Error: Claude's response exceeded the 64 output token maximum. To configure this behavior, set the CLAUDE_CODE_MAX_OUTPUT_TOKENS environment variable."}"#.to_owned(),
            stderr: String::new(),
        };
        assert!(output_hit_token_limit(&output));
    }

    #[test]
    fn does_not_retry_a_recovered_api_limit_after_a_terminal_result() {
        let output = ProcessOutput {
            success: true,
            code: Some(0),
            stdout: concat!(
                "{\"type\":\"system\",\"subtype\":\"api_retry\",\"error\":\"max_output_tokens\"}\n",
                "{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\",\"structured_output\":{\"opsx_status\":\"READY\",\"summary\":\"done\"}}\n"
            )
            .to_owned(),
            stderr: String::new(),
        };
        assert!(!output_hit_token_limit(&output));
    }

    #[test]
    fn retries_when_an_injected_turn_hits_the_limit_after_a_stage_result() {
        let output = ProcessOutput {
            success: true,
            code: Some(0),
            stdout: concat!(
                "{\"type\":\"result\",\"subtype\":\"success\",\"structured_output\":{\"opsx_status\":\"READY\",\"summary\":\"initial turn\"}}\n",
                "{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"max_tokens\",\"result\":\"unfinished follow-up\"}\n"
            )
            .to_owned(),
            stderr: String::new(),
        };
        assert!(output_hit_token_limit(&output));
    }

    #[test]
    fn continuation_prompt_preserves_the_stage_protocol() {
        let prompt = output_limit_continuation_prompt(StageProtocol::Verify);
        assert!(prompt.contains("Continue the same phase"));
        assert!(prompt.contains("VERIFIED, RETRY, or BLOCKED"));
    }

    #[cfg(unix)]
    #[test]
    fn output_limit_recovery_compacts_and_resumes_the_same_session() {
        let directory = std::env::temp_dir().join(format!("ospx-limit-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let state = directory.join("count");
        let script = directory.join("fake-claude.sh");
        let source = r#"
state='__STATE__'
if [ -f "$state" ]; then count=$(sed -n '1p' "$state"); else count=0; fi
case "$count" in
  0) printf '%s\n' '{"is_error":false,"stop_reason":"max_tokens","session_id":"00000000-0000-0000-0000-000000000000","result":"unfinished"}' ;;
  1) printf '%s\n' 'compacted' ;;
  *) printf '%s\n' '{"is_error":false,"stop_reason":"end_turn","session_id":"00000000-0000-0000-0000-000000000000","result":"done","structured_output":{"opsx_status":"READY","summary":"continued successfully"}}' ;;
esac
printf '%s\n' "$((count + 1))" > "$state"
"#
        .replace("__STATE__", &state.display().to_string());
        std::fs::write(&script, source).unwrap();

        let launcher = ClaudeLauncher {
            connection_name: None,
            environment_name: None,
            program: "sh".to_owned(),
            prefix_args: vec![script.display().to_string()],
            model: None,
            auto_compact_window: None,
            auto_compact_percent: None,
            max_output_tokens: None,
            environment: Vec::new(),
            unset_environment: Vec::new(),
        };
        let ui = QuietUi;
        let client = ClaudeClient::new(&directory, &launcher, "auto", false, None, 3, &ui);
        let result = client
            .invoke(
                SessionMode::New {
                    id: Uuid::nil(),
                    name: None,
                },
                "do the work",
                "Fake Claude stage",
                StageProtocol::Ready,
            )
            .unwrap();

        assert_eq!(result.signal, StageSignal::Ready);
        assert_eq!(result.text, "continued successfully");
        assert_eq!(std::fs::read_to_string(&state).unwrap().trim(), "3");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
