use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{
    backend::{
        AgentBackend, MissingTerminalResult, SessionId, SessionMode, StageProtocol, StageResult,
        parse_terminal_signal,
    },
    claude::{CONNECTION_TEST_MARKER, ConnectionTestMode},
    cli::{AgentConnection, BackendKind, ConnectionEnvironmentValue},
    process::{CommandSpec, ProcessOutput, ProcessRunner, diagnostic_text},
    stream::{StreamFilter, StreamItem},
    ui::Ui,
};

const CONNECTION_TOOL_MARKER: &str = "OPSX_TOOL_ROUNDTRIP_OK";

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
            environment,
            unset_environment,
        })
    }
}

pub fn build_opencode_command(
    repo: &Path,
    launcher: &OpenCodeLauncher,
    session: &SessionMode,
    prompt: &str,
) -> CommandSpec {
    let invocation = resolve_invocation(repo, prompt);
    let mut args = launcher.prefix_args.clone();
    args.extend(["run".to_owned(), "--format".to_owned(), "json".to_owned()]);
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
    if let Some(command) = invocation.command {
        args.extend(["--command".to_owned(), command]);
    }
    if !invocation.message.is_empty() {
        args.push(invocation.message);
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
    stream_transport: bool,
    stream_filter: Option<StreamFilter>,
    stage_timeout: Option<Duration>,
    resume_repo: Option<PathBuf>,
    ui: &'a U,
    runner: ProcessRunner<'a, U>,
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
            stream_transport: stream_transport || stream_filter.is_some(),
            stream_filter,
            stage_timeout: None,
            resume_repo: None,
            ui,
            runner: ProcessRunner::new(ui),
        }
    }

    pub fn with_stage_timeout(mut self, timeout: Duration) -> Self {
        self.stream_transport = true;
        self.stage_timeout = Some(timeout);
        self
    }

    pub fn with_resume_repo(mut self, repo: &Path) -> Self {
        self.resume_repo = Some(repo.to_path_buf());
        self
    }

    fn invoke_once(
        &self,
        session: SessionMode,
        prompt: &str,
        activity: &str,
    ) -> Result<StageResult> {
        self.ui.debug(&format!(
            "OpenCode session: {}",
            session_description(&session)
        ));
        self.ui.debug_prompt(activity, prompt);
        let spec = build_opencode_command(self.repo, self.launcher, &session, prompt);
        let spec = if let Some(repo) = self.resume_repo.as_deref() {
            spec.resume_from(repo)
        } else {
            spec
        };
        let output = if self.stream_transport {
            self.runner
                .run_streaming_with_timeout(&spec, activity, self.stage_timeout, |line| {
                    if let Some(filter) = self.stream_filter {
                        for item in filter_opencode_line(line, filter) {
                            self.ui.stream_item(&item);
                        }
                    }
                })?
        } else {
            self.runner.run(&spec, activity)?
        };
        if !output.success {
            return Err(opencode_stage_failure(&output));
        }
        parse_opencode_output(&output.stdout)
    }
}

impl<U: Ui> AgentBackend for OpenCodeBackend<'_, U> {
    fn name(&self) -> &'static str {
        "OpenCode"
    }

    fn invoke(
        &self,
        session: SessionMode,
        prompt: &str,
        activity: &str,
        _protocol: StageProtocol,
    ) -> Result<StageResult> {
        self.invoke_once(session, prompt, activity)
    }

    fn rename_session(&self, _session_id: &SessionId, _name: &str) -> Result<()> {
        // `opencode run` can title a new session but cannot rename an existing one.
        // The orchestrator retains the change/session association in its checkpoint.
        Ok(())
    }

    fn compact_session(&self, session_id: &SessionId, completed_phase: &str) -> Result<()> {
        self.ui.warn(&format!(
            "OpenCode's synchronous `run` transport cannot hard-compact session {session_id} after {completed_phase}; continuing with its automatic context management"
        ));
        Ok(())
    }
}

#[derive(Debug)]
struct Invocation {
    command: Option<String>,
    message: String,
}

fn resolve_invocation(repo: &Path, prompt: &str) -> Invocation {
    let trimmed = prompt.trim_start();
    let Some(after_slash) = trimmed.strip_prefix('/') else {
        return Invocation {
            command: None,
            message: prompt.to_owned(),
        };
    };
    let command_end = after_slash
        .find(char::is_whitespace)
        .unwrap_or(after_slash.len());
    let name = &after_slash[..command_end];
    let arguments = after_slash[command_end..].trim_start();
    if skill_exists(repo, name) {
        Invocation {
            command: None,
            message: format!(
                "Use the skill tool to load and follow the `{name}` skill for this assignment.\n\n{arguments}"
            ),
        }
    } else {
        Invocation {
            command: Some(name.to_owned()),
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
            "OpenCode response did not contain an `OPSX_STATUS` terminal marker; rerun with --stream-claude=raw to inspect it",
            Some(&text),
        )
    })?;
    Ok(StageResult {
        text,
        session_id,
        signal,
    })
}

fn opencode_stage_failure(output: &ProcessOutput) -> anyhow::Error {
    let events = output
        .stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    if let Some(error) = events.iter().rev().find_map(opencode_event_error) {
        anyhow::anyhow!("OpenCode stage failed: {error}")
    } else {
        anyhow::anyhow!("OpenCode stage failed: {}", diagnostic_text(output))
    }
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
            .map(|text| vec![StreamItem::Assistant(text.to_owned())])
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

fn session_description(session: &SessionMode) -> String {
    match session {
        SessionMode::New { id, name } => match name {
            Some(name) => format!("new (provisional {id}), title `{name}`"),
            None => format!("new (provisional {id})"),
        },
        SessionMode::Resume { id } => format!("resume {id}"),
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
    use uuid::Uuid;

    fn launcher() -> OpenCodeLauncher {
        OpenCodeLauncher {
            connection_name: Some("open".to_owned()),
            environment_name: None,
            program: "opencode".to_owned(),
            prefix_args: Vec::new(),
            model: Some("openrouter/qwen/qwen3.6-35b-a3b".to_owned()),
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
    fn uses_opencode_command_mode_when_the_leading_name_is_not_a_skill() {
        let command = build_opencode_command(
            Path::new("/repo"),
            &launcher(),
            &SessionMode::New {
                id: SessionId::new("provisional"),
                name: None,
            },
            "/opsx:apply change-name",
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["--command", "opsx:apply"])
        );
        assert_eq!(command.args.last().map(String::as_str), Some("change-name"));
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
    fn parses_open_code_profile_and_rejects_claude_model_spelling() {
        let connection = AgentConnection {
            name: Some("open".to_owned()),
            backend: BackendKind::OpenCode,
            environment_name: None,
            command: "wrapper opencode".to_owned(),
            model: Some("provider/model".to_owned()),
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
