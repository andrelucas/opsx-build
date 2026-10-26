//! Request comparisons and failure evidence; never a replay or an acceptance decision.
use std::{
    collections::hash_map::DefaultHasher,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::Result;
use serde_json::{Value, json};

use crate::{codex::CodexLauncher, ui::Ui, usage::UsageRecord};

pub(crate) struct Capture {
    request: Value,
    events: Vec<Value>,
    secrets: Vec<String>,
    result: Value,
    started: Instant,
}

impl Capture {
    pub(crate) fn new(launcher: &CodexLauncher, protocol: &str) -> Self {
        let mut secrets: Vec<_> = launcher
            .environment
            .iter()
            .filter(|v| v.redacted)
            .map(|v| v.value.clone())
            .chain(
                std::env::vars()
                    .filter(|(k, _)| sensitive(k))
                    .map(|(_, v)| v),
            )
            .filter(|v| v.len() >= 4)
            .collect();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        let rust_log = launcher.diagnostic_log_filter();
        let overrides: serde_json::Map<_, _> = launcher
            .prefix_args
            .iter()
            .filter_map(|arg| {
                let (key, value) = arg
                    .strip_prefix("--config=")
                    .unwrap_or(arg)
                    .split_once('=')?;
                matches!(
                    key,
                    "model" | "model_provider" | "model_reasoning_effort" | "service_tier"
                )
                .then(|| (key.to_owned(), json!(value)))
            })
            .collect();
        Self {
            request: json!({
                "protocol": protocol, "structured_output": launcher.structured_output,
                "launch_overrides": overrides, "context_window_override": launcher.context_window,
                "auto_compact_window_override": launcher.auto_compact_window,
                "max_output_tokens_override": launcher.max_output_tokens,
                "permission_profile": launcher.permission_profile, "harness_sandbox":launcher.harness_sandbox,
                "rust_log_filter":rust_log,
            }),
            events: Vec::new(),
            secrets,
            result: Value::Null,
            started: Instant::now(),
        }
    }

    pub(crate) fn request(&mut self, input: &Value, schema: Option<&Value>) {
        self.request["method"] = json!("turn/start");
        self.request["input"] = json!(
            input
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    let mut descriptor = json!({"type": item["type"]});
                    if let Some(text) = item["text"].as_str() {
                        descriptor["text"] = fingerprint(text.as_bytes());
                    }
                    if item["type"] == "skill" {
                        descriptor["name"] = item["name"].clone();
                        descriptor["path"] = item["path"].clone();
                        descriptor["file"] = item["path"]
                            .as_str()
                            .and_then(|path| fs::read(path).ok())
                            .map(|bytes| fingerprint(&bytes))
                            .unwrap_or(Value::Null);
                    }
                    descriptor
                })
                .collect::<Vec<_>>()
        );
        self.request["output_schema"] = schema
            .map(|v| {
                let mut description = fingerprint(v.to_string().as_bytes());
                description["value"] = v.clone();
                description
            })
            .unwrap_or(Value::Null);
    }

    pub(crate) fn event(&mut self, event: &Value) {
        let params = &event["params"];
        let mut value = match event["method"].as_str() {
            Some("error") => {
                json!({"method":"error", "error":params["error"], "willRetry":params["willRetry"]})
            }
            Some("turn/completed") => {
                json!({"method":"turn/completed", "status":params["turn"]["status"], "error":params["turn"]["error"]})
            }
            Some("thread/tokenUsage/updated") => {
                json!({"method":"thread/tokenUsage/updated", "tokenUsage":params["tokenUsage"]})
            }
            Some("model/rerouted") => {
                json!({"method":"model/rerouted", "fromModel":params["fromModel"], "toModel":params["toModel"], "reason":params["reason"]})
            }
            _ => return,
        };
        value["reported_thread_id"] = params["threadId"].clone();
        value["reported_turn_id"] = params
            .get("turnId")
            .unwrap_or(&params["turn"]["id"])
            .clone();
        value["observed_elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        if self.events.len() == 16 {
            self.events.remove(0);
        }
        self.events.push(value);
    }

    pub(crate) fn turn(&mut self, id: &str) {
        self.request["turn_id"] = json!(id);
    }

    pub(crate) fn finish(&mut self, outcome: &str, failure: Option<Value>, server: Value) {
        self.result = json!({"outcome":outcome,"failure":failure,"server":server,
            "elapsed_ms":self.started.elapsed().as_millis() as u64});
    }

    pub(crate) fn save<U: Ui>(&self, repo: &Path, record: &UsageRecord, ui: &U) -> Result<PathBuf> {
        let directory = if let Some(guard) = ui.operating_policy() {
            guard
                .checkpoint_path()
                .parent()
                .expect("checkpoint has a parent")
                .to_path_buf()
        } else {
            crate::git::metadata_dir(repo, ui)?
        }
        .join("diagnostics/codex");
        let value = json!({
            "format_version":1, "runner_version":env!("CARGO_PKG_VERSION"),
            "runner_request_id":record.id, "invocation_id":record.invocation_id, "attempt":record.attempt,
            "started_at_unix_ms":record.started_at_unix_ms, "context":record.context,
            "connection":record.connection, "requested_model":record.requested_model,
            "reported_models":record.reported_models, "session_id":record.session_id,
            "limitations":[
                "Input and schema fingerprints are grouping identifiers, not cryptographic hashes. Request descriptors omit prompt/skill contents; error and transport excerpts are bounded and credential-redacted.",
                "Server stderr is a process-wide tail; it may include earlier requests and delayed or partial output.",
                "Missing upstream response bodies, request IDs or retry hints remain unknown; HTTP 429 alone does not identify a limit.",
            ],
        });
        let mut value = redact(&value, &self.secrets, &mut 8_000, 0);
        // Separate budgets keep a noisy event stream from hiding the terminal error.
        value["result"] = redact(&self.result, &self.secrets, &mut 24_000, 0);
        value["request"] = redact(&self.request, &self.secrets, &mut 16_000, 0);
        value["events"] = redact(&json!(self.events), &self.secrets, &mut 16_000, 0);
        write_report(&directory, &record.id, &value)
    }
}

fn fingerprint(bytes: &[u8]) -> Value {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    json!({"bytes":bytes.len(), "fingerprint":format!("{:016x}",hasher.finish())})
}

fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "authorization",
        "cookie",
        "apikey",
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "password",
        "secret",
    ]
    .iter()
    .any(|part| key.contains(part))
        || key.ends_with("token")
        || key.ends_with("key")
}

fn redact(value: &Value, secrets: &[String], budget: &mut usize, depth: usize) -> Value {
    if *budget < 16 || depth > 8 {
        return json!("[truncated]");
    }
    *budget -= 16;
    match value {
        Value::String(text) => {
            if let Ok(parsed @ (Value::Object(_) | Value::Array(_))) =
                serde_json::from_str::<Value>(text)
            {
                return json!(redact(&parsed, secrets, budget, depth + 1).to_string());
            }
            let mut text = scrub_credentials(text);
            for secret in secrets {
                text = text.replace(secret, "[redacted]");
            }
            text = text
                .split_inclusive(char::is_whitespace)
                .map(|word| {
                    if ["sk-", "sess-"].iter().any(|prefix| {
                        word.match_indices(prefix)
                            .any(|(i, _)| i == 0 || !word.as_bytes()[i - 1].is_ascii_alphanumeric())
                    }) || (word.contains("eyJ") && word.len() > 30)
                    {
                        "[credential-shaped token redacted] ".to_owned()
                    } else {
                        word.to_owned()
                    }
                })
                .collect();
            let limit = (*budget).min(8192);
            if text.len() > limit {
                let mut end = limit;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("[truncated]");
            }
            *budget = budget.saturating_sub(text.len());
            json!(text)
        }
        Value::Array(values) => {
            let mut result = Vec::new();
            for v in values.iter().take(16) {
                if *budget < 16 {
                    result.push(json!("[truncated]"));
                    break;
                }
                result.push(redact(v, secrets, budget, depth + 1));
            }
            Value::Array(result)
        }
        Value::Object(fields) => {
            let mut result = serde_json::Map::new();
            for (k, v) in fields.iter().take(32) {
                if *budget < 16 {
                    result.insert("[truncated]".to_owned(), json!(true));
                    break;
                }
                let key: String = k.chars().take(128).collect();
                *budget = budget.saturating_sub(key.len());
                result.insert(
                    key,
                    if sensitive(k) {
                        json!("[redacted]")
                    } else {
                        redact(v, secrets, budget, depth + 1)
                    },
                );
            }
            Value::Object(result)
        }
        _ => value.clone(),
    }
}

fn scrub_credentials(text: &str) -> String {
    let mut text = text.to_owned();
    for label in [
        "authorization",
        "cookie",
        "api_key",
        "api-key",
        "access_token",
        "refresh_token",
        "id_token",
        "password",
        "client_secret",
        "bearer",
    ] {
        let mut offset = 0;
        while let Some(found) = text.to_ascii_lowercase()[offset..].find(label) {
            let end = offset + found + label.len();
            let mut start = end;
            while text
                .as_bytes()
                .get(start)
                .is_some_and(|b| b.is_ascii_whitespace() || matches!(b, b'"' | b'\'' | b'\\'))
            {
                start += 1;
            }
            if label != "bearer" {
                if !text
                    .as_bytes()
                    .get(start)
                    .is_some_and(|b| matches!(b, b':' | b'='))
                {
                    offset = end;
                    continue;
                }
                start += 1;
                while text
                    .as_bytes()
                    .get(start)
                    .is_some_and(u8::is_ascii_whitespace)
                {
                    start += 1;
                }
            }
            if start >= text.len() {
                break;
            }
            let mut parsed =
                serde_json::Deserializer::from_str(&text[start..]).into_iter::<String>();
            let quoted = text.as_bytes()[start] == b'"' && matches!(parsed.next(), Some(Ok(_)));
            let length = if quoted {
                parsed.byte_offset()
            } else {
                text[start..]
                    .find(|c: char| {
                        c == '\n'
                            || c == ','
                            || c == '}'
                            || (label != "authorization" && label != "cookie" && c.is_whitespace())
                    })
                    .unwrap_or(text.len() - start)
            };
            if length == 0 {
                offset = end;
                continue;
            }
            let replacement = if quoted {
                "\"[redacted]\""
            } else {
                "[redacted]"
            };
            text.replace_range(start..start + length, replacement);
            offset = start + replacement.len();
        }
    }
    text
}

fn write_report(directory: &Path, id: &str, value: &Value) -> Result<PathBuf> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let path = directory.join(format!("{id}.json"));
    let temporary = directory.join(format!("{id}.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_preserves_error_codes_and_retry_evidence_without_credentials() {
        let value = json!({"codexErrorInfo":{"responseTooManyFailedAttempts":{"httpStatusCode":429}},
            "additionalDetails":{"request_id":"req-123","retry_after":30,"api_key":"private-key"},
            "stderr":"HTTP 429 request=req-123\nAuthorization: Bearer private-key\nmessage secret-value"});
        let value = redact(&value, &["secret-value".to_owned()], &mut 48_000, 0);
        let text = value.to_string();
        assert_eq!(
            value["codexErrorInfo"]["responseTooManyFailedAttempts"]["httpStatusCode"],
            429
        );
        assert_eq!(value["additionalDetails"]["retry_after"], 30);
        assert_eq!(value["additionalDetails"]["request_id"], "req-123");
        assert!(!text.contains("private-key"));
        assert!(!text.contains("secret-value"));
        let headers = scrub_credentials(
            r#"WebSocket response status:429 headers:{"set-cookie":"private-cookie; Path=/", "x-oai-request-id":"req-123", "authorization":"Bearer private-token"}"#,
        );
        assert!(headers.contains("status:429"));
        assert!(headers.contains("req-123"));
        assert!(!headers.contains("private-cookie"));
        assert!(!headers.contains("private-token"));
        assert_eq!(
            redact(&json!("task-owned checks"), &[], &mut 1000, 0),
            json!("task-owned checks")
        );
    }

    #[test]
    fn diagnostic_bounds_are_utf8_safe_and_reports_are_private() {
        let value = redact(&json!({"details":"λ".repeat(100_000)}), &[], &mut 48_000, 0);
        assert!(value.to_string().len() < 9000);
        let root =
            std::env::temp_dir().join(format!("opsx-codex-diagnostic-{}", uuid::Uuid::new_v4()));
        let path = write_report(&root, "request", &value).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            value
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
