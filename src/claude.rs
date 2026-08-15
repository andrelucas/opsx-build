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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeLauncher {
    pub program: String,
    pub prefix_args: Vec<String>,
    pub model: Option<String>,
}

impl ClaudeLauncher {
    pub fn parse(command: &str, model: Option<String>) -> Result<Self> {
        let mut words = split_command(command)?;
        if words.is_empty() {
            bail!("--claude-command cannot be empty");
        }
        Ok(Self {
            program: words.remove(0),
            prefix_args: words,
            model,
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
    CommandSpec::new(&launcher.program, repo).args(args)
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
    CommandSpec::new(&launcher.program, repo).args(args)
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
        let spec = build_claude_command(
            self.repo,
            self.launcher,
            self.permission_mode,
            &session,
            prompt,
            output_format,
        );
        let mut output = self.run_stage_command(&spec, activity)?;

        if !output.success && unsupported_json_option(&output) {
            output_format = match output_format {
                ClaudeOutputFormat::StreamJson => ClaudeOutputFormat::Json,
                ClaudeOutputFormat::Json | ClaudeOutputFormat::Text => ClaudeOutputFormat::Text,
            };
            self.ui.warn(&format!(
                "Claude rejected {}; retrying with {}",
                format_name(if self.stream_filter.is_some() {
                    ClaudeOutputFormat::StreamJson
                } else {
                    ClaudeOutputFormat::Json
                }),
                format_name(output_format)
            ));
            let fallback = build_claude_command(
                self.repo,
                self.launcher,
                self.permission_mode,
                &session,
                prompt,
                output_format,
            );
            output = self.runner.run(&fallback, "Retrying Claude output mode")?;
        }

        if !output.success {
            bail!("Claude stage failed: {}", diagnostic_text(&output));
        }

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

    let signal = parse_signal(&text).with_context(|| {
        "Claude response did not contain an `OSPX_STATUS` terminal marker; rerun with --verbose to inspect it"
    })?;

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
    let signal = parse_signal(&text).context(
        "Claude stream result did not contain an `OSPX_STATUS` terminal marker; rerun with --stream-claude=raw to inspect it",
    )?;
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

fn unsupported_json_option(output: &ProcessOutput) -> bool {
    let diagnostic = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    diagnostic.contains("unknown option") && diagnostic.contains("output-format")
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

pub fn stage_prompt(command: &str, subject: &str, terminal_values: &str) -> String {
    format!(
        "{command} {subject}\n\nThis invocation is controlled by ospx-build. Operate autonomously and do not commit changes. End the final response with exactly one line in the form `OSPX_STATUS: <value>`. Allowed values for this stage: {terminal_values}. Use BLOCKED only for a genuine ambiguity or external dependency, not for an ordinary correctable engineering failure."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_new_and_resumed_session_commands() {
        let id = Uuid::nil();
        let launcher =
            ClaudeLauncher::parse("omlx launch claude", Some("qwen3.6-35b-a3b".to_owned()))
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
        );
        assert_eq!(new.program, "omlx");
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
        )
        .unwrap();
        assert_eq!(launcher.program, "/Applications/oMLX Preview.app/omlx");
        assert_eq!(launcher.prefix_args, ["launch", "claude code"]);
        assert!(ClaudeLauncher::parse("omlx 'unterminated", None).is_err());
    }

    #[test]
    fn constructs_interactive_command_without_print_mode() {
        let launcher =
            ClaudeLauncher::parse("omlx launch claude", Some("local-model".to_owned())).unwrap();
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
}
