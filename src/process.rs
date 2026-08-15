use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};

use anyhow::{Context, Result, bail};

use crate::ui::Ui;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
        }
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
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
        let output = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
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
        self.ui.info(activity);
        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

        let mut captured_stdout = String::new();
        let mut captured_stderr = String::new();
        let mut read_error = None;
        for chunk in receiver {
            match chunk {
                PipeChunk::Line(PipeKind::Stdout, line) => {
                    on_stdout_line(line.trim_end_matches(['\r', '\n']));
                    captured_stdout.push_str(&line);
                }
                PipeChunk::Line(PipeKind::Stderr, line) => captured_stderr.push_str(&line),
                PipeChunk::Error(error) => {
                    read_error.get_or_insert(error);
                }
            };
        }

        let status = child
            .wait()
            .with_context(|| format!("failed waiting for `{}`", spec.program))?;
        if let Some(error) = read_error {
            bail!("failed reading streamed subprocess output: {error}");
        }
        let output = ProcessOutput {
            success: status.success(),
            code: status.code(),
            stdout: captured_stdout,
            stderr: captured_stderr,
        };
        self.ui.finish_activity(None, output.success, activity);
        self.ui.output(&output.stdout, &output.stderr);
        Ok(output)
    }

    pub fn run_interactive(&self, spec: &CommandSpec) -> Result<()> {
        self.ui.command(&spec.display());
        let status = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
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
    use super::*;

    #[test]
    fn displays_shell_safe_commands() {
        let spec = CommandSpec::new("claude", "/tmp/repo").args(["--name", "my change", "simple"]);
        assert_eq!(spec.display(), "claude --name 'my change' simple");
    }
}
