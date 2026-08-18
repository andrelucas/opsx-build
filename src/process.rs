use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::ui::Ui;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: cwd.into(),
        }
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    pub fn display(&self) -> String {
        let mut parts = self
            .env
            .iter()
            .map(|(name, value)| format!("{name}={}", shell_quote(value)))
            .collect::<Vec<_>>();
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
        mut on_stdout_line: F,
    ) -> Result<ProcessOutput>
    where
        F: FnMut(&str),
    {
        self.ui.command(&spec.display());
        let mut command = command_for(spec);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
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
        let mut read_error = None;
        let mut cancelled = false;
        let mut cancel_started = None;
        let mut forced_stop = false;
        loop {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(PipeChunk::Line(PipeKind::Stdout, line)) => {
                    on_stdout_line(line.trim_end_matches(['\r', '\n']));
                    captured_stdout.push_str(&line);
                }
                Ok(PipeChunk::Line(PipeKind::Stderr, line)) => captured_stderr.push_str(&line),
                Ok(PipeChunk::Error(error)) => {
                    read_error.get_or_insert(error);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if !cancelled && self.ui.poll_stream() {
                cancelled = true;
                cancel_started = Some(std::time::Instant::now());
                if let Err(error) = interrupt_stream_process(&mut child) {
                    read_error.get_or_insert(error);
                }
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

fn command_for(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
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

fn shell_quote(value: &str) -> String {
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
    use std::io::IsTerminal;

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

    #[cfg(unix)]
    #[test]
    fn passes_explicit_environment_to_subprocesses() {
        let spec = CommandSpec::new("sh", "/tmp")
            .args(["-c", "printf %s \"$OSPX_BUILD_TEST_WINDOW\""])
            .env("OSPX_BUILD_TEST_WINDOW", "196608");
        let output = command_for(&spec).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "196608");
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
