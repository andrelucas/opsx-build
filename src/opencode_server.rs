use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdin, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::{
    opencode::{OpenCodeLauncher, build_opencode_server_command},
    process::{
        command_for, configure_stream_process, interrupt_stream_process, kill_stream_process,
    },
    ui::Ui,
};

const SERVER_START_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_STOP_GRACE: Duration = Duration::from_millis(500);
const MAX_SERVER_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_EVENT_BYTES: usize = 1024 * 1024;

pub(crate) struct OpenCodeServer {
    repo: PathBuf,
    launcher: OpenCodeLauncher,
    process: Mutex<Option<(ServerProcess, OpenCodeServerClient)>>,
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
        let mut server = Self::new(repo, launcher);
        server.external_client = Some(OpenCodeServerClient::new(
            base_url,
            repo.to_path_buf(),
            launcher.model.clone(),
            launcher.context_window,
            None,
        ));
        server
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
        if let Some((running, client)) = process.as_mut() {
            if let Some(status) = running.child.try_wait()? {
                bail!("OpenCode V2 private server exited unexpectedly with {status}");
            }
            return Ok(client.clone());
        }

        let password = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let spec = build_opencode_server_command(&self.repo, &self.launcher)
            .secret_env("OPENCODE_PASSWORD", &password);
        ui.command(&spec.display());
        let mut command = command_for(&spec);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_stream_process(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to launch OpenCode V2 server `{}`", spec.program))?;
        let lease = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .context("OpenCode server stdout was not piped")?;
        let stderr = child.stderr.take().map(spawn_stderr_reader);
        let (sender, receiver) = mpsc::sync_channel(1);
        let stdout = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let result = reader
                .by_ref()
                .take(MAX_SERVER_DIAGNOSTIC_BYTES as u64 + 1)
                .read_line(&mut line)
                .map(|_| line);
            let _ = sender.send(result);
            // Drain later writes, including logs, without exposing the lease credential.
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        let mut running = ServerProcess {
            child,
            lease,
            stderr,
            stdout: Some(stdout),
        };
        let ready = receiver.recv_timeout(SERVER_START_TIMEOUT).context(
            "OpenCode V2 (2.2+) did not report its private-server handshake; upgrade OpenCode",
        )?;
        let ready = ready.context("could not read OpenCode V2 private-server handshake")?;
        let url = serde_json::from_str::<Value>(&ready)
            .ok()
            .and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_owned));
        let Some(url) = url
            .filter(|url| url.starts_with("http://127.0.0.1:") || url.starts_with("http://[::1]:"))
        else {
            stop_server_process(&mut running.child);
            let diagnostic = join_stderr(running.stderr.take());
            bail!(
                "OpenCode V2 (2.2+) private-server startup failed; upgrade OpenCode{}",
                suffix_diagnostic(&diagnostic)
            );
        };
        let client = OpenCodeServerClient::new(
            url,
            self.repo.clone(),
            self.launcher.model.clone(),
            self.launcher.context_window,
            Some(password),
        );
        client.check_version()?;
        ui.debug(&format!("OpenCode V2 private server: {}", client.base_url));
        *process = Some((running, client.clone()));
        Ok(client)
    }
}

struct ServerProcess {
    child: Child,
    lease: Option<ChildStdin>,
    stderr: Option<JoinHandle<String>>,
    stdout: Option<JoinHandle<()>>,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.lease.take();
        stop_server_process(&mut self.child);
        let _ = join_stderr(self.stderr.take());
        if let Some(stdout) = self.stdout.take() {
            let _ = stdout.join();
        }
    }
}

#[derive(Clone)]
pub(crate) struct OpenCodeServerClient {
    agent: ureq::Agent,
    base_url: String,
    directory: PathBuf,
    configured_model: Option<String>,
    context_window: Option<u64>,
    authorization: Option<String>,
    // One subscription for the private server's lifetime. Ending a turn does not
    // strand a reader thread or require replaying live-only events on reconnect.
    events: Arc<Mutex<Option<mpsc::Receiver<Result<Value>>>>>,
}

impl OpenCodeServerClient {
    fn new(
        base_url: String,
        directory: PathBuf,
        configured_model: Option<String>,
        context_window: Option<u64>,
        password: Option<String>,
    ) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(1))
                // V2 sends SSE heartbeats every 15 seconds, including prefill.
                .timeout_read(Duration::from_secs(90))
                .timeout_write(CONTROL_REQUEST_TIMEOUT)
                .build(),
            base_url,
            directory,
            configured_model,
            context_window,
            authorization: password
                .map(|p| format!("Basic {}", STANDARD.encode(format!("opencode:{p}")))),
            events: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn check_version(&self) -> Result<()> {
        let info = self.request_json("GET", "/api/info", None)?;
        let version = info
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let mut parts = version.split('.');
        if parts.next() != Some("2")
            || parts
                .next()
                .and_then(|n| n.parse::<u32>().ok())
                .is_none_or(|n| n < 2)
        {
            bail!(
                "opsx-build requires OpenCode V2 2.2 or newer in the V2 series; server reports {version}. Upgrade OpenCode before resuming"
            );
        }
        Ok(())
    }

    pub(crate) fn create_session(&self, title: Option<&str>) -> Result<String> {
        let mut body = json!({"location": {"directory": self.directory}});
        if let Some(title) = title {
            body["title"] = title.into();
        }
        if let Some(model) = &self.configured_model {
            body["model"] = model_ref(model)?;
        }
        let value = self.request_json("POST", "/api/session", Some(&body))?;
        value
            .pointer("/data/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("OpenCode V2 create-session response omitted data.id")
    }

    pub(crate) fn prepare_session(&self, session_id: &str) -> Result<()> {
        let value = self.request_json("GET", &format!("/api/session/{session_id}"), None)?;
        let directory = value
            .pointer("/data/location/directory")
            .and_then(Value::as_str)
            .context("OpenCode V2 session response omitted its location")?;
        let actual = Path::new(directory)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(directory));
        let expected = self
            .directory
            .canonicalize()
            .unwrap_or_else(|_| self.directory.clone());
        if actual != expected {
            bail!(
                "OpenCode session `{session_id}` belongs to `{directory}`, not `{}`; select the correct checkpoint",
                self.directory.display()
            );
        }
        if let Some(model) = &self.configured_model {
            let model = model_ref(model)?;
            if value.pointer("/data/model") != Some(&model) {
                self.request_json(
                    "POST",
                    &format!("/api/session/{session_id}/model"),
                    Some(&json!({"model": model})),
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn rename_session(&self, session_id: &str, title: &str) -> Result<()> {
        self.request_json(
            "PATCH",
            &format!("/api/session/{session_id}"),
            Some(&json!({"title":title})),
        )?;
        Ok(())
    }

    pub(crate) fn abort_session(&self, session_id: &str) -> Result<()> {
        let value = self.request_json(
            "POST",
            &format!("/api/session/{session_id}/interrupt"),
            None,
        )?;
        if value.get("interrupted").and_then(Value::as_bool).is_none() {
            bail!("OpenCode V2 did not confirm interruption of session `{session_id}`");
        }
        // false is the documented, successful idle no-op.
        Ok(())
    }

    pub(crate) fn prompt(
        &self,
        session_id: &str,
        id: &str,
        text: &str,
        skill: Option<&str>,
        command: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({"text":text,"delivery":"steer"});
        if let Some(skill) = skill {
            body["skills"] = json!([{"id":skill}]);
        }
        let path = if let Some(command) = command {
            body["name"] = command.into();
            format!("/api/session/{session_id}/command")
        } else {
            body["id"] = id.into();
            format!("/api/session/{session_id}/prompt")
        };
        let value = self.request_json("POST", &path, Some(&body))?;
        if command.is_some() {
            return Ok(String::new());
        }
        let admitted = value
            .pointer("/data/id")
            .and_then(Value::as_str)
            .context("OpenCode V2 prompt response omitted its durable admission ID")?;
        if admitted != id {
            bail!("OpenCode V2 admitted unexpected input `{admitted}` instead of `{id}`");
        }
        Ok(admitted.to_owned())
    }

    pub(crate) fn admit_compaction(&self, session_id: &str, id: &str) -> Result<()> {
        let value = self.request_json(
            "POST",
            &format!("/api/session/{session_id}/compact"),
            Some(&json!({"id":id,"delivery":"steer"})),
        )?;
        if value.pointer("/data/id").and_then(Value::as_str) != Some(id) {
            bail!("OpenCode V2 compaction response omitted the requested admission ID");
        }
        Ok(())
    }

    pub(crate) fn owns_session(&self, session_id: &str, root: &str) -> Result<bool> {
        let mut current = session_id.to_owned();
        let mut visited = std::collections::BTreeSet::new();
        while visited.insert(current.clone()) {
            if current == root {
                return Ok(true);
            }
            let info = self.request_json("GET", &format!("/api/session/{current}"), None)?;
            let Some(parent) = info.pointer("/data/parentID").and_then(Value::as_str) else {
                return Ok(false);
            };
            current = parent.to_owned();
        }
        Ok(false)
    }

    pub(crate) fn approve_permission(&self, session_id: &str, request_id: &str) -> Result<()> {
        self.request_json(
            "POST",
            &format!("/api/session/{session_id}/permission/{request_id}/reply"),
            Some(&json!({"decision":"once"})),
        )?;
        Ok(())
    }

    pub(crate) fn context_report(&self, session_id: &str) -> Result<String> {
        let session = self.request_json("GET", &format!("/api/session/{session_id}"), None)?;
        let messages = self.request_json(
            "GET",
            &format!("/api/session/{session_id}/message?limit=20&order=desc"),
            None,
        )?;
        let latest = messages
            .get("data")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find(|m| {
                    m.get("type").and_then(Value::as_str) == Some("assistant")
                        && m.get("tokens").is_some()
                })
            });
        let latest_context = latest.and_then(|m| {
            Some(
                m.pointer("/tokens/input")?.as_u64()?
                    + m.pointer("/tokens/cache/read")?.as_u64()?
                    + m.pointer("/tokens/cache/write")?.as_u64()?,
            )
        });
        let latest = latest_context.map_or_else(
            || "unavailable".into(),
            |tokens| {
                let capacity =
                    self.context_window
                        .filter(|w| *w > 0)
                        .map_or_else(String::new, |window| {
                            format!(
                                " of {window} configured ({:.1}%)",
                                tokens as f64 * 100.0 / window as f64
                            )
                        });
                format!("approximately {tokens} context tokens{capacity}")
            },
        );
        let totals = session
            .pointer("/data/tokens")
            .context("OpenCode V2 session omitted cumulative usage")?;
        Ok(format!(
            "OpenCode context for `{session_id}`: latest reported input: {latest}. Session cumulative tokens: {totals}. Latest-step usage estimates context; it is not a measured remaining output budget."
        ))
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        let request = self
            .agent
            .request(method, &format!("{}{path}", self.base_url));
        match &self.authorization {
            Some(value) => request.set("Authorization", value),
            None => request,
        }
    }

    fn request_json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let request = self.request(method, path).timeout(CONTROL_REQUEST_TIMEOUT);
        let response = match body {
            Some(body) => request.send_json(body),
            None => request.call(),
        }
        .map_err(|error| api_error(method, path, error))?;
        if response.status() == 204 {
            return Ok(Value::Null);
        }
        response
            .into_json()
            .with_context(|| format!("OpenCode V2 {method} {path} returned invalid JSON"))
    }

    pub(crate) fn subscribe(&self) -> Result<()> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| anyhow::anyhow!("OpenCode event subscription lock was poisoned"))?;
        if events.is_none() {
            let response = self
                .request("GET", "/api/event")
                .call()
                .map_err(|error| api_error("GET", "/api/event", error))?;
            let (sender, receiver) = mpsc::sync_channel(256);
            thread::spawn(move || {
                let result = read_events(response.into_reader(), |event| {
                    sender.send(Ok(event)).is_ok()
                });
                let error = result.err().unwrap_or_else(|| {
                    anyhow::anyhow!(
                        "OpenCode V2 event stream disconnected; completion is unconfirmed"
                    )
                });
                let _ = sender.send(Err(error));
            });
            let first = receiver
                .recv_timeout(SERVER_START_TIMEOUT)
                .context("OpenCode V2 event stream did not confirm subscription")??;
            if first.get("type").and_then(Value::as_str) != Some("server.connected") {
                bail!("OpenCode V2 event stream omitted server.connected before admission");
            }
            *events = Some(receiver);
        }
        // Discard events from completed turns before dispatch, never after it.
        while let Some(event) = events.as_ref().unwrap().try_iter().next() {
            event?;
        }
        Ok(())
    }

    pub(crate) fn next_event(&self, timeout: Duration) -> Result<Option<Value>> {
        let events = self
            .events
            .lock()
            .map_err(|_| anyhow::anyhow!("OpenCode event subscription lock was poisoned"))?;
        let receiver = events
            .as_ref()
            .context("OpenCode events must be subscribed before admission")?;
        match receiver.recv_timeout(timeout) {
            Ok(event) => event.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("OpenCode V2 event reader stopped; completion is unconfirmed")
            }
        }
    }
}

fn model_ref(model: &str) -> Result<Value> {
    let (provider, model) = model
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .with_context(|| {
            format!("OpenCode model `{model}` must use provider/model[#variant] form")
        })?;
    let (id, variant) = model
        .split_once('#')
        .map_or((model, None), |(id, variant)| (id, Some(variant)));
    let mut value = json!({"providerID":provider,"id":id});
    if let Some(variant) = variant {
        value["variant"] = variant.into();
    }
    Ok(value)
}

fn read_events(reader: impl Read, mut emit: impl FnMut(Value) -> bool) -> Result<()> {
    let mut reader = BufReader::new(reader);
    let mut data = String::new();
    loop {
        let mut line = String::new();
        let count = reader
            .by_ref()
            .take(MAX_EVENT_BYTES as u64 + 1)
            .read_line(&mut line)?;
        if count == 0 {
            break;
        }
        if data.len() + count > MAX_EVENT_BYTES {
            bail!("OpenCode V2 SSE frame exceeds 1 MiB");
        }
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            if !data.is_empty() {
                let event =
                    serde_json::from_str(&data).context("OpenCode V2 sent malformed event JSON")?;
                if !emit(event) {
                    return Ok(());
                }
                data.clear();
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start_matches(' ').trim_end_matches(['\r', '\n']));
        }
    }
    if !data.is_empty() {
        bail!("OpenCode V2 event stream ended mid-frame");
    }
    Ok(())
}

fn api_error(method: &str, path: &str, error: ureq::Error) -> anyhow::Error {
    match error {
        ureq::Error::Status(status, response) => {
            let detail = response.into_string().unwrap_or_default();
            anyhow::anyhow!(
                "OpenCode V2 {method} {path} failed with HTTP {status}{}",
                suffix_diagnostic(&detail)
            )
        }
        error => anyhow::anyhow!("OpenCode V2 {method} {path} transport failed: {error}"),
    }
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
pub(crate) mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::Write,
        net::{TcpListener, TcpStream},
        sync::atomic::{AtomicBool, Ordering},
    };

    pub(crate) struct FakeServer {
        pub url: String,
        pub requests: Arc<Mutex<Vec<(String, Value, String)>>>,
        stop: Arc<AtomicBool>,
        event_stream: Arc<Mutex<Option<TcpStream>>>,
        thread: Option<JoinHandle<()>>,
    }

    pub(crate) fn event(kind: &str, mut data: Value) -> Value {
        if data.get("sessionID").is_none() {
            data["sessionID"] = "ses_created".into();
        }
        json!({"id":uuid::Uuid::new_v4().to_string(),"type":kind,"data":data})
    }

    pub(crate) fn turn(text: &str) -> Vec<Value> {
        vec![
            event(
                "session.text.ended",
                json!({"assistantMessageID":"msg_answer","ordinal":0,"text":text}),
            ),
            event(
                "session.step.ended",
                json!({"assistantMessageID":"msg_answer","finish":"stop","cost":0.01,"tokens":{"input":10,"output":3,"reasoning":2,"cache":{"read":4,"write":1}}}),
            ),
            event("session.execution.succeeded", json!({})),
        ]
    }

    impl FakeServer {
        pub(crate) fn start(repo: &Path, turns: Vec<Vec<Value>>) -> Self {
            Self::with_version(repo, turns, "2.2.0")
        }

        fn with_version(repo: &Path, turns: Vec<Vec<Value>>, version: &str) -> Self {
            let version = version.to_owned();
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let event_stream = Arc::new(Mutex::new(None::<TcpStream>));
            let stop = Arc::new(AtomicBool::new(false));
            let captured = requests.clone();
            let stream_slot = event_stream.clone();
            let stopping = stop.clone();
            let repo = repo.to_path_buf();
            let mut turns = VecDeque::from(turns);
            let thread = thread::spawn(move || {
                let mut interrupt_error = false;
                while !stopping.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(value) => value,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => panic!("fake OpenCode accept failed: {e}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let (line, body, headers) = read_request(&mut stream);
                    captured
                        .lock()
                        .unwrap()
                        .push((line.clone(), body.clone(), headers));
                    let method = line.split_whitespace().next().unwrap();
                    let path = line.split_whitespace().nth(1).unwrap();
                    if path.ends_with("/interrupt") && interrupt_error {
                        let body = "cannot confirm interrupt";
                        write!(stream,"HTTP/1.1 503 Service Unavailable\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                        continue;
                    }
                    if path == "/v1/chat/completions" {
                        let model = body["model"].as_str().unwrap_or("fixture");
                        let prompt = body.to_string();
                        let has_tool_result = body["messages"]
                            .as_array()
                            .is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool"));
                        let shell = body["tools"].as_array().and_then(|tools| {
                            tools.iter().find_map(|t| {
                                t.pointer("/function/name")
                                    .and_then(Value::as_str)
                                    .filter(|n| matches!(*n, "bash" | "shell"))
                            })
                        });
                        let tool = shell.filter(|_| {
                            prompt.contains("OPSX_NATIVE_TOOL_PROBE") && !has_tool_result
                        });
                        let content = if prompt.contains("<template>")
                            && prompt.contains("## Objective")
                        {
                            "## Objective\nFinish the isolated fixture.\n\n## Requirements\nNo repository work.\n\n## Work State\nThe previous phase completed.\n\n## Next Move\nContinue the requested phase."
                        } else {
                            "OPSX_STATUS: READY"
                        };
                        if body["stream"].as_bool() == Some(true) {
                            let delta = match tool {
                                Some(name) => {
                                    json!({"role":"assistant","tool_calls":[{"index":0,"id":"fixture_tool_call","type":"function","function":{"name":name,"arguments":json!({"command":"printf 'OPSX_NATIVE_TOOL_PROBE_OK\\n'","description":"Isolated shell compatibility probe"}).to_string()}}]})
                                }
                                None => json!({"role":"assistant","content":content}),
                            };
                            let first = json!({"id":"fixture_completion","object":"chat.completion.chunk","created":1,"model":model,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                            let last = json!({"id":"fixture_completion","object":"chat.completion.chunk","created":1,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":if tool.is_some(){"tool_calls"}else{"stop"}}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}});
                            let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
                            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
                        } else {
                            reply(
                                &mut stream,
                                &json!({"id":"fixture_completion","object":"chat.completion","created":1,"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}),
                            );
                        }
                        continue;
                    }
                    if path == "/api/event" {
                        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
                        send_event(&mut stream, &json!({"type":"server.connected","data":{}}));
                        *stream_slot.lock().unwrap() = Some(stream);
                        continue;
                    }
                    let response = match (method, path) {
                        ("GET", "/api/info") => {
                            json!({"version":version,"pid":1,"urls":[],"paths":{"tmp":"/tmp"}})
                        }
                        ("POST", "/api/session") => {
                            assert_eq!(
                                body["location"]["directory"],
                                repo.to_string_lossy().as_ref()
                            );
                            json!({"data":{"id":"ses_created"}})
                        }
                        ("GET", "/api/session/ses_created") => {
                            json!({"data":{"id":"ses_created","location":{"directory":repo},"tokens":{"input":40,"output":5}}})
                        }
                        ("POST", "/api/session/ses_created/model")
                        | ("PATCH", "/api/session/ses_created")
                        | ("POST", "/api/session/ses_created/permission/ask_1/reply") => {
                            Value::Null
                        }
                        ("POST", "/api/session/ses_created/interrupt") => {
                            json!({"interrupted":false})
                        }
                        ("GET", "/api/session/ses_created/message?limit=20&order=desc") => {
                            json!({"data":[{"type":"assistant","tokens":{"input":30,"output":4,"cache":{"read":15,"write":5}}}],"cursor":{}})
                        }
                        ("POST", "/api/session/ses_created/prompt")
                        | ("POST", "/api/session/ses_created/compact")
                        | ("POST", "/api/session/ses_created/command") => {
                            assert!(
                                body.get("providerID").is_none() && body.get("modelID").is_none()
                            );
                            if path.ends_with("/command") {
                                assert!(body.get("command").is_none());
                                Value::Null
                            } else {
                                json!({"data":{"id":body["id"]}})
                            }
                        }
                        _ => panic!("unexpected V2 request: {line} {body}"),
                    };
                    reply(&mut stream, &response);
                    if method == "POST"
                        && (path.ends_with("/prompt")
                            || path.ends_with("/compact")
                            || path.ends_with("/command"))
                    {
                        let id = body
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("msg_command");
                        let mut slot = stream_slot.lock().unwrap();
                        let writer = slot
                            .as_mut()
                            .expect("subscribe before admitting model work");
                        send_event(writer, &event("session.execution.succeeded", json!({}))); // stale completion
                        send_event(
                            writer,
                            &event("session.inbox.enqueued", json!({"inboxID":id})),
                        );
                        send_event(
                            writer,
                            &event(
                                "session.text.ended",
                                json!({"assistantMessageID":"msg_old","text":"OPSX_STATUS: BLOCKED"}),
                            ),
                        );
                        send_event(
                            writer,
                            &event("session.inbox.delivered", json!({"inboxID":id})),
                        );
                        for mut value in turns.pop_front().expect("unexpected extra model request")
                        {
                            if value.get("_interrupt_error").and_then(Value::as_bool) == Some(true)
                            {
                                interrupt_error = true;
                                value.as_object_mut().unwrap().remove("_interrupt_error");
                            }
                            if let Some(delay) = value.get("_delay_ms").and_then(Value::as_u64) {
                                thread::sleep(Duration::from_millis(delay));
                                value.as_object_mut().unwrap().remove("_delay_ms");
                            }
                            if value.get("_disconnect").and_then(Value::as_bool) == Some(true) {
                                let _ = writer.shutdown(std::net::Shutdown::Both);
                                break;
                            }
                            send_event(writer, &value);
                        }
                    }
                }
            });
            Self {
                url,
                requests,
                stop,
                event_stream,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let result = thread.join();
                if !std::thread::panicking() {
                    result.unwrap();
                }
            }
            self.event_stream.lock().unwrap().take();
        }
    }

    fn send_event(stream: &mut TcpStream, event: &Value) {
        write!(stream, "data: {event}\n\n").unwrap();
        stream.flush().unwrap();
    }

    fn reply(stream: &mut TcpStream, value: &Value) {
        if value.is_null() {
            write!(
                stream,
                "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        } else {
            let body = value.to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
    }

    fn read_request(stream: &mut TcpStream) -> (String, Value, String) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut headers = String::new();
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header == "\r\n" {
                break;
            }
            if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
            headers.push_str(&header);
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (
            line.trim().into(),
            if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            },
            headers,
        )
    }

    #[test]
    fn v2_control_requests_use_wrappers_locations_empty_responses_and_auth() {
        let server = FakeServer::start(Path::new("/repo"), vec![]);
        let client = OpenCodeServerClient::new(
            server.url.clone(),
            PathBuf::from("/repo"),
            None,
            Some(100),
            Some("private-password".into()),
        );
        client.check_version().unwrap();
        assert_eq!(client.create_session(Some("test")).unwrap(), "ses_created");
        client.prepare_session("ses_created").unwrap();
        client.rename_session("ses_created", "renamed").unwrap();
        client.abort_session("ses_created").unwrap();
        assert!(
            client
                .context_report("ses_created")
                .unwrap()
                .contains("50 context tokens of 100 configured (50.0%)")
        );
        let requests = server.requests.lock().unwrap();
        assert!(requests.iter().all(|(_, _, headers)| {
            headers.to_ascii_lowercase().contains(
                &format!(
                    "authorization: basic {}",
                    STANDARD.encode("opencode:private-password")
                )
                .to_ascii_lowercase(),
            )
        }));
        assert!(
            requests
                .iter()
                .all(|(line, _, _)| !line.contains("directory="))
        );
    }

    #[test]
    fn incompatible_versions_stop_before_session_creation() {
        for version in ["1.19.3", "2.1.4", "3.0.0", "unknown"] {
            let server = FakeServer::with_version(Path::new("/repo"), vec![], version);
            let client = OpenCodeServerClient::new(
                server.url.clone(),
                PathBuf::from("/repo"),
                None,
                None,
                None,
            );
            let error = client.check_version().unwrap_err().to_string();
            assert!(error.contains("Upgrade OpenCode") && error.contains(version));
            assert_eq!(server.requests.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn sse_parser_handles_multiline_comments_and_rejects_truncation() {
        let input = ": ping\r\ndata: {\"type\":\"server.connected\",\r\ndata: \"data\":{}}\r\n\r\n";
        let mut events = Vec::new();
        read_events(input.as_bytes(), |v| {
            events.push(v);
            true
        })
        .unwrap();
        assert_eq!(events.len(), 1);
        assert!(read_events(b"data: {".as_slice(), |_| true).is_err());
        assert!(read_events(b"data: nope\n\n".as_slice(), |_| true).is_err());
        assert!(
            read_events(
                format!("data: {}\n\n", "x".repeat(MAX_EVENT_BYTES)).as_bytes(),
                |_| true
            )
            .is_err()
        );
    }

    #[test]
    fn parses_native_model_variants_without_splitting_model_slashes() {
        assert_eq!(
            model_ref("openrouter/qwen/qwen3.6#high").unwrap(),
            json!({"providerID":"openrouter","id":"qwen/qwen3.6","variant":"high"})
        );
        assert!(model_ref("model").is_err());
    }
}
