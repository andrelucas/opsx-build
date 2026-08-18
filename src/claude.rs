use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    cli::Cli,
    process::{CommandSpec, ProcessOutput, ProcessRunner, diagnostic_text, stream_user_message},
    stream::{StreamFilter, context_report_items, filter_line},
    ui::Ui,
};

const AUTO_COMPACT_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
const AUTO_COMPACT_PERCENT_ENV: &str = "CLAUDE_AUTOCOMPACT_PCT_OVERRIDE";
const MAX_OUTPUT_TOKENS_ENV: &str = "CLAUDE_CODE_MAX_OUTPUT_TOKENS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeLauncher {
    pub program: String,
    pub prefix_args: Vec<String>,
    pub model: Option<String>,
    pub auto_compact_window: Option<u64>,
    pub auto_compact_percent: Option<u8>,
    pub max_output_tokens: Option<u64>,
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
            program: words.remove(0),
            prefix_args: words,
            model,
            auto_compact_window,
            auto_compact_percent,
            max_output_tokens,
        })
    }
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
            explore: resolve_command(
                repo,
                cli.explore_command.as_deref(),
                &[SkillCandidate::skill("explore-unattended")],
                "explore",
            )?,
            propose: resolve_command(
                repo,
                cli.propose_command.as_deref(),
                &[SkillCandidate::skill("propose-unattended")],
                "propose",
            )?,
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
    Verified,
    Retry,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageProtocol {
    Ready,
    Verify,
}

impl StageProtocol {
    fn terminal_values(self) -> &'static str {
        match self {
            Self::Ready => "READY or BLOCKED",
            Self::Verify => "VERIFIED, RETRY, or BLOCKED",
        }
    }

    pub(crate) fn json_schema(self) -> &'static str {
        match self {
            Self::Ready => {
                r#"{"type":"object","properties":{"ospx_status":{"type":"string","enum":["READY","BLOCKED"]},"summary":{"type":"string","description":"Concise stage result. For BLOCKED, include the exact blocker and evidence needed for a human decision."}},"required":["ospx_status","summary"],"additionalProperties":false}"#
            }
            Self::Verify => {
                r#"{"type":"object","properties":{"ospx_status":{"type":"string","enum":["VERIFIED","RETRY","BLOCKED"]},"summary":{"type":"string","description":"Concise stage result. For RETRY, include every actionable verification finding needed by the repair stage. For BLOCKED, include the exact blocker."}},"required":["ospx_status","summary"],"additionalProperties":false}"#
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

fn with_launcher_environment(spec: CommandSpec, launcher: &ClaudeLauncher) -> CommandSpec {
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
            ui,
            runner: ProcessRunner::new(ui),
        }
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

        loop {
            match self.invoke_once(
                &current_session,
                &current_prompt,
                &current_activity,
                protocol,
            )? {
                ClaudeAttempt::Complete(result) => return Ok(result),
                ClaudeAttempt::OutputLimit if recoveries < self.max_output_retries => {
                    recoveries += 1;
                    self.ui.warn(&format!(
                        "Claude reached its output token limit; compacting and continuing the same phase ({recoveries}/{})",
                        self.max_output_retries
                    ));
                    self.compact_session(session_id, "an output-limit interruption");
                    current_session = SessionMode::Resume { id: session_id };
                    current_prompt = output_limit_continuation_prompt(protocol);
                    current_activity = format!(
                        "{activity} (output-limit continuation {recoveries}/{})",
                        self.max_output_retries
                    );
                }
                ClaudeAttempt::OutputLimit => {
                    bail!(
                        "Claude repeatedly reached its output token limit; the phase remains resumable at its current checkpoint after {} automatic continuation(s). Raise --max-output-retries or resume after reducing the requested output",
                        self.max_output_retries
                    );
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
                self.run_stage_command(&spec, activity)?
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

    fn run_stage_command(&self, spec: &CommandSpec, activity: &str) -> Result<ProcessOutput> {
        self.runner.run_streaming(spec, activity, |line| {
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

    pub fn compact_session(&self, session_id: Uuid, completed_phase: &str) {
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
            self.run_stage_command(&spec, &activity)
        } else {
            self.runner.run(&spec, &activity)
        };
        match result {
            Ok(output) if output.success => {}
            Ok(output) => self.ui.warn(&format!(
                "Could not compact Claude context after {completed_phase}; continuing with the existing session: {}",
                diagnostic_text(&output)
            )),
            Err(error) => self.ui.warn(&format!(
                "Could not compact Claude context after {completed_phase}; continuing with the existing session: {error}"
            )),
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
        "The preceding Claude turn reached its output token limit before this ospx-build phase produced a terminal result. Continue the same phase from the existing Claude session and durable repository state. Preserve correct completed work, do not restart the phase, and finish the outstanding work now.",
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
        .and_then(|output| output.get("ospx_status"))
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
            "Claude response contained neither structured `ospx_status` output nor an `OSPX_STATUS` terminal marker; rerun with --verbose to inspect it"
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
        "Claude stream results contained neither structured `ospx_status` output nor an `OSPX_STATUS` terminal marker; rerun with --stream-claude=raw to inspect them"
    )
}

pub fn parse_signal(text: &str) -> Option<StageSignal> {
    text.lines().rev().find_map(|line| {
        let line = line.trim().to_ascii_uppercase();
        if !line.contains("OSPX_STATUS") {
            return None;
        }
        if line.contains("BLOCKED") {
            Some(StageSignal::Blocked)
        } else if line.contains("VERIFIED") {
            Some(StageSignal::Verified)
        } else if line.contains("RETRY") {
            Some(StageSignal::Retry)
        } else if line.contains("READY") {
            Some(StageSignal::Ready)
        } else {
            None
        }
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
        .get("ospx_status")
        .and_then(Value::as_str)
        .context("Claude structured output omitted string field `ospx_status`")?;
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
        "VERIFIED" => Some(StageSignal::Verified),
        "RETRY" => Some(StageSignal::Retry),
        "BLOCKED" => Some(StageSignal::Blocked),
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
        "{task}\n\nThis invocation is controlled by ospx-build. Operate autonomously. Use BLOCKED only when progress genuinely requires a human decision or unavailable external input.\n\nTERMINAL PROTOCOL — MANDATORY\n\nReturn the supplied structured output with `ospx_status` set to exactly one of: {terminal_values}. Include a concise `summary`. If structured output is unavailable, you MUST NOT finish this invocation without emitting exactly one final line in the form `OSPX_STATUS: <value>` using the same allowed values. This obligation belongs to this outermost invocation even if a nested skill or OpenSpec command already reported success. Do not omit or paraphrase the fallback marker, wrap it in Markdown, or place text after it."
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
    fn parses_terminal_markers_from_last_matching_line() {
        assert_eq!(
            parse_signal("details\nOSPX_STATUS: RETRY"),
            Some(StageSignal::Retry)
        );
        assert_eq!(
            parse_signal("OSPX_STATUS: READY\nmore\nOSPX_STATUS: BLOCKED"),
            Some(StageSignal::Blocked)
        );
        assert_eq!(parse_signal("ordinary prose"), None);
    }

    #[test]
    fn parses_terminal_result_from_a_jsonl_stream() {
        let stdout = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Working\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-1\",\"result\":\"Done\\nOSPX_STATUS: VERIFIED\"}\n"
        );
        let parsed = parse_claude_stream_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Verified);
        assert_eq!(parsed.session_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn parses_structured_result_without_terminal_marker() {
        let stdout = r#"{"type":"result","subtype":"success","session_id":"session-2","result":"{\"ospx_status\":\"READY\",\"summary\":\"Proposal complete\"}","structured_output":{"ospx_status":"READY","summary":"Proposal complete"}}"#;
        let parsed = parse_claude_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Ready);
        assert_eq!(parsed.text, "Proposal complete");
        assert_eq!(parsed.session_id.as_deref(), Some("session-2"));
    }

    #[test]
    fn parses_structured_result_from_jsonl_stream() {
        let stdout = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-3\",\"result\":\"structured response\",\"structured_output\":{\"ospx_status\":\"VERIFIED\",\"summary\":\"Checks passed\"}}\n"
        );
        let parsed = parse_claude_stream_output(stdout).unwrap();
        assert_eq!(parsed.signal, StageSignal::Verified);
        assert_eq!(parsed.text, "Checks passed");
        assert_eq!(parsed.session_id.as_deref(), Some("session-3"));
    }

    #[test]
    fn ignores_a_later_compact_result_when_parsing_the_stage_result() {
        let stdout = concat!(
            "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-4\",\"result\":\"stage response\",\"structured_output\":{\"ospx_status\":\"READY\",\"summary\":\"Proposal complete\"}}\n",
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
                "{\"type\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\",\"structured_output\":{\"ospx_status\":\"READY\",\"summary\":\"done\"}}\n"
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
                "{\"type\":\"result\",\"subtype\":\"success\",\"structured_output\":{\"ospx_status\":\"READY\",\"summary\":\"initial turn\"}}\n",
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
  *) printf '%s\n' '{"is_error":false,"stop_reason":"end_turn","session_id":"00000000-0000-0000-0000-000000000000","result":"done","structured_output":{"ospx_status":"READY","summary":"continued successfully"}}' ;;
esac
printf '%s\n' "$((count + 1))" > "$state"
"#
        .replace("__STATE__", &state.display().to_string());
        std::fs::write(&script, source).unwrap();

        let launcher = ClaudeLauncher {
            program: "sh".to_owned(),
            prefix_args: vec![script.display().to_string()],
            model: None,
            auto_compact_window: None,
            auto_compact_percent: None,
            max_output_tokens: None,
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
