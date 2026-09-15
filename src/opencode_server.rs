use std::{
    io::Read,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, ChildStderr, Stdio},
    sync::Mutex,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::{
    opencode::{OpenCodeLauncher, build_opencode_server_command},
    process::{
        command_for, configure_stream_process, interrupt_stream_process, kill_stream_process,
    },
    ui::Ui,
};

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const COMPACTION_REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const SERVER_STOP_GRACE: Duration = Duration::from_millis(500);
const MAX_SERVER_DIAGNOSTIC_BYTES: usize = 64 * 1024;

pub(crate) struct OpenCodeServer {
    repo: PathBuf,
    launcher: OpenCodeLauncher,
    process: Mutex<Option<ServerProcess>>,
    #[cfg(test)]
    external_client: Option<OpenCodeServerClient>,
}

impl OpenCodeServer {
    pub(crate) fn new(repo: &Path, launcher: &OpenCodeLauncher) -> Self {
        Self {
            repo: repo.to_path_buf(),
            launcher: launcher.clone(),
            process: Mutex::new(None),
            #[cfg(test)]
            external_client: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn external(repo: &Path, launcher: &OpenCodeLauncher, base_url: String) -> Self {
        Self {
            repo: repo.to_path_buf(),
            launcher: launcher.clone(),
            process: Mutex::new(None),
            external_client: Some(OpenCodeServerClient::new(
                base_url,
                repo.to_path_buf(),
                launcher.model.clone(),
                launcher.context_window,
            )),
        }
    }

    pub(crate) fn client<U: Ui>(&self, ui: &U) -> Result<OpenCodeServerClient> {
        #[cfg(test)]
        if let Some(client) = &self.external_client {
            return Ok(client.clone());
        }
        let mut process = self
            .process
            .lock()
            .map_err(|_| anyhow::anyhow!("OpenCode server state lock was poisoned"))?;
        if let Some(running) = process.as_mut() {
            match running.child.try_wait()? {
                None => return Ok(running.client.clone()),
                Some(status) => {
                    let mut stopped = process
                        .take()
                        .expect("running OpenCode server remains in state");
                    let diagnostic = stopped.take_diagnostic();
                    bail!(
                        "OpenCode server exited unexpectedly with {status}{}",
                        suffix_diagnostic(&diagnostic)
                    );
                }
            }
        }

        let port = reserve_loopback_port()?;
        let spec = build_opencode_server_command(&self.repo, &self.launcher, port);
        ui.command(&spec.display());
        let mut command = command_for(&spec);
        command.stdout(Stdio::null()).stderr(Stdio::piped());
        configure_stream_process(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch OpenCode server `{}`", spec.program))?;
        let stderr = child.stderr.take().map(spawn_stderr_reader);
        let client = OpenCodeServerClient::new(
            format!("http://127.0.0.1:{port}"),
            self.repo.clone(),
            self.launcher.model.clone(),
            self.launcher.context_window,
        );
        let started = Instant::now();
        loop {
            if client.health().is_ok() {
                ui.debug(&format!("OpenCode control server: {}", client.base_url));
                *process = Some(ServerProcess {
                    child,
                    stderr,
                    client: client.clone(),
                });
                return Ok(client);
            }
            if let Some(status) = child.try_wait()? {
                let diagnostic = join_stderr(stderr);
                bail!(
                    "OpenCode server exited with {status} before becoming ready{}",
                    suffix_diagnostic(&diagnostic)
                );
            }
            if started.elapsed() >= SERVER_START_TIMEOUT {
                stop_server_process(&mut child);
                let diagnostic = join_stderr(stderr);
                bail!(
                    "OpenCode server did not become ready within {} seconds{}",
                    SERVER_START_TIMEOUT.as_secs(),
                    suffix_diagnostic(&diagnostic)
                );
            }
            thread::sleep(SERVER_POLL_INTERVAL);
        }
    }
}

struct ServerProcess {
    child: Child,
    stderr: Option<JoinHandle<String>>,
    client: OpenCodeServerClient,
}

impl ServerProcess {
    fn take_diagnostic(&mut self) -> String {
        join_stderr(self.stderr.take())
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        stop_server_process(&mut self.child);
        let _ = self.take_diagnostic();
    }
}

#[derive(Clone)]
pub(crate) struct OpenCodeServerClient {
    agent: ureq::Agent,
    base_url: String,
    directory: PathBuf,
    configured_model: Option<String>,
    context_window: Option<u64>,
}

impl OpenCodeServerClient {
    fn new(
        base_url: String,
        directory: PathBuf,
        configured_model: Option<String>,
        context_window: Option<u64>,
    ) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_millis(250))
                .timeout_read(Duration::from_secs(5))
                .timeout_write(Duration::from_secs(5))
                .build(),
            base_url,
            directory,
            configured_model,
            context_window,
        }
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    fn health(&self) -> Result<()> {
        let value =
            self.request_json("GET", "/global/health", None, false, Duration::from_secs(1))?;
        if value.get("healthy").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            bail!("OpenCode server returned an unhealthy response")
        }
    }

    pub(crate) fn create_session(&self, title: Option<&str>) -> Result<String> {
        let body = title.map_or_else(|| json!({}), |title| json!({ "title": title }));
        let value = self.request_json(
            "POST",
            "/session",
            Some(&body),
            true,
            CONTROL_REQUEST_TIMEOUT,
        )?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("OpenCode create-session response omitted its session ID")
    }

    pub(crate) fn rename_session(&self, session_id: &str, title: &str) -> Result<()> {
        self.request_json(
            "PATCH",
            &format!("/session/{session_id}"),
            Some(&json!({ "title": title })),
            true,
            CONTROL_REQUEST_TIMEOUT,
        )?;
        Ok(())
    }

    pub(crate) fn abort_session(&self, session_id: &str) -> Result<()> {
        let value = self.request_json(
            "POST",
            &format!("/session/{session_id}/abort"),
            Some(&json!({})),
            true,
            CONTROL_REQUEST_TIMEOUT,
        )?;
        if value.as_bool() == Some(false) {
            bail!("OpenCode reported that session `{session_id}` was not active")
        }
        Ok(())
    }

    pub(crate) fn compact_session(&self, session_id: &str) -> Result<()> {
        let (provider_id, model_id) = self.session_model(session_id)?;
        let value = self.request_json(
            "POST",
            &format!("/session/{session_id}/summarize"),
            Some(&json!({
                "providerID": provider_id,
                "modelID": model_id,
                "auto": false
            })),
            true,
            COMPACTION_REQUEST_TIMEOUT,
        )?;
        if value.as_bool() != Some(true) {
            bail!("OpenCode did not confirm compaction of session `{session_id}`")
        }
        Ok(())
    }

    pub(crate) fn context_report(&self, session_id: &str) -> Result<String> {
        let session = self.request_json(
            "GET",
            &format!("/session/{session_id}"),
            None,
            true,
            CONTROL_REQUEST_TIMEOUT,
        )?;
        let messages = self.request_json(
            "GET",
            &format!("/session/{session_id}/message"),
            None,
            true,
            CONTROL_REQUEST_TIMEOUT,
        )?;
        let latest = messages.as_array().and_then(|messages| {
            messages.iter().rev().find_map(|message| {
                let info = message.get("info")?;
                (info.get("role").and_then(Value::as_str) == Some("assistant")).then_some(info)
            })
        });
        let latest_input = token_value(latest, "/tokens/input");
        let cache_read = token_value(latest, "/tokens/cache/read");
        let cache_write = token_value(latest, "/tokens/cache/write");
        let latest_output = token_value(latest, "/tokens/output");
        let estimated_context = latest_input + cache_read + cache_write;
        let capacity = self.context_window.map(|window| {
            let percent = if window == 0 {
                0.0
            } else {
                estimated_context as f64 * 100.0 / window as f64
            };
            format!(" of {window} configured ({percent:.1}%)")
        });
        let session_input = token_value(Some(&session), "/tokens/input");
        let session_output = token_value(Some(&session), "/tokens/output");
        Ok(format!(
            "OpenCode context for `{session_id}`: latest turn reported {latest_input} input + {cache_read} cache-read + {cache_write} cache-write tokens (approximately {estimated_context} context{}), with {latest_output} output tokens. Session cumulative totals: {session_input} input, {session_output} output.",
            capacity.unwrap_or_default()
        ))
    }

    fn session_model(&self, session_id: &str) -> Result<(String, String)> {
        if let Some(model) = self.configured_model.as_deref() {
            return split_model(model);
        }
        let session = self.request_json(
            "GET",
            &format!("/session/{session_id}"),
            None,
            true,
            CONTROL_REQUEST_TIMEOUT,
        )?;
        if let (Some(provider), Some(model)) = (
            session.pointer("/model/providerID").and_then(Value::as_str),
            session.pointer("/model/id").and_then(Value::as_str),
        ) {
            return Ok((provider.to_owned(), model.to_owned()));
        }
        let messages = self.request_json(
            "GET",
            &format!("/session/{session_id}/message"),
            None,
            true,
            CONTROL_REQUEST_TIMEOUT,
        )?;
        messages
            .as_array()
            .and_then(|messages| {
                messages.iter().rev().find_map(|message| {
                    let info = message.get("info")?;
                    let provider = info.get("providerID")?.as_str()?;
                    let model = info.get("modelID")?.as_str()?;
                    Some((provider.to_owned(), model.to_owned()))
                })
            })
            .context(
                "OpenCode session has no model metadata; configure `model = \"provider/model\"` for hard compaction",
            )
    }

    fn request_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        scoped: bool,
        timeout: Duration,
    ) -> Result<Value> {
        let url = format!("{}{path}", self.base_url);
        let mut request = self.agent.request(method, &url).timeout(timeout);
        if scoped {
            request = request.query("directory", &self.directory.to_string_lossy());
        }
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        }
        .map_err(|error| api_error(method, path, error))?;
        response
            .into_json::<Value>()
            .with_context(|| format!("OpenCode {method} {path} returned invalid JSON"))
    }
}

fn split_model(model: &str) -> Result<(String, String)> {
    model
        .split_once('/')
        .map(|(provider, model)| (provider.to_owned(), model.to_owned()))
        .with_context(|| format!("OpenCode model `{model}` is not in provider/model form"))
}

fn token_value(value: Option<&Value>, pointer: &str) -> u64 {
    value
        .and_then(|value| value.pointer(pointer))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn api_error(method: &str, path: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(status, response) => {
            let detail = response.into_string().unwrap_or_default();
            anyhow::anyhow!(
                "OpenCode {method} {path} failed with HTTP {status}{}",
                suffix_diagnostic(&detail)
            )
        }
        ureq::Error::Transport(error) => {
            anyhow::anyhow!("OpenCode {method} {path} failed: {error}")
        }
    }
}

fn reserve_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("could not reserve a loopback port for the OpenCode server")?;
    Ok(listener.local_addr()?.port())
}

fn spawn_stderr_reader(mut stderr: ChildStderr) -> JoinHandle<String> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    captured.extend_from_slice(&buffer[..count]);
                    if captured.len() > MAX_SERVER_DIAGNOSTIC_BYTES {
                        let excess = captured.len() - MAX_SERVER_DIAGNOSTIC_BYTES;
                        captured.drain(..excess);
                    }
                }
            }
        }
        String::from_utf8_lossy(&captured).into_owned()
    })
}

fn join_stderr(stderr: Option<JoinHandle<String>>) -> String {
    stderr
        .and_then(|thread| thread.join().ok())
        .unwrap_or_default()
}

fn stop_server_process(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = interrupt_stream_process(child);
    let started = Instant::now();
    while started.elapsed() < SERVER_STOP_GRACE {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(_) => break,
        }
    }
    let _ = kill_stream_process(child);
    let _ = child.wait();
}

fn suffix_diagnostic(diagnostic: &str) -> String {
    let diagnostic = diagnostic.trim();
    if diagnostic.is_empty() {
        String::new()
    } else {
        format!(": {diagnostic}")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread::JoinHandle,
    };

    use super::*;

    fn fake_json_server(responses: Vec<&str>) -> (String, mpsc::Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        let responses = responses.into_iter().map(str::to_owned).collect::<Vec<_>>();
        let thread = std::thread::spawn(move || {
            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                sender.send(request).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (url, receiver, thread)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    #[test]
    fn splits_provider_from_a_model_name_that_contains_slashes() {
        assert_eq!(
            split_model("openrouter/qwen/qwen3.6-35b-a3b").unwrap(),
            ("openrouter".to_owned(), "qwen/qwen3.6-35b-a3b".to_owned())
        );
    }

    #[test]
    fn missing_token_fields_are_reported_as_zero() {
        assert_eq!(
            token_value(Some(&json!({ "tokens": {} })), "/tokens/input"),
            0
        );
    }

    #[test]
    fn context_report_uses_latest_turn_and_configured_window() {
        let session = r#"{"tokens":{"input":900,"output":100}}"#;
        let messages = r#"[{"info":{"role":"assistant","tokens":{"input":300,"output":40,"cache":{"read":150,"write":50}}}}]"#;
        let (url, requests, thread) = fake_json_server(vec![session, messages]);
        let client = OpenCodeServerClient::new(
            url,
            PathBuf::from("/repo"),
            Some("openrouter/qwen/model".to_owned()),
            Some(1_000),
        );

        let report = client.context_report("ses_123").unwrap();

        assert!(report.contains("approximately 500 context of 1000 configured (50.0%)"));
        assert!(report.contains("Session cumulative totals: 900 input, 100 output"));
        assert!(
            requests
                .recv()
                .unwrap()
                .starts_with("GET /session/ses_123?directory=%2Frepo")
        );
        assert!(
            requests
                .recv()
                .unwrap()
                .starts_with("GET /session/ses_123/message?directory=%2Frepo")
        );
        thread.join().unwrap();
    }

    #[test]
    fn compaction_sends_the_selected_provider_and_full_model_id() {
        let (url, requests, thread) = fake_json_server(vec!["true"]);
        let client = OpenCodeServerClient::new(
            url,
            PathBuf::from("/repo"),
            Some("openrouter/qwen/qwen3.6-35b-a3b".to_owned()),
            None,
        );

        client.compact_session("ses_123").unwrap();

        let request = requests.recv().unwrap();
        assert!(request.starts_with("POST /session/ses_123/summarize?directory=%2Frepo"));
        assert!(request.contains(r#""providerID":"openrouter""#));
        assert!(request.contains(r#""modelID":"qwen/qwen3.6-35b-a3b""#));
        thread.join().unwrap();
    }
}
