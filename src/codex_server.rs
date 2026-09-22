use std::{
    collections::{HashSet, VecDeque},
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{codex::CodexLauncher, ui::Ui};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const COMPACTION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(crate) struct CodexRequestError {
    method: String,
    error: Value,
}

impl CodexRequestError {
    pub(crate) fn is_missing_thread(&self, thread_id: &str) -> bool {
        self.method == "thread/resume"
            && self.error.get("code").and_then(Value::as_i64) == Some(-32600)
            && self.error.get("message").and_then(Value::as_str)
                == Some(format!("no rollout found for thread id {thread_id}").as_str())
    }
}

impl std::fmt::Display for CodexRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Codex App Server `{}` failed: {}",
            self.method,
            compact_json(&self.error)
        )
    }
}

impl std::error::Error for CodexRequestError {}

pub(crate) struct CodexServer {
    repo: std::path::PathBuf,
    launcher: CodexLauncher,
    client: Mutex<Option<CodexServerClient>>,
}

impl CodexServer {
    pub(crate) fn new(repo: &std::path::Path, launcher: &CodexLauncher) -> Self {
        Self {
            repo: repo.to_path_buf(),
            launcher: launcher.clone(),
            client: Mutex::new(None),
        }
    }

    pub(crate) fn client<U: Ui>(&self, ui: &U) -> Result<CodexServerClient> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex server state lock was poisoned"))?;
        if let Some(client) = client.as_ref() {
            return Ok(client.clone());
        }
        let started = CodexServerClient::start(&self.launcher, &self.repo, ui)?;
        *client = Some(started.clone());
        Ok(started)
    }
}

enum Incoming {
    Message(Value),
    Invalid(String),
    ReadError(String),
    Eof,
}

struct CodexClientInner {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    incoming: Mutex<Receiver<Incoming>>,
    deferred: Mutex<VecDeque<Value>>,
    next_request_id: AtomicU64,
    active_threads: Mutex<HashSet<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Drop for CodexClientInner {
    fn drop(&mut self) {
        if let Ok(stdin) = self.stdin.get_mut() {
            stdin.take();
        }
        if let Ok(child) = self.child.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Clone)]
pub(crate) struct CodexServerClient {
    inner: Arc<CodexClientInner>,
}

impl CodexServerClient {
    pub(crate) fn start<U: Ui>(
        launcher: &CodexLauncher,
        repo: &std::path::Path,
        ui: &U,
    ) -> Result<Self> {
        let mut command = Command::new(&launcher.program);
        command
            .args(&launcher.prefix_args)
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in &launcher.unset_environment {
            command.env_remove(name);
        }
        for variable in &launcher.environment {
            command.env(&variable.name, &variable.value);
        }
        ui.debug(&format!(
            "Codex App Server: {}",
            launcher.server_command(repo).display()
        ));
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch Codex App Server `{}`", launcher.program))?;
        let stdin = child
            .stdin
            .take()
            .context("Codex App Server did not expose stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex App Server did not expose stdout")?;
        let stderr_pipe = child
            .stderr
            .take()
            .context("Codex App Server did not expose stderr")?;
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => match serde_json::from_str::<Value>(&line) {
                        Ok(value) => {
                            if sender.send(Incoming::Message(value)).is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            if sender.send(Incoming::Invalid(line)).is_err() {
                                return;
                            }
                        }
                    },
                    Err(error) => {
                        let _ = sender.send(Incoming::ReadError(error.to_string()));
                        return;
                    }
                }
            }
            let _ = sender.send(Incoming::Eof);
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let captured_stderr = stderr.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                if let Ok(mut output) = captured_stderr.lock() {
                    if output.len() > 32 * 1024 {
                        output.drain(..16 * 1024);
                    }
                    output.push_str(&line);
                    output.push('\n');
                }
            }
        });
        let client = Self {
            inner: Arc::new(CodexClientInner {
                child: Mutex::new(child),
                stdin: Mutex::new(Some(stdin)),
                incoming: Mutex::new(receiver),
                deferred: Mutex::new(VecDeque::new()),
                next_request_id: AtomicU64::new(1),
                active_threads: Mutex::new(HashSet::new()),
                stderr,
            }),
        };
        client.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "opsx-build",
                    "title": "opsx-build",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {"experimentalApi": true}
            }),
            REQUEST_TIMEOUT,
        )?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    pub(crate) fn start_thread(
        &self,
        launcher: &CodexLauncher,
        repo: &std::path::Path,
        permission_mode: &str,
        name: Option<&str>,
        additional_roots: &[std::path::PathBuf],
        ephemeral: bool,
    ) -> Result<String> {
        let mut params = launcher.thread_params(repo, permission_mode, additional_roots)?;
        params["ephemeral"] = json!(ephemeral);
        let response = self.request("thread/start", params, REQUEST_TIMEOUT)?;
        let thread_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("Codex thread/start response omitted thread.id")?
            .to_owned();
        self.remember_thread(&thread_id)?;
        if let Some(name) = name.filter(|_| !ephemeral) {
            self.rename_thread(&thread_id, name)?;
        }
        Ok(thread_id)
    }

    pub(crate) fn ensure_thread(
        &self,
        launcher: &CodexLauncher,
        repo: &std::path::Path,
        permission_mode: &str,
        thread_id: &str,
        additional_roots: &[std::path::PathBuf],
    ) -> Result<()> {
        if self
            .inner
            .active_threads
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex active-thread state lock was poisoned"))?
            .contains(thread_id)
        {
            return Ok(());
        }
        let mut params = launcher.thread_params(repo, permission_mode, additional_roots)?;
        params["threadId"] = json!(thread_id);
        params["excludeTurns"] = json!(true);
        self.request("thread/resume", params, REQUEST_TIMEOUT)?;
        self.remember_thread(thread_id)
    }

    pub(crate) fn rename_thread(&self, thread_id: &str, name: &str) -> Result<()> {
        self.request(
            "thread/name/set",
            json!({"threadId": thread_id, "name": name}),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    pub(crate) fn start_turn(
        &self,
        thread_id: &str,
        input: Value,
        output_schema: Value,
        sandbox_policy: Option<Value>,
    ) -> Result<String> {
        let mut params = json!({
            "threadId": thread_id,
            "input": input,
            "outputSchema": output_schema
        });
        if let Some(sandbox_policy) = sandbox_policy {
            params["sandboxPolicy"] = sandbox_policy;
        }
        let response = self.request("turn/start", params, REQUEST_TIMEOUT)?;
        response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("Codex turn/start response omitted turn.id")
    }

    pub(crate) fn steer_turn(&self, thread_id: &str, turn_id: &str, message: &str) -> Result<()> {
        self.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": [{"type": "text", "text": message}]
            }),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    pub(crate) fn interrupt_turn(&self, thread_id: &str, turn_id: &str) -> Result<()> {
        self.request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
            REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    pub(crate) fn compact_thread(&self, thread_id: &str) -> Result<()> {
        self.request(
            "thread/compact/start",
            json!({"threadId": thread_id}),
            COMPACTION_TIMEOUT,
        )?;
        let deadline = Instant::now() + COMPACTION_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("Codex did not confirm compaction of thread `{thread_id}`");
            }
            let event = self.next_event(remaining.min(Duration::from_millis(250)))?;
            if event.as_ref().is_some_and(|event| {
                event.get("method").and_then(Value::as_str) == Some("thread/compacted")
                    && event.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
            }) {
                return Ok(());
            }
        }
    }

    pub(crate) fn next_event(&self, timeout: Duration) -> Result<Option<Value>> {
        if let Some(value) = self
            .inner
            .deferred
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex event queue lock was poisoned"))?
            .pop_front()
        {
            return Ok(Some(value));
        }
        self.receive(timeout)
    }

    pub(crate) fn stderr_excerpt(&self) -> String {
        self.inner
            .stderr
            .lock()
            .map(|value| value.trim().to_owned())
            .unwrap_or_default()
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        self.write(&json!({"id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("Codex App Server timed out waiting for `{method}`");
            }
            let Some(message) = self.receive(remaining.min(Duration::from_millis(250)))? else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(CodexRequestError {
                        method: method.to_owned(),
                        error: error.clone(),
                    }
                    .into());
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.inner
                .deferred
                .lock()
                .map_err(|_| anyhow::anyhow!("Codex event queue lock was poisoned"))?
                .push_back(message);
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"method": method, "params": params}))
    }

    fn write(&self, value: &Value) -> Result<()> {
        let mut guard = self
            .inner
            .stdin
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex stdin lock was poisoned"))?;
        let stdin = guard
            .as_mut()
            .context("Codex App Server stdin is already closed")?;
        serde_json::to_writer(&mut *stdin, value).context("failed writing Codex request")?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn receive(&self, timeout: Duration) -> Result<Option<Value>> {
        let receiver = self
            .inner
            .incoming
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex response receiver lock was poisoned"))?;
        match receiver.recv_timeout(timeout) {
            Ok(Incoming::Message(value)) => Ok(Some(value)),
            Ok(Incoming::Invalid(line)) => {
                bail!("Codex App Server emitted non-JSON output: {line}")
            }
            Ok(Incoming::ReadError(error)) => {
                bail!("failed reading Codex App Server output: {error}")
            }
            Ok(Incoming::Eof) => {
                let stderr = self.stderr_excerpt();
                if stderr.is_empty() {
                    bail!("Codex App Server exited unexpectedly")
                }
                bail!("Codex App Server exited unexpectedly: {stderr}")
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                bail!("Codex App Server response channel closed unexpectedly")
            }
        }
    }

    fn remember_thread(&self, thread_id: &str) -> Result<()> {
        self.inner
            .active_threads
            .lock()
            .map_err(|_| anyhow::anyhow!("Codex active-thread state lock was poisoned"))?
            .insert(thread_id.to_owned());
        Ok(())
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{
        backend::{AgentBackend, SessionId, SessionMode, StageProtocol},
        codex::{CodexBackend, CodexLauncher},
        stream::{StreamControl, StreamItem},
        ui::Ui,
    };
    use uuid::Uuid;

    struct QuietUi;

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

    fn planning_test_launcher(directory: &std::path::Path, resume_reply: &str) -> CodexLauncher {
        let script = directory.join("server.sh");
        std::fs::write(
            &script,
            r#"id=0
while IFS= read -r request; do
  printf '%s\n' "$request" >> requests.jsonl
  case "$request" in
    *'"method":"initialized"'*) continue ;;
  esac
  id=$((id + 1))
  case "$request" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"thread/resume"'*) printf '{"id":%s,__RESUME_REPLY__}\n' "$id" ;;
    *'"method":"thread/start"'*) printf '{"id":%s,"result":{"thread":{"id":"actual-thread"}}}\n' "$id" ;;
    *'"method":"thread/name/set"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
    *'"method":"turn/start"'*) printf '{"id":%s,"error":{"code":-32603,"message":"simulated turn failure"}}\n' "$id" ;;
    *) exit 1 ;;
  esac
done
"#
            .replace("__RESUME_REPLY__", resume_reply),
        )
        .unwrap();
        CodexLauncher {
            connection_name: None,
            environment_name: None,
            program: "sh".to_owned(),
            prefix_args: vec![script.display().to_string()],
            model: None,
            permission_profile: Some("build-permissions".to_owned()),
            context_window: None,
            auto_compact_window: None,
            auto_compact_percent: None,
            max_output_tokens: None,
            environment: Vec::new(),
            unset_environment: Vec::new(),
        }
    }

    #[test]
    fn planning_session_is_resolved_before_a_failed_turn() {
        for (mode, resume_reply, expected_methods) in [
            (
                SessionMode::New {
                    id: SessionId::new("placeholder"),
                    name: Some("planning".to_owned()),
                },
                r#""result":{}"#,
                vec![
                    "initialize",
                    "initialized",
                    "thread/start",
                    "thread/name/set",
                    "turn/start",
                ],
            ),
            (
                SessionMode::Resume {
                    id: SessionId::new("actual-thread"),
                },
                r#""result":{"thread":{"id":"actual-thread"}}"#,
                vec!["initialize", "initialized", "thread/resume", "turn/start"],
            ),
            (
                SessionMode::Resume {
                    id: SessionId::new("missing-thread"),
                },
                r#""error":{"code":-32600,"message":"no rollout found for thread id missing-thread"}"#,
                vec![
                    "initialize",
                    "initialized",
                    "thread/resume",
                    "thread/start",
                    "thread/name/set",
                    "turn/start",
                ],
            ),
        ] {
            let directory =
                std::env::temp_dir().join(format!("opsx-codex-planning-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let launcher = planning_test_launcher(&directory, resume_reply);
            let backend = CodexBackend::new(&directory, &launcher, "auto", None, &QuietUi);
            let session = backend.prepare_session(mode).unwrap();
            assert!(
                matches!(&session, SessionMode::Resume { id } if id.as_str() == "actual-thread")
            );

            let error = backend
                .invoke(
                    session,
                    "continue existing proposal",
                    "Propose",
                    StageProtocol::Propose,
                )
                .unwrap_err();
            assert!(error.to_string().contains("simulated turn failure"));
            let requests: Vec<Value> = std::fs::read_to_string(directory.join("requests.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let methods: Vec<_> = requests
                .iter()
                .map(|request| request["method"].as_str().unwrap())
                .collect();
            assert_eq!(methods, expected_methods);
            for request in &requests {
                match request["method"].as_str().unwrap() {
                    "thread/start" | "thread/resume" => {
                        assert_eq!(
                            request["params"]["config"]["default_permissions"],
                            "build-permissions"
                        );
                        assert_eq!(
                            request["params"]["cwd"],
                            directory.to_string_lossy().as_ref()
                        );
                        assert!(request["params"].get("sandbox").is_none());
                        if request["method"] == "thread/start" {
                            assert_eq!(request["params"]["ephemeral"], false);
                        }
                    }
                    "turn/start" => assert_eq!(request["params"]["threadId"], "actual-thread"),
                    _ => {}
                }
            }
            drop(backend);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn planning_resume_does_not_replace_threads_for_other_errors() {
        for (code, message) in [
            (-32600, "invalid permission profile"),
            (-32603, "authentication failed"),
            (-32600, "no rollout found for thread id another-thread"),
            (-32603, "no rollout found for thread id missing-thread"),
        ] {
            let directory =
                std::env::temp_dir().join(format!("opsx-codex-resume-error-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let reply = format!("\"error\":{}", json!({"code": code, "message": message}));
            let launcher = planning_test_launcher(&directory, &reply);
            let backend = CodexBackend::new(&directory, &launcher, "auto", None, &QuietUi);
            let error = backend
                .prepare_session(SessionMode::Resume {
                    id: SessionId::new("missing-thread"),
                })
                .unwrap_err();
            assert!(error.to_string().contains(message));
            let requests = std::fs::read_to_string(directory.join("requests.jsonl")).unwrap();
            assert!(!requests.contains("thread/start"));
            assert!(!requests.contains("turn/start"));
            drop(backend);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn initializes_starts_names_and_compacts_over_json_lines() {
        let directory = std::env::temp_dir().join(format!("opsx-codex-rpc-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("server.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
read initialize
printf '%s\n' '{"id":1,"result":{"userAgent":"test","codexHome":"/tmp","platformFamily":"unix","platformOs":"test"}}'
read initialized
read start
printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-1"},"model":"test","modelProvider":"test","cwd":"/tmp","approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"workspaceWrite"}}}'
read rename
printf '%s\n' '{"id":3,"result":{}}'
read compact
printf '%s\n' '{"method":"thread/compacted","params":{"threadId":"thread-1","turnId":"compact-1"}}'
printf '%s\n' '{"id":4,"result":{}}'
read remaining
"#,
        )
        .unwrap();
        let launcher = CodexLauncher {
            connection_name: None,
            environment_name: None,
            program: "sh".to_owned(),
            prefix_args: vec![script.display().to_string()],
            model: None,
            permission_profile: None,
            context_window: None,
            auto_compact_window: None,
            auto_compact_percent: None,
            max_output_tokens: None,
            environment: Vec::new(),
            unset_environment: Vec::new(),
        };
        let client = CodexServerClient::start(&launcher, &directory, &QuietUi).unwrap();
        let thread = client
            .start_thread(&launcher, &directory, "dontAsk", Some("slice"), &[], false)
            .unwrap();
        assert_eq!(thread, "thread-1");
        client.compact_thread(&thread).unwrap();
        drop(client);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
