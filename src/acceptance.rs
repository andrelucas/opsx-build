//! Maintainer-selected checks, executed by the runner rather than the agent.
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    git::{current_head, metadata_dir},
    process::{PauseRequested, configure_stream_process, kill_stream_process},
    state::Stage,
    stream::{StreamControl, StreamItem},
    ui::Ui,
    verification::{Files, snapshot},
};

pub(crate) const DEFAULT_TIMEOUT_SECONDS: u64 = 600;
pub(crate) const CONFIGURE_GUIDANCE: &str = "\n\nFor supplied-contract builds, ask which maintainer-supplied commands define executable final acceptance. Record each agreed --acceptance-command COMMAND and --acceptance-timeout-seconds SECONDS (default 600 per command). Commands run independently through /bin/sh -c from the campaign root, in the runner's environment and outer sandbox, after independent review. Finish the campaign only when they pass. Exit zero is required; preserve the check's real exit status (prefer a supplied script over pipelines). They do not run during bootstrap or intermediate slices. List every maintainer-owned test, driver and fixture they rely on with --acceptance-file PATH so it is protected unchanged; an existing contract_test.go is an acceptance input, not something the campaign may rewrite. Do not invent test scenarios or claim an existing suite is complete. If only some required scenarios have supplied checks, explain the coverage gap. A provenance manifest identifies inputs and original hashes; it is not itself an executable gate. The runner pins current file contents, not hashes imported from a manifest. If original hash verification has not been independently established, explain that it remains necessary; do not claim to have calculated hashes using read-only text tools. Never run builds or checks during configure. Commands, timeout and protected inputs are pinned in a run's checkpoint; changing campaign settings cannot waive them on resume. A maintainer may use accept-changes to adopt corrected registered inputs, commands and timeouts for an existing run. Builders must not invoke it to change their own requirements. Explain when no independent gate is selected. Keep secrets out of command strings and logs.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AcceptanceGate {
    pub commands: Vec<String>,
    pub timeout_seconds: u64,
    pub next_stage: Stage,
    pub failures: u32,
    evidence: Option<Evidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Evidence {
    files: Files,
    report: PathBuf,
}

#[derive(Serialize)]
struct Report<'a> {
    format_version: u32,
    started_at_unix_ms: u128,
    commands: &'a [String],
    cwd: &'a Path,
    files: &'a Files,
    head: Option<String>,
    timeout_seconds: u64,
    outcome: String,
    results: Vec<CheckResult>,
}

#[derive(Serialize)]
struct CheckResult {
    command: String,
    outcome: String,
    exit_code: Option<i32>,
    elapsed_ms: u128,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl AcceptanceGate {
    pub fn new(commands: Vec<String>, timeout: Option<u64>) -> Option<Self> {
        (!commands.is_empty()).then(|| Self {
            commands,
            timeout_seconds: timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS),
            next_stage: Stage::Archive,
            failures: 0,
            evidence: None,
        })
    }

    pub fn check_selection(&self, commands: &[String], timeout: Option<u64>) -> Result<()> {
        if (!commands.is_empty() && commands != self.commands)
            || timeout.is_some_and(|value| value != self.timeout_seconds)
        {
            bail!(
                "acceptance commands or timeout differ from the checkpoint; use `accept-changes` to accept the maintainer's corrected configuration"
            );
        }
        Ok(())
    }

    pub fn guidance(&self) -> String {
        format!(
            "\n\nMAINTAINER ACCEPTANCE\nThe runner independently executes these pinned commands from the campaign root after final verification, with {} seconds per command:\n{}\nA successful model report cannot waive these checks. Preserve supplied tests, drivers, fixtures and their expectations unchanged. Repair implementation and derived plans to satisfy the original requirements. Add your own tests separately. Never substitute a weaker check or manipulate discovery to exclude supplied tests. These checks establish only their actual coverage; also satisfy all other supplied requirements. Output and real exit statuses are retained by the runner.\n",
            self.timeout_seconds,
            self.commands
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    pub fn is_current<U: Ui>(&self, repo: &Path, change: Option<&str>, ui: &U) -> Result<bool> {
        match &self.evidence {
            Some(evidence) => Ok(evidence.files == product_snapshot(repo, change, ui)?),
            None => Ok(false),
        }
    }

    /// None means passed; Some contains a bounded diagnostic for Repair.
    pub fn run<U: Ui>(
        &mut self,
        repo: &Path,
        change: Option<&str>,
        ui: &U,
        check_inputs: impl Fn() -> Result<()>,
    ) -> Result<Option<String>> {
        self.evidence = None;
        check_inputs()?;
        let before = product_snapshot(repo, change, ui)?;
        let directory = metadata_dir(repo, ui)?
            .join("acceptance")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&directory)?;
        let report_path = directory.join("result.json");
        let mut report = Report {
            format_version: 1,
            started_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
            commands: &self.commands,
            cwd: repo,
            files: &before,
            head: current_head(repo, ui)?,
            timeout_seconds: self.timeout_seconds,
            outcome: "running".to_owned(),
            results: Vec::new(),
        };
        write_report(&report_path, &report)?;
        ui.info(&format!("Acceptance evidence: `{}`", report_path.display()));
        for (index, command) in self.commands.iter().enumerate() {
            check_inputs()?;
            let mut result = CheckResult {
                command: command.clone(),
                outcome: "running".to_owned(),
                exit_code: None,
                elapsed_ms: 0,
                stdout: directory.join(format!("{}.stdout", index + 1)),
                stderr: directory.join(format!("{}.stderr", index + 1)),
            };
            let activity = format!(
                "Acceptance check {}/{}: {command}",
                index + 1,
                self.commands.len()
            );
            ui.start_stream(&activity);
            let execution = run_check(repo, &mut result, self.timeout_seconds, &activity, ui);
            let success = result.outcome == "passed";
            ui.finish_stream(
                success,
                &format!("{activity} ({}s elapsed)", result.elapsed_ms / 1000),
            );
            report.results.push(result);
            report.outcome = if success { "running" } else { "failed" }.to_owned();
            write_report(&report_path, &report)?;
            execution?; // Pause/interruption retains an incomplete gate and its log.
            if let Err(error) = check_inputs() {
                report.outcome = "protected-input-changed".to_owned();
                write_report(&report_path, &report)?;
                return Err(error);
            }
            if !success {
                let result = report.results.last().unwrap();
                return Ok(Some(format!(
                    "Maintainer acceptance failed: `{command}` ({}, exit {:?}). Evidence: {}.\nstdout (tail):\n{}\nstderr (tail):\n{}\nRepair the implementation against the supplied requirements. Keep maintainer-owned acceptance inputs unchanged; do not replace or weaken this check.",
                    result.outcome,
                    result.exit_code,
                    report_path.display(),
                    tail(&result.stdout)?,
                    tail(&result.stderr)?
                )));
            }
        }
        if before != product_snapshot(repo, change, ui)? {
            report.outcome = "inputs-changed-during-checks".to_owned();
            write_report(&report_path, &report)?;
            return Ok(Some(format!(
                "Acceptance commands changed tracked or non-ignored product inputs; results cannot certify the resulting tree. Finish required generation/setup in Apply/Repair, then rerun the unchanged checks. Evidence: {}",
                report_path.display()
            )));
        }
        report.outcome = "passed".to_owned();
        write_report(&report_path, &report)?;
        self.evidence = Some(Evidence {
            files: before,
            report: report_path,
        });
        ui.success("Maintainer acceptance passed");
        Ok(None)
    }
}

fn product_snapshot<U: Ui>(repo: &Path, change: Option<&str>, ui: &U) -> Result<Files> {
    let mut files = snapshot(repo, ui)?;
    if let Some(change) = change {
        let active = format!("openspec/changes/{change}/");
        files.retain(|path, _| {
            !path.starts_with(&active)
                && !path
                    .strip_prefix("openspec/changes/archive/")
                    .is_some_and(|archived| {
                        archived.split('/').next().is_some_and(|directory| {
                            // OpenSpec archives are named YYYY-MM-DD-<change>.
                            directory.get(11..) == Some(change)
                        })
                    })
        });
    }
    Ok(files)
}

fn write_report(path: &Path, report: &Report<'_>) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(report)?)?;
    Ok(())
}

fn tail(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::End(-(file.metadata()?.len().min(4096) as i64)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Read the evidence files without putting pipes or output backpressure in the check's path.
struct LiveOutput {
    file: File,
    pending: Vec<u8>,
}

impl LiveOutput {
    fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            pending: Vec::new(),
        })
    }

    fn forward<U: Ui>(
        &mut self,
        ui: &U,
        item: fn(String) -> StreamItem,
        finished: bool,
    ) -> Result<()> {
        loop {
            // Bound each poll's work so noisy checks still honour controls and timeouts.
            let mut bytes = [0; 8192];
            let count = self.file.read(&mut bytes)?;
            self.pending.extend_from_slice(&bytes[..count]);
            let end = if finished && count == 0 {
                self.pending.len()
            } else {
                self.pending
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |index| index + 1)
            };
            for line in String::from_utf8_lossy(&self.pending[..end]).lines() {
                ui.stream_item(&item(line.to_owned()));
            }
            self.pending.drain(..end);
            if !finished || count == 0 {
                return Ok(());
            }
        }
    }
}

fn run_check<U: Ui>(
    repo: &Path,
    result: &mut CheckResult,
    timeout: u64,
    activity: &str,
    ui: &U,
) -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", &result.command])
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(File::create(&result.stdout)?)
        .stderr(File::create(&result.stderr)?);
    let mut stdout = LiveOutput::open(&result.stdout)?;
    let mut stderr = LiveOutput::open(&result.stderr)?;
    configure_stream_process(&mut command);
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            result.outcome = format!("launch failed: {error}");
            return Ok(());
        }
    };
    let mut last_elapsed = None;
    let execution = (|| {
        loop {
            if let Some(guard) = ui.operating_policy()
                && let Err(error) = guard.poll()
            {
                result.outcome = "operating-policy stop; incomplete".into();
                return Err(error);
            }
            stdout.forward(ui, StreamItem::Stdout, false)?;
            stderr.forward(ui, StreamItem::Stderr, false)?;
            let elapsed = started.elapsed().as_secs();
            if last_elapsed != Some(elapsed) {
                ui.stream_progress(&format!("{elapsed}s elapsed · {activity}"));
                last_elapsed = Some(elapsed);
            }
            if let Some(status) = child.try_wait().context("waiting for acceptance check")? {
                result.exit_code = status.code();
                result.outcome = if status.success() { "passed" } else { "failed" }.to_owned();
                return Ok(());
            }
            if started.elapsed() >= Duration::from_secs(timeout) {
                result.outcome = "timed out".to_owned();
                return Ok(());
            }
            match ui.poll_stream() {
                StreamControl::None => {}
                StreamControl::Pause => {
                    result.outcome = "paused".to_owned();
                    return Err(PauseRequested::new(repo.to_owned()).into());
                }
                StreamControl::Interrupt => {
                    result.outcome = "interrupted".to_owned();
                    bail!("acceptance check interrupted; resume to rerun the gate");
                }
                _ => ui.warn(
                    "Agent controls are unavailable during runner-executed acceptance checks",
                ),
            }
            thread::sleep(Duration::from_millis(50));
        }
    })();
    // Also stop descendants if a driver exits without joining them.
    let cleanup = kill_stream_process(&mut child);
    let reaped = child.wait();
    let output = stdout
        .forward(ui, StreamItem::Stdout, true)
        .and_then(|()| stderr.forward(ui, StreamItem::Stderr, true));
    result.elapsed_ms = started.elapsed().as_millis();
    execution?;
    cleanup.context("stopping acceptance process group")?;
    reaped.context("reaping acceptance process")?;
    output.context("streaming acceptance output")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct ControlUi {
        control: Mutex<Option<StreamControl>>,
        items: Mutex<Vec<StreamItem>>,
        progress: Mutex<Vec<String>>,
        release: Option<PathBuf>,
    }
    impl Ui for ControlUi {
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
        fn stream_progress(&self, message: &str) {
            self.progress.lock().unwrap().push(message.to_owned());
        }
        fn poll_stream(&self) -> StreamControl {
            if let Some(release) = &self.release {
                let items = self.items.lock().unwrap();
                let progress = self.progress.lock().unwrap();
                if items.contains(&StreamItem::Stdout("first".into()))
                    && items.contains(&StreamItem::Stderr("warning".into()))
                    && progress.iter().any(|message| {
                        message.split_once("s elapsed").is_some_and(|(seconds, _)| {
                            seconds.parse::<u64>().is_ok_and(|seconds| seconds >= 1)
                        })
                    })
                {
                    fs::write(release, "continue").unwrap();
                }
            }
            self.control.lock().unwrap().take().unwrap_or_default()
        }
        fn stream_message_sent(&self, _: &str) {}
        fn stream_item(&self, item: &StreamItem) {
            self.items.lock().unwrap().push(item.clone());
        }
        fn finish_stream(&self, _: bool, _: &str) {}
        fn finish_dashboard(&self) {}
        fn output(&self, _: &str, _: &str) {}
        fn start_activity(&self, _: &str) -> Option<indicatif::ProgressBar> {
            None
        }
        fn finish_activity(&self, _: Option<indicatif::ProgressBar>, _: bool, _: &str) {}
    }

    #[test]
    fn pause_and_interrupt_stop_the_check_without_passing() {
        let root =
            std::env::temp_dir().join(format!("opsx-check-controls-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        for control in [StreamControl::Pause, StreamControl::Interrupt] {
            let mut result = CheckResult {
                command: "sleep 30".to_owned(),
                outcome: "running".to_owned(),
                exit_code: None,
                elapsed_ms: 0,
                stdout: root.join("stdout"),
                stderr: root.join("stderr"),
            };
            let pause = control == StreamControl::Pause;
            let error = run_check(
                &root,
                &mut result,
                60,
                "Acceptance check 1/1: sleep 30",
                &ControlUi {
                    control: Mutex::new(Some(control)),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert_eq!(error.downcast_ref::<PauseRequested>().is_some(), pause);
            assert!(result.elapsed_ms < 5000);
            assert_eq!(result.outcome, if pause { "paused" } else { "interrupted" });
            assert_eq!(result.exit_code, None);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn all_commands_must_pass_and_inputs_must_remain_stable() {
        let root = std::env::temp_dir().join(format!("opsx-check-gate-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("product"), "original").unwrap();
        let ui = ControlUi::default();
        let mut gate = AcceptanceGate::new(
            vec![
                "true".to_owned(),
                "echo exact-failure >&2; exit 17".to_owned(),
            ],
            Some(2),
        )
        .unwrap();
        assert!(gate.check_selection(&[], None).is_ok());
        assert!(gate.check_selection(&gate.commands, Some(3)).is_err());
        let finding = gate.run(&root, None, &ui, || Ok(())).unwrap().unwrap();
        assert!(finding.contains("Some(17)") && finding.contains("exact-failure"));
        assert!(!gate.is_current(&root, None, &ui).unwrap());
        let mut gate =
            AcceptanceGate::new(vec!["echo changed > product".to_owned()], None).unwrap();
        assert!(
            gate.run(&root, None, &ui, || Ok(()))
                .unwrap()
                .unwrap()
                .contains("changed tracked or non-ignored product inputs")
        );
        assert!(!gate.is_current(&root, None, &ui).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn output_and_elapsed_time_are_visible_before_the_check_exits() {
        let root = std::env::temp_dir().join(format!("opsx-check-live-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let ui = ControlUi {
            release: Some(root.join("release")),
            ..Default::default()
        };
        let mut result = CheckResult {
            command: "printf 'first\\n'; printf 'warning\\n' >&2; while [ ! -f release ]; do sleep 0.02; done; printf 'last'; printf 'tail' >&2; exit 17".into(),
            outcome: "running".into(),
            exit_code: None,
            elapsed_ms: 0,
            stdout: root.join("stdout"),
            stderr: root.join("stderr"),
        };
        run_check(&root, &mut result, 3, "Acceptance check 1/1", &ui).unwrap();
        assert_eq!(result.outcome, "failed");
        assert_eq!(result.exit_code, Some(17));
        assert!(result.elapsed_ms >= 1000);
        assert_eq!(fs::read(&result.stdout).unwrap(), b"first\nlast");
        assert_eq!(fs::read(&result.stderr).unwrap(), b"warning\ntail");
        let items = ui.items.lock().unwrap();
        let stdout = items
            .iter()
            .filter_map(|item| match item {
                StreamItem::Stdout(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let stderr = items
            .iter()
            .filter_map(|item| match item {
                StreamItem::Stderr(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(stdout, ["first", "last"]);
        assert_eq!(stderr, ["warning", "tail"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_output_preserves_split_utf8_and_flushes_the_final_partial_line_once() {
        let root = std::env::temp_dir().join(format!("opsx-check-output-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("stdout");
        let line = format!("{}é", "x".repeat(8191));
        let text = format!("{line}\nfinal partial line");
        fs::write(&path, &text).unwrap();
        let mut output = LiveOutput::open(&path).unwrap();
        let ui = ControlUi::default();
        output.forward(&ui, StreamItem::Stdout, false).unwrap();
        assert!(ui.items.lock().unwrap().is_empty());
        output.forward(&ui, StreamItem::Stdout, false).unwrap();
        assert_eq!(
            *ui.items.lock().unwrap(),
            [StreamItem::Stdout(line.clone())]
        );
        output.forward(&ui, StreamItem::Stdout, true).unwrap();
        output.forward(&ui, StreamItem::Stdout, true).unwrap();
        assert_eq!(
            *ui.items.lock().unwrap(),
            [
                StreamItem::Stdout(line),
                StreamItem::Stdout("final partial line".into())
            ]
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        fs::remove_dir_all(root).unwrap();
    }
}
