use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    cli::Cli,
    process::{CommandSpec, ProcessOutput, ProcessRunner, diagnostic_text},
    stream::{StreamFilter, filter_line},
    ui::Ui,
};

const AUTO_COMPACT_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeLauncher {
    pub program: String,
    pub prefix_args: Vec<String>,
    pub model: Option<String>,
    pub auto_compact_window: Option<u64>,
}

impl ClaudeLauncher {
    pub fn parse(
        command: &str,
        model: Option<String>,
        auto_compact_window: Option<u64>,
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
    args.push(prompt.to_owned());
    with_launcher_environment(
        CommandSpec::new(&launcher.program, repo).args(args),
        launcher,
    )
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
    match launcher.auto_compact_window {
        Some(window) => spec.env(AUTO_COMPACT_ENV, window.to_string()),
        None => spec,
    }
}

pub struct ClaudeClient<'a, U: Ui> {
    repo: &'a Path,
    launcher: &'a ClaudeLauncher,
    permission_mode: &'a str,
    stream_filter: Option<StreamFilter>,
    ui: &'a U,
    runner: ProcessRunner<'a, U>,
}

impl<'a, U: Ui> ClaudeClient<'a, U> {
    pub fn new(
        repo: &'a Path,
        launcher: &'a ClaudeLauncher,
        permission_mode: &'a str,
        stream_filter: Option<StreamFilter>,
        ui: &'a U,
    ) -> Self {
        Self {
            repo,
            launcher,
            permission_mode,
            stream_filter,
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
        self.ui.debug(&format!(
            "Claude session: {}",
            session_description(&session)
        ));
        self.ui.debug_prompt(activity, prompt);
        let mut output_format = if self.stream_filter.is_some() {
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
                &session,
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
                break output;
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

        match output_format {
            ClaudeOutputFormat::StreamJson => parse_claude_stream_output(&output.stdout),
            ClaudeOutputFormat::Json | ClaudeOutputFormat::Text => {
                parse_claude_output(&output.stdout)
            }
        }
    }

    fn run_stage_command(&self, spec: &CommandSpec, activity: &str) -> Result<ProcessOutput> {
        let Some(filter) = self.stream_filter else {
            return self.runner.run(spec, activity);
        };
        self.runner.run_streaming(spec, activity, |line| {
            for item in filter_line(line, filter) {
                self.ui.stream_item(&item);
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
    let result = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .rev()
        .find(|value| value.get("type").and_then(Value::as_str) == Some("result"))
        .context("Claude stream ended without a result event")?;
    let text = result
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let session_id = result
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if result
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("Claude reported an error: {text}");
    }
    let signal = match parse_structured_output(&result)? {
        Some((signal, summary)) => {
            return Ok(ClaudeResult {
                text: summary,
                session_id,
                signal,
            });
        }
        None => parse_signal(&text).context(
            "Claude stream result contained neither structured `ospx_status` output nor an `OSPX_STATUS` terminal marker; rerun with --stream-claude=raw to inspect it",
        )?,
    };
    Ok(ClaudeResult {
        text,
        session_id,
        signal,
    })
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
    unsupported_option(&diagnostic) && diagnostic.contains("output-format")
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

    #[test]
    fn constructs_new_and_resumed_session_commands() {
        let id = Uuid::nil();
        let launcher = ClaudeLauncher::parse(
            "omlx launch claude",
            Some("qwen3.6-35b-a3b".to_owned()),
            Some(196_608),
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
            [(AUTO_COMPACT_ENV.to_owned(), "196608".to_owned())]
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
        )
        .unwrap();
        assert_eq!(launcher.program, "/Applications/oMLX Preview.app/omlx");
        assert_eq!(launcher.prefix_args, ["launch", "claude code"]);
        assert!(ClaudeLauncher::parse("omlx 'unterminated", None, None).is_err());
    }

    #[test]
    fn constructs_interactive_command_without_print_mode() {
        let launcher = ClaudeLauncher::parse(
            "omlx launch claude",
            Some("local-model".to_owned()),
            Some(196_608),
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
            [(AUTO_COMPACT_ENV.to_owned(), "196608".to_owned())]
        );
    }

    #[test]
    fn adds_stage_schema_to_unattended_command() {
        let launcher = ClaudeLauncher::parse("claude", None, None).unwrap();
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
}
