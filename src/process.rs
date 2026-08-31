use std::{
    collections::{BTreeSet, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{stream::StreamControl, ui::Ui};

const MODEL_CONFUSION_REJECTION_MARKER: &str = "Rejected by opsx-build incident";
const MODEL_CONFUSION_COMPACTION_THRESHOLD: usize = 3;

#[derive(Debug)]
pub struct PauseRequested {
    repo: PathBuf,
}

impl PauseRequested {
    fn new(repo: PathBuf) -> Self {
        Self { repo }
    }

    pub fn resume_command(&self) -> String {
        format!(
            "opsx-build --repo {} --resume",
            shell_quote(&self.repo.to_string_lossy())
        )
    }
}

impl std::fmt::Display for PauseRequested {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "workflow pause requested")
    }
}

impl std::error::Error for PauseRequested {}

#[derive(Debug)]
pub struct WorkerEscalationRequested {
    reason: String,
}

impl WorkerEscalationRequested {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for WorkerEscalationRequested {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "local worker escalation requested: {}",
            self.reason
        )
    }
}

impl std::error::Error for WorkerEscalationRequested {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    redacted_env: BTreeSet<String>,
    pub cwd: PathBuf,
    pub initial_stdin: Option<String>,
    pub accepts_stream_messages: bool,
    resume_repo: Option<PathBuf>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            redacted_env: BTreeSet::new(),
            cwd: cwd.into(),
            initial_stdin: None,
            accepts_stream_messages: false,
            resume_repo: None,
        }
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set_env(name.into(), value.into(), false);
        self
    }

    pub fn secret_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set_env(name.into(), value.into(), true);
        self
    }

    pub fn remove_env(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        if !self.env_remove.contains(&name) {
            self.env_remove.push(name);
        }
        self
    }

    fn set_env(&mut self, name: String, value: String, redacted: bool) {
        if let Some((_, existing)) = self.env.iter_mut().find(|(key, _)| key == &name) {
            *existing = value;
        } else {
            self.env.push((name.clone(), value));
        }
        if redacted {
            self.redacted_env.insert(name);
        } else {
            self.redacted_env.remove(&name);
        }
    }

    pub fn stream_input(mut self, initial: impl Into<String>) -> Self {
        self.initial_stdin = Some(initial.into());
        self.accepts_stream_messages = true;
        self
    }

    pub fn disable_stream_messages(mut self) -> Self {
        self.accepts_stream_messages = false;
        self
    }

    pub fn resume_from(mut self, repo: &Path) -> Self {
        self.resume_repo = Some(repo.to_path_buf());
        self
    }

    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if !self.env_remove.is_empty() {
            parts.push("env".to_owned());
            for name in &self.env_remove {
                parts.extend(["-u".to_owned(), shell_quote(name)]);
            }
        }
        parts.extend(self.env.iter().map(|(name, value)| {
            if self.redacted_env.contains(name) {
                format!("{name}=<redacted>")
            } else {
                format!("{name}={}", shell_quote(value))
            }
        }));
        parts.push(shell_quote(&self.program));
        parts.extend(self.args.iter().map(|arg| shell_quote(arg)));
        parts.join(" ")
    }
}

#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug)]
struct PendingTurn {
    input: String,
    purpose: TurnPurpose,
}

#[derive(Debug)]
enum TurnPurpose {
    Work,
    InterruptFollowUp {
        action: InterruptAction,
        resume_input: Option<String>,
    },
}

#[derive(Debug)]
enum InterruptAction {
    Compact,
    Context,
    Steer(String),
}

impl InterruptAction {
    fn command(&self) -> &str {
        match self {
            Self::Compact => "/compact",
            Self::Context => "/context",
            Self::Steer(message) => message,
        }
    }

    fn noun(&self) -> &'static str {
        match self {
            Self::Compact => "compaction",
            Self::Context => "context inspection",
            Self::Steer(_) => "steering instruction",
        }
    }

    fn resumes_interrupted_input(&self) -> bool {
        !matches!(self, Self::Steer(_))
    }
}

#[derive(Debug)]
struct PendingInterrupt {
    request_id: String,
    action: InterruptAction,
}

pub struct ProcessRunner<'a, U: Ui> {
    ui: &'a U,
}

impl<'a, U: Ui> ProcessRunner<'a, U> {
    pub fn new(ui: &'a U) -> Self {
        Self { ui }
    }

    pub fn run(&self, spec: &CommandSpec, activity: &str) -> Result<ProcessOutput> {
        self.ui.command(&spec.display());
        let spinner = self.ui.start_activity(activity);
        let output = command_for(spec)
            .output()
            .with_context(|| format!("failed to launch `{}`", spec.program));

        match output {
            Ok(output) => {
                let result = ProcessOutput {
                    success: output.status.success(),
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                };
                self.ui.finish_activity(spinner, result.success, activity);
                self.ui.output(&result.stdout, &result.stderr);
                Ok(result)
            }
            Err(error) => {
                self.ui.finish_activity(spinner, false, activity);
                Err(error)
            }
        }
    }

    pub fn checked(&self, spec: &CommandSpec, activity: &str) -> Result<ProcessOutput> {
        let output = self.run(spec, activity)?;
        if !output.success {
            bail!(
                "command failed (exit {}): {}\n{}",
                output
                    .code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                spec.display(),
                diagnostic_text(&output)
            );
        }
        Ok(output)
    }

    pub fn run_streaming<F>(
        &self,
        spec: &CommandSpec,
        activity: &str,
        on_stdout_line: F,
    ) -> Result<ProcessOutput>
    where
        F: FnMut(&str),
    {
        self.run_streaming_with_timeout(spec, activity, None, on_stdout_line)
    }

    pub fn run_streaming_with_timeout<F>(
        &self,
        spec: &CommandSpec,
        activity: &str,
        timeout: Option<Duration>,
        mut on_stdout_line: F,
    ) -> Result<ProcessOutput>
    where
        F: FnMut(&str),
    {
        self.ui.command(&spec.display());
        let mut command = command_for(spec);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if spec.initial_stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        configure_stream_process(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch `{}`", spec.program))?;
        let stdout = child
            .stdout
            .take()
            .context("could not capture subprocess stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("could not capture subprocess stderr")?;

        let (sender, receiver) = mpsc::channel();
        spawn_pipe_reader(stdout, PipeKind::Stdout, sender.clone());
        spawn_pipe_reader(stderr, PipeKind::Stderr, sender.clone());
        drop(sender);
        self.ui.start_stream(activity);

        let mut captured_stdout = String::new();
        let mut captured_stderr = String::new();
        let mut read_error: Option<std::io::Error> = None;
        let mut cancelled = false;
        let mut pause_requested = false;
        let mut escalation_reason = None;
        let mut cancel_started = None;
        let mut forced_stop = false;
        let started_at = std::time::Instant::now();
        let mut child_stdin = child.stdin.take();
        let mut sent_messages = usize::from(spec.initial_stdin.is_some());
        let mut completed_messages = 0usize;
        let mut pending_turns = VecDeque::new();
        let mut pending_interrupt: Option<PendingInterrupt> = None;
        let mut model_confusion_rejections = 0usize;
        let mut model_confusion_compaction_attempted = false;
        if let (Some(stdin), Some(initial)) = (child_stdin.as_mut(), spec.initial_stdin.as_deref())
        {
            match write_stream_input(stdin, initial) {
                Ok(()) => pending_turns.push_back(PendingTurn {
                    input: initial.to_owned(),
                    purpose: TurnPurpose::Work,
                }),
                Err(error) => {
                    read_error = Some(std::io::Error::new(
                        error.kind(),
                        format!("failed writing initial streamed input: {error}"),
                    ));
                    cancelled = true;
                    cancel_started = Some(std::time::Instant::now());
                    let _ = interrupt_stream_process(&mut child);
                }
            }
        }
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(PipeChunk::Line(PipeKind::Stdout, line)) => {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    on_stdout_line(trimmed);
                    if trimmed.contains(MODEL_CONFUSION_REJECTION_MARKER) {
                        model_confusion_rejections += 1;
                        if model_confusion_rejections >= MODEL_CONFUSION_COMPACTION_THRESHOLD
                            && !model_confusion_compaction_attempted
                            && pending_interrupt.is_none()
                            && spec.accepts_stream_messages
                        {
                            let request_id = format!("opsx_interrupt_{}", uuid::Uuid::new_v4());
                            if let Err(error) =
                                queue_stream_interrupt(&mut child_stdin, &request_id)
                            {
                                self.ui.warn(&format!(
                                    "Repeated model-confusion rejections detected, but automatic compaction could not interrupt Claude: {error}"
                                ));
                            } else {
                                model_confusion_compaction_attempted = true;
                                pending_interrupt = Some(PendingInterrupt {
                                    request_id,
                                    action: InterruptAction::Compact,
                                });
                                self.ui.stream_message_sent(
                                    "Repeated model-confusion rejections detected; automatic compaction interrupt written to Claude",
                                );
                            }
                        }
                    }
                    if let Some(result) = stream_result(trimmed) {
                        completed_messages += 1;
                        let completed_turn = pending_turns.pop_front();
                        if let Some(interrupt) = pending_interrupt.take() {
                            let resume_input = interrupt
                                .action
                                .resumes_interrupted_input()
                                .then(|| completed_turn.filter(|_| result_was_interrupted(&result)))
                                .flatten()
                                .map(|turn| turn.input);
                            let command = interrupt.action.command().to_owned();
                            let noun = interrupt.action.noun();
                            if let Err(error) = queue_interrupt_follow_up(
                                &mut child_stdin,
                                interrupt.action,
                                resume_input,
                                &mut sent_messages,
                                &mut pending_turns,
                            ) {
                                self.ui.warn(&format!(
                                    "Claude was interrupted, but {noun} could not be queued: {error}"
                                ));
                            } else {
                                let message = if command.starts_with('/') {
                                    format!("{command} written after interrupt; waiting for {noun}")
                                } else {
                                    "Steering instruction written after interrupt; Claude is continuing the stage".to_owned()
                                };
                                self.ui.stream_message_sent(&message);
                            }
                        } else if let Some(PendingTurn {
                            purpose:
                                TurnPurpose::InterruptFollowUp {
                                    action,
                                    resume_input,
                                },
                            ..
                        }) = completed_turn
                            && let Some(input) = resume_input
                        {
                            if let Err(error) = queue_raw_turn(
                                &mut child_stdin,
                                input,
                                TurnPurpose::Work,
                                &mut sent_messages,
                                &mut pending_turns,
                            ) {
                                self.ui.warn(&format!(
                                    "Claude completed {}, but the interrupted command could not be resumed: {error}",
                                    action.noun()
                                ));
                            } else {
                                self.ui.stream_message_sent(&format!(
                                    "Interrupted command reissued after {}",
                                    action.noun()
                                ));
                            }
                        }
                    } else if let Some(response) = pending_interrupt
                        .as_ref()
                        .and_then(|interrupt| control_response(trimmed, &interrupt.request_id))
                    {
                        match response {
                            Ok(()) => self.ui.stream_message_sent(
                                "Claude acknowledged the interrupt; waiting for active work to stop",
                            ),
                            Err(error) => {
                                let interrupt = pending_interrupt
                                    .take()
                                    .expect("matched response has a pending interrupt");
                                let noun = interrupt.action.noun();
                                self.ui.warn(&format!(
                                    "Claude rejected the interrupt ({error}); {noun} will remain a queued follow-up"
                                ));
                                if let Err(error) = queue_interrupt_follow_up(
                                    &mut child_stdin,
                                    interrupt.action,
                                    None,
                                    &mut sent_messages,
                                    &mut pending_turns,
                                ) {
                                    self.ui.warn(&format!(
                                        "Could not queue {noun}: {error}"
                                    ));
                                } else {
                                    self.ui.stream_message_sent(&format!(
                                        "Interrupt unsupported; {noun} queued for the next turn"
                                    ));
                                }
                            }
                        }
                    }
                    captured_stdout.push_str(&line);
                }
                Ok(PipeChunk::Line(PipeKind::Stderr, line)) => captured_stderr.push_str(&line),
                Ok(PipeChunk::Error(error)) => {
                    read_error.get_or_insert(error);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if !cancelled {
                if timeout.is_some_and(|limit| started_at.elapsed() >= limit) {
                    let limit = timeout.expect("timeout was checked above");
                    let reason = format!(
                        "local worker exceeded its {} minute stage timeout",
                        limit.as_secs() / 60
                    );
                    self.ui
                        .warn(&format!("{reason}; stopping it before frontier replanning"));
                    child_stdin.take();
                    cancelled = true;
                    escalation_reason = Some(reason);
                    cancel_started = Some(std::time::Instant::now());
                    if let Err(error) = interrupt_stream_process(&mut child) {
                        read_error.get_or_insert(error);
                    }
                    continue;
                }
                match self.ui.poll_stream() {
                    StreamControl::None => {}
                    StreamControl::Interrupt => {
                        child_stdin.take();
                        cancelled = true;
                        cancel_started = Some(std::time::Instant::now());
                        if let Err(error) = interrupt_stream_process(&mut child) {
                            read_error.get_or_insert(error);
                        }
                    }
                    StreamControl::Pause => {
                        child_stdin.take();
                        cancelled = true;
                        pause_requested = true;
                        cancel_started = Some(std::time::Instant::now());
                        if let Err(error) = interrupt_stream_process(&mut child) {
                            read_error.get_or_insert(error);
                        }
                    }
                    StreamControl::Escalate => {
                        child_stdin.take();
                        cancelled = true;
                        escalation_reason =
                            Some("frontier assistance requested from the terminal".to_owned());
                        cancel_started = Some(std::time::Instant::now());
                        if let Err(error) = interrupt_stream_process(&mut child) {
                            read_error.get_or_insert(error);
                        }
                    }
                    control @ (StreamControl::Compact
                    | StreamControl::Context
                    | StreamControl::Inject(_)) => {
                        if spec.accepts_stream_messages {
                            let action = match control {
                                StreamControl::Context => InterruptAction::Context,
                                StreamControl::Inject(message) => InterruptAction::Steer(message),
                                StreamControl::Compact => InterruptAction::Compact,
                                _ => unreachable!("matched interrupting stream control"),
                            };
                            if pending_interrupt.is_some() {
                                self.ui
                                    .warn("Another interrupting command is already in progress");
                                continue;
                            }
                            let request_id = format!("opsx_interrupt_{}", uuid::Uuid::new_v4());
                            if let Err(error) =
                                queue_stream_interrupt(&mut child_stdin, &request_id)
                            {
                                self.ui.warn(&format!(
                                    "Could not interrupt Claude for {}: {error}",
                                    action.noun()
                                ));
                            } else {
                                let noun = action.noun();
                                pending_interrupt = Some(PendingInterrupt { request_id, action });
                                self.ui.stream_message_sent(&format!(
                                    "Interrupt written to Claude stdin for {}; waiting for active work to stop",
                                    noun
                                ));
                            }
                        } else {
                            self.ui.warn(
                                "This Claude invocation does not accept interrupting commands",
                            );
                        }
                    }
                }
            }
            if child_stdin.is_some() && sent_messages > 0 && completed_messages >= sent_messages {
                child_stdin.take();
            }
            if !forced_stop
                && cancel_started.is_some_and(|started| started.elapsed() >= Duration::from_secs(2))
            {
                forced_stop = true;
                if let Err(error) = kill_stream_process(&mut child) {
                    read_error.get_or_insert(error);
                }
            }
        }

        let status = child.wait();
        let succeeded = status.as_ref().is_ok_and(|status| status.success())
            && read_error.is_none()
            && !cancelled;
        self.ui.finish_stream(succeeded, activity);
        let status = status.with_context(|| format!("failed waiting for `{}`", spec.program))?;
        if let Some(error) = read_error {
            bail!("failed reading streamed subprocess output: {error}");
        }
        let output = ProcessOutput {
            success: status.success(),
            code: status.code(),
            stdout: captured_stdout,
            stderr: captured_stderr,
        };
        self.ui.output(&output.stdout, &output.stderr);
        if pause_requested {
            return Err(PauseRequested::new(
                spec.resume_repo.clone().unwrap_or_else(|| spec.cwd.clone()),
            )
            .into());
        }
        if let Some(reason) = escalation_reason {
            return Err(WorkerEscalationRequested::new(reason).into());
        }
        if cancelled {
            bail!("streamed command interrupted by user");
        }
        Ok(output)
    }

    pub fn run_interactive(&self, spec: &CommandSpec) -> Result<()> {
        self.ui.command(&spec.display());
        let status = command_for(spec)
            .status()
            .with_context(|| format!("failed to launch `{}`", spec.program))?;
        if !status.success() {
            bail!(
                "interactive Claude exited with {}",
                status
                    .code()
                    .map_or_else(|| "a signal".to_owned(), |code| format!("status {code}"))
            );
        }
        Ok(())
    }
}

pub fn stream_user_message(message: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": message}]
            },
            "parent_tool_use_id": serde_json::Value::Null
        })
    )
}

fn stream_interrupt_request(request_id: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {"subtype": "interrupt"}
        })
    )
}

fn queue_stream_interrupt<W: Write>(
    stdin: &mut Option<W>,
    request_id: &str,
) -> std::io::Result<()> {
    let Some(stdin) = stdin.as_mut() else {
        return Err(closed_stdin_error());
    };
    write_stream_input(stdin, &stream_interrupt_request(request_id))
}

fn queue_interrupt_follow_up<W: Write>(
    stdin: &mut Option<W>,
    action: InterruptAction,
    resume_input: Option<String>,
    sent_messages: &mut usize,
    pending_turns: &mut VecDeque<PendingTurn>,
) -> std::io::Result<()> {
    let input = stream_user_message(action.command());
    let purpose = if action.resumes_interrupted_input() {
        TurnPurpose::InterruptFollowUp {
            action,
            resume_input,
        }
    } else {
        TurnPurpose::Work
    };
    queue_raw_turn(stdin, input, purpose, sent_messages, pending_turns)
}

fn queue_raw_turn<W: Write>(
    stdin: &mut Option<W>,
    input: String,
    purpose: TurnPurpose,
    sent_messages: &mut usize,
    pending_turns: &mut VecDeque<PendingTurn>,
) -> std::io::Result<()> {
    let Some(stdin) = stdin.as_mut() else {
        return Err(closed_stdin_error());
    };
    write_stream_input(stdin, &input)?;
    *sent_messages += 1;
    pending_turns.push_back(PendingTurn { input, purpose });
    Ok(())
}

fn closed_stdin_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "Claude stdin is already closed",
    )
}

fn write_stream_input(mut stdin: impl Write, input: &str) -> std::io::Result<()> {
    stdin.write_all(input.as_bytes())?;
    if !input.ends_with('\n') {
        stdin.write_all(b"\n")?;
    }
    stdin.flush()
}

fn stream_result(line: &str) -> Option<serde_json::Value> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    (value.get("type").and_then(serde_json::Value::as_str) == Some("result")).then_some(value)
}

fn result_was_interrupted(result: &serde_json::Value) -> bool {
    result.get("subtype").and_then(serde_json::Value::as_str) == Some("error_during_execution")
}

fn control_response(line: &str, request_id: &str) -> Option<Result<(), String>> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("control_response") {
        return None;
    }
    let response = value.get("response")?;
    if response
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        != Some(request_id)
    {
        return None;
    }
    match response.get("subtype").and_then(serde_json::Value::as_str) {
        Some("success") => Some(Ok(())),
        Some("error") => Some(Err(response
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown control error")
            .to_owned())),
        _ => None,
    }
}

fn command_for(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    for name in &spec.env_remove {
        command.env_remove(name);
    }
    command
        .envs(spec.env.iter().map(|(name, value)| (name, value)))
        .current_dir(&spec.cwd);
    command
}

#[cfg(unix)]
fn configure_stream_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_stream_process(_command: &mut Command) {}

#[cfg(unix)]
fn interrupt_stream_process(child: &mut Child) -> std::io::Result<()> {
    signal_process_group(child, libc::SIGINT)
}

#[cfg(not(unix))]
fn interrupt_stream_process(child: &mut Child) -> std::io::Result<()> {
    child.kill()
}

#[cfg(unix)]
fn kill_stream_process(child: &mut Child) -> std::io::Result<()> {
    signal_process_group(child, libc::SIGKILL)
}

#[cfg(not(unix))]
fn kill_stream_process(child: &mut Child) -> std::io::Result<()> {
    child.kill()
}

#[cfg(unix)]
fn signal_process_group(child: &Child, signal: libc::c_int) -> std::io::Result<()> {
    let process_group = -(child.id() as libc::pid_t);
    // SAFETY: `kill` is called with the process group created for this child and a valid signal.
    if unsafe { libc::kill(process_group, signal) } == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[derive(Clone, Copy)]
enum PipeKind {
    Stdout,
    Stderr,
}

enum PipeChunk {
    Line(PipeKind, String),
    Error(std::io::Error),
}

fn spawn_pipe_reader<R>(reader: R, kind: PipeKind, sender: mpsc::Sender<PipeChunk>)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if sender.send(PipeChunk::Line(kind, line)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(PipeChunk::Error(error));
                    break;
                }
            }
        }
    });
}

pub fn prerequisite_exists<U: Ui>(name: &str, repo: &Path, ui: &U) -> Result<()> {
    let spec = CommandSpec::new(name, repo).args(["--version"]);
    ui.command(&spec.display());
    let status = Command::new(name)
        .arg("--version")
        .current_dir(repo)
        .output()
        .with_context(|| format!("missing prerequisite `{name}`; install it and retry"))?;
    if !status.status.success() {
        bail!("prerequisite `{name}` exists but `{name} --version` failed");
    }
    Ok(())
}

pub fn diagnostic_text(output: &ProcessOutput) -> String {
    let text = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    if text.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        text.to_owned()
    }
}

pub(crate) fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/:=".contains(character))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use std::{io::IsTerminal, sync::Mutex};

    #[cfg(unix)]
    use crate::{stream::StreamItem, ui::TerminalUi};

    use super::*;

    #[test]
    fn displays_shell_safe_commands() {
        let spec = CommandSpec::new("claude", "/tmp/repo")
            .args(["--name", "my change", "simple"])
            .env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", "196608");
        assert_eq!(
            spec.display(),
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW=196608 claude --name 'my change' simple"
        );
    }

    #[test]
    fn redacts_secrets_and_displays_environment_removals() {
        let spec = CommandSpec::new("claude", "/tmp/repo")
            .remove_env("ANTHROPIC_API_KEY")
            .env("ANTHROPIC_BASE_URL", "https://provider.example")
            .secret_env("ANTHROPIC_AUTH_TOKEN", "do-not-print-me");
        let display = spec.display();

        assert!(display.starts_with("env -u ANTHROPIC_API_KEY "));
        assert!(display.contains("ANTHROPIC_BASE_URL=https://provider.example"));
        assert!(display.contains("ANTHROPIC_AUTH_TOKEN=<redacted>"));
        assert!(!display.contains("do-not-print-me"));
    }

    #[test]
    fn encodes_streamed_user_messages_as_json_lines() {
        let encoded = stream_user_message("/compact");
        assert!(encoded.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(encoded.trim()).unwrap();
        assert_eq!(value["type"], "user");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"][0]["text"], "/compact");
        assert!(value["parent_tool_use_id"].is_null());
    }

    #[test]
    fn queues_follow_up_messages_without_closing_the_writer() {
        let mut writer = Some(Vec::new());
        let mut sent_messages = 1;
        let mut pending_turns = VecDeque::new();
        queue_raw_turn(
            &mut writer,
            stream_user_message("please continue"),
            TurnPurpose::Work,
            &mut sent_messages,
            &mut pending_turns,
        )
        .unwrap();

        assert_eq!(sent_messages, 2);
        assert_eq!(pending_turns.len(), 1);
        let encoded = String::from_utf8(writer.unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(encoded.trim()).unwrap();
        assert_eq!(value["message"]["content"][0]["text"], "please continue");
    }

    #[test]
    fn encodes_interrupt_as_a_claude_control_request() {
        let encoded = stream_interrupt_request("request-123");
        let value: serde_json::Value = serde_json::from_str(encoded.trim()).unwrap();
        assert_eq!(value["type"], "control_request");
        assert_eq!(value["request_id"], "request-123");
        assert_eq!(value["request"]["subtype"], "interrupt");
    }

    #[test]
    fn recognizes_matching_interrupt_control_responses() {
        let success = r#"{"type":"control_response","response":{"subtype":"success","request_id":"request-123","response":{"still_queued":[]}}}"#;
        assert_eq!(control_response(success, "request-123"), Some(Ok(())));
        assert_eq!(control_response(success, "another-request"), None);

        let failure = r#"{"type":"control_response","response":{"subtype":"error","request_id":"request-123","error":"not supported"}}"#;
        assert_eq!(
            control_response(failure, "request-123"),
            Some(Err("not supported".to_owned()))
        );
    }

    #[cfg(unix)]
    struct InterruptingUi {
        control: Mutex<Option<StreamControl>>,
        messages: Mutex<Vec<String>>,
    }

    #[cfg(unix)]
    impl InterruptingUi {
        fn new(control: StreamControl) -> Self {
            Self {
                control: Mutex::new(Some(control)),
                messages: Mutex::new(Vec::new()),
            }
        }
    }

    #[cfg(unix)]
    impl Ui for InterruptingUi {
        fn banner(&self, _: &str) {}
        fn change_name(&self, _: Option<&str>) {}
        fn stage(&self, _: usize, _: usize, _: &str) {}
        fn info(&self, _: &str) {}
        fn warn(&self, message: &str) {
            self.messages
                .lock()
                .unwrap()
                .push(format!("warn: {message}"));
        }
        fn success(&self, _: &str) {}
        fn failure(&self, _: &str) {}
        fn command(&self, _: &str) {}
        fn debug(&self, _: &str) {}
        fn debug_prompt(&self, _: &str, _: &str) {}
        fn start_stream(&self, _: &str) {}
        fn poll_stream(&self) -> StreamControl {
            self.control.lock().unwrap().take().unwrap_or_default()
        }
        fn stream_message_sent(&self, message: &str) {
            self.messages.lock().unwrap().push(message.to_owned());
        }
        fn stream_item(&self, _: &StreamItem) {}
        fn finish_stream(&self, _: bool, _: &str) {}
        fn finish_dashboard(&self) {}
        fn output(&self, _: &str, _: &str) {}
        fn start_activity(&self, _: &str) -> Option<indicatif::ProgressBar> {
            None
        }
        fn finish_activity(&self, _: Option<indicatif::ProgressBar>, _: bool, _: &str) {}
    }

    #[cfg(unix)]
    fn run_interrupting_follow_up(
        control: StreamControl,
        expected_command: &str,
    ) -> (ProcessOutput, String) {
        let script = r#"
IFS= read -r initial
IFS= read -r control
expected=$1
request_id=$(printf '%s' "$control" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
test -n "$request_id" || exit 10
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"still_queued":[]}}}\n' "$request_id"
printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools","result":"Interrupted by user"}'
IFS= read -r follow_up
printf '%s' "$follow_up" | grep -Fq "\"text\":\"$expected\"" || exit 11
if [ "$expected" = /compact ]; then
  printf '%s\n' '{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"manual","pre_tokens":24000}}'
elif [ "$expected" = /context ]; then
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Context diagnostic"}]}}'
fi
if [ "${expected#/}" != "$expected" ]; then
  printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Follow-up complete"}'
  IFS= read -r resumed
  test "$resumed" = "$initial" || exit 12
fi
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Done","structured_output":{"opsx_status":"APPLIED","summary":"resumed"}}'
"#;
        let ui = InterruptingUi::new(control);
        let runner = ProcessRunner::new(&ui);
        let initial = stream_user_message("/opsx:apply slice-a");
        let spec = CommandSpec::new("sh", "/tmp")
            .args(["-c", script, "opsx-test", expected_command])
            .stream_input(initial);

        let output = runner
            .run_streaming(&spec, "Apply", |_| {})
            .expect("interrupt/follow-up/resume cycle should complete");

        assert!(output.success, "stderr: {}", output.stderr);
        let messages = ui.messages.lock().unwrap().join("\n");
        assert!(messages.contains("Interrupt written"));
        assert!(messages.contains("acknowledged the interrupt"));
        (output, messages)
    }

    #[cfg(unix)]
    #[test]
    fn compact_interrupts_then_compacts_and_reissues_the_active_turn() {
        let (output, messages) = run_interrupting_follow_up(StreamControl::Compact, "/compact");
        assert!(output.stdout.contains("compact_boundary"));
        assert_eq!(output.stdout.matches(r#""type":"result""#).count(), 3);
        assert!(messages.contains("/compact written after interrupt"));
        assert!(messages.contains("reissued after compaction"));
    }

    #[cfg(unix)]
    #[test]
    fn repeated_model_confusion_rejections_trigger_one_automatic_compaction() {
        let script = r#"
IFS= read -r initial
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Rejected by opsx-build incident one"}]}}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Rejected by opsx-build incident two"}]}}'
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Rejected by opsx-build incident three"}]}}'
IFS= read -r control
request_id=$(printf '%s' "$control" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
test -n "$request_id" || exit 20
printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"still_queued":[]}}}\n' "$request_id"
printf '%s\n' '{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools","result":"Interrupted"}'
IFS= read -r compact
printf '%s' "$compact" | grep -Fq '"text":"/compact"' || exit 21
printf '%s\n' '{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"automatic-model-confusion","pre_tokens":24000}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Compacted"}'
IFS= read -r resumed
test "$resumed" = "$initial" || exit 22
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Done","structured_output":{"opsx_status":"APPLIED","summary":"resumed"}}'
"#;
        let ui = InterruptingUi::new(StreamControl::None);
        let runner = ProcessRunner::new(&ui);
        let initial = stream_user_message("/opsx:apply slice-a");
        let spec = CommandSpec::new("sh", "/tmp")
            .args(["-c", script])
            .stream_input(initial);

        let output = runner
            .run_streaming(&spec, "Apply", |_| {})
            .expect("automatic model-confusion compaction should recover the turn");

        assert!(output.success, "stderr: {}", output.stderr);
        assert_eq!(output.stdout.matches("compact_boundary").count(), 1);
        let messages = ui.messages.lock().unwrap().join("\n");
        assert!(messages.contains("automatic compaction interrupt"));
        assert!(messages.contains("reissued after compaction"));
    }

    #[cfg(unix)]
    #[test]
    fn context_interrupts_then_reports_and_reissues_the_active_turn() {
        let (output, messages) = run_interrupting_follow_up(StreamControl::Context, "/context");
        assert!(output.stdout.contains("Context diagnostic"));
        assert_eq!(output.stdout.matches(r#""type":"result""#).count(), 3);
        assert!(messages.contains("/context written after interrupt"));
        assert!(messages.contains("reissued after context inspection"));
    }

    #[cfg(unix)]
    #[test]
    fn steering_message_interrupts_and_continues_the_active_turn() {
        let direction = "Use the existing parser helper instead";
        let (output, messages) =
            run_interrupting_follow_up(StreamControl::Inject(direction.to_owned()), direction);
        assert_eq!(output.stdout.matches(r#""type":"result""#).count(), 2);
        assert!(messages.contains("Steering instruction written after interrupt"));
        assert!(!messages.contains("Interrupted command reissued"));
    }

    #[cfg(unix)]
    #[test]
    fn pause_stops_the_process_group_and_uses_the_user_facing_resume_repo() {
        let ui = InterruptingUi::new(StreamControl::Pause);
        let runner = ProcessRunner::new(&ui);
        let spec = CommandSpec::new("sh", "/tmp")
            .args(["-c", "sleep 30"])
            .resume_from(Path::new("/tmp/product"));

        let error = runner
            .run_streaming(&spec, "Apply", |_| {})
            .expect_err("pause should stop the subprocess and unwind the workflow");
        let pause = error
            .downcast_ref::<PauseRequested>()
            .expect("pause should remain a typed outcome");
        assert_eq!(
            pause.resume_command(),
            "opsx-build --repo /tmp/product --resume"
        );
    }

    #[cfg(unix)]
    #[test]
    fn frontier_key_stops_the_worker_with_a_typed_escalation() {
        let ui = InterruptingUi::new(StreamControl::Escalate);
        let runner = ProcessRunner::new(&ui);
        let spec = CommandSpec::new("sh", "/tmp").args(["-c", "sleep 30"]);

        let error = runner
            .run_streaming(&spec, "Apply", |_| {})
            .expect_err("frontier escalation should stop the subprocess");
        let escalation = error
            .downcast_ref::<WorkerEscalationRequested>()
            .expect("frontier escalation should remain a typed outcome");
        assert!(escalation.reason().contains("terminal"));
    }

    #[cfg(unix)]
    #[test]
    fn stage_timeout_stops_the_worker_with_a_typed_escalation() {
        let ui = InterruptingUi {
            control: Mutex::new(None),
            messages: Mutex::new(Vec::new()),
        };
        let runner = ProcessRunner::new(&ui);
        let spec = CommandSpec::new("sh", "/tmp").args(["-c", "sleep 30"]);

        let error = runner
            .run_streaming_with_timeout(&spec, "Apply", Some(Duration::from_millis(25)), |_| {})
            .expect_err("worker timeout should stop the subprocess");
        let escalation = error
            .downcast_ref::<WorkerEscalationRequested>()
            .expect("worker timeout should remain a typed outcome");
        assert!(escalation.reason().contains("stage timeout"));
    }

    #[cfg(unix)]
    #[test]
    fn passes_explicit_environment_to_subprocesses() {
        let spec = CommandSpec::new("sh", "/tmp")
            .args(["-c", "printf %s \"$OPSX_BUILD_TEST_WINDOW\""])
            .env("OPSX_BUILD_TEST_WINDOW", "196608");
        let output = command_for(&spec).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "196608");
    }

    #[cfg(unix)]
    #[test]
    fn removes_inherited_environment_from_subprocesses() {
        let spec = CommandSpec::new("sh", "/tmp")
            .args([
                "-c",
                "if [ -z \"${HOME+x}\" ]; then printf removed; else printf inherited; fi",
            ])
            .remove_env("HOME");
        let output = command_for(&spec).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "removed");
    }

    #[cfg(unix)]
    #[test]
    fn streamed_process_dashboard_smoke_test_when_available() {
        if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
            return;
        }
        let ui = TerminalUi::new(false, false, true);
        ui.banner("/tmp/example");
        ui.stage(4, 7, "Apply");
        let runner = ProcessRunner::new(&ui);
        let spec = CommandSpec::new("sh", "/tmp").args([
            "-c",
            "printf 'first line\\n'; sleep 0.1; printf 'second line\\n'",
        ]);
        let output = runner
            .run_streaming(&spec, "Streaming test output", |line| {
                ui.stream_item(&StreamItem::Assistant(line.to_owned()));
            })
            .unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, "first line\nsecond line\n");

        ui.stage(5, 7, "Verify");
        let verify = CommandSpec::new("sh", "/tmp").args(["-c", "printf 'verified\\n'"]);
        let output = runner
            .run_streaming(&verify, "Streaming verification output", |line| {
                ui.stream_item(&StreamItem::Assistant(line.to_owned()));
            })
            .unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, "verified\n");
        ui.finish_dashboard();
    }
}
