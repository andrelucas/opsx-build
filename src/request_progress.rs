//! Conservative observations for Apply/Repair reassessment, not acceptance or labels.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(test)]
mod tests;

pub(crate) const HISTORY_SECONDS: u64 = 900;
pub(crate) const HISTORY_EVENTS: usize = 256;

pub(crate) fn check_command(data: &Value) -> Option<&str> {
    let name = data["name"].as_str()?.to_ascii_lowercase();
    if !matches!(
        name.as_str(),
        "bash" | "commandexecution" | "shell" | "exec_command" | "run_command"
    ) {
        return None;
    }
    let command = data["input"]["command"]
        .as_str()
        .or_else(|| data["input"]["cmd"].as_str())?;
    let words = command
        .split(|c: char| c.is_whitespace() || ";|&".contains(c))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    // Do not bypass the helper's stronger rules, including with global flags.
    if let Some(index) = words
        .iter()
        .position(|w| w.trim_matches(['\'', '"']).rsplit('/').next() == Some("opsx-build"))
        && words[index + 1..]
            .iter()
            .any(|w| w.trim_matches(['\'', '"']) == "check")
    {
        return None;
    }
    let pair = words.windows(2).any(|w| {
        let program = w[0]
            .trim_matches(['\'', '"'])
            .rsplit('/')
            .next()
            .unwrap_or(w[0]);
        matches!(
            (program, w[1]),
            ("go", "test" | "vet" | "build")
                | ("cargo", "test" | "check" | "clippy" | "build")
                | ("npm" | "pnpm" | "yarn", "test" | "check" | "build")
                | ("helm", "test")
        )
    });
    let program = words.iter().any(|word| {
        let word = word.trim_matches(['\'', '"', '(', ')', ',']);
        let name = word.rsplit('/').next().unwrap_or(word).to_ascii_lowercase();
        matches!(
            name.as_str(),
            "pytest"
                | "unittest"
                | "ctest"
                | "make"
                | "ninja"
                | "mvn"
                | "gradle"
                | "tlc"
                | "tlc2.tlc"
                | "tlapm"
                | "apalache-mc"
                | "tla2tools.jar"
                | "tlc.jar"
        ) || ((name.ends_with(".py") || name.ends_with(".sh"))
            && [
                "test",
                "check",
                "verify",
                "proof",
                "calibr",
                "conformance",
                "native",
            ]
            .iter()
            .any(|part| name.contains(part)))
    });
    let cluster_wait = words
        .iter()
        .any(|w| w.rsplit('/').next() == Some("kubectl"))
        && words.iter().any(|w| matches!(*w, "wait" | "rollout"));
    (pair || program || cluster_wait).then_some(command)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Hash regular tracked/non-ignored inputs, not timestamps, HEAD or Git metadata.
/// An unavailable/incomplete observation never masquerades as changed inputs.
fn worktree(roots: &[PathBuf]) -> Option<String> {
    let mut digest = Sha256::new();
    let mut remaining = 64 * 1024 * 1024;
    let mut entries = 0;
    for root in roots {
        digest.update(root.as_os_str().as_encoded_bytes());
        let output = Command::new("git")
            .current_dir(root)
            .args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = std::str::from_utf8(&output.stdout).ok()?;
        for path in text
            .split('\0')
            .filter(|s| !s.is_empty())
            .collect::<BTreeSet<_>>()
        {
            entries += 1;
            if entries > 4096
                || !Path::new(path)
                    .components()
                    .all(|p| matches!(p, Component::Normal(_)))
            {
                return None;
            }
            let path_on_disk = root.join(path);
            let metadata = match fs::symlink_metadata(&path_on_disk) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return None,
            };
            digest.update(path.as_bytes());
            digest.update([0]);
            if metadata.file_type().is_symlink() {
                digest.update(b"symlink");
                digest.update(
                    fs::read_link(&path_on_disk)
                        .ok()?
                        .as_os_str()
                        .as_encoded_bytes(),
                );
                continue;
            }
            if !metadata.is_file() || metadata.len() > remaining {
                return None;
            }
            let mut bytes = Vec::new();
            File::open(&path_on_disk)
                .ok()?
                .take(remaining + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > remaining {
                return None;
            }
            remaining -= bytes.len() as u64;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                digest.update((metadata.permissions().mode() & 0o111).to_le_bytes());
            }
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
    }
    Some(format!("{:x}", digest.finalize()))
}

struct Pending {
    fingerprint: String,
    started: Duration,
    deadline: Duration,
    explicit_deadline: bool,
    eligible: bool,
}

pub(crate) struct History {
    roots: Vec<PathBuf>,
    sampled: Duration,
    inputs: Option<String>,
    seen_inputs: BTreeSet<String>,
    seen_calls: BTreeSet<String>,
    seen_executions: BTreeSet<String>,
    pending: BTreeMap<String, Pending>,
    recent: VecDeque<Value>,
    work: Option<Duration>,
    completed: Option<Duration>,
}

impl History {
    pub(crate) fn new(repo: &Path, directory: Option<&Path>) -> Self {
        let mut roots = vec![repo.to_owned()];
        if let Some(directory) = directory
            && let Ok(bytes) = fs::read(directory.join("context.json"))
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(repositories) = value["repositories"].as_array()
        {
            roots.extend(
                repositories
                    .iter()
                    .filter_map(Value::as_str)
                    .map(PathBuf::from),
            );
        }
        roots.sort();
        roots.dedup();
        let inputs = worktree(&roots);
        Self {
            roots,
            sampled: Duration::ZERO,
            seen_inputs: inputs.iter().cloned().collect(),
            inputs,
            seen_calls: BTreeSet::new(),
            seen_executions: BTreeSet::new(),
            pending: BTreeMap::new(),
            recent: VecDeque::new(),
            work: None,
            completed: None,
        }
    }

    fn record(&mut self, elapsed: Duration, mut value: Value, grace: Duration) {
        value["elapsed_ms"] = json!(elapsed.as_millis());
        self.recent.push_back(value);
        while self.recent.len() > HISTORY_EVENTS
            || self.recent.front().is_some_and(|e| {
                elapsed
                    .as_millis()
                    .saturating_sub(e["elapsed_ms"].as_u64().unwrap_or(0) as u128)
                    > grace.as_millis()
            })
        {
            self.recent.pop_front();
        }
    }

    pub(crate) fn sample(&mut self, elapsed: Duration, grace: Duration, force: bool) {
        if !force && elapsed.saturating_sub(self.sampled) < grace.min(Duration::from_secs(60)) {
            return;
        }
        self.sampled = elapsed;
        self.inputs = worktree(&self.roots);
        let first_observation = self.seen_inputs.is_empty();
        if let Some(inputs) = &self.inputs
            && self.seen_inputs.insert(inputs.clone())
            && !first_observation
        {
            self.work = Some(elapsed);
            self.record(
                elapsed,
                json!({"kind":"worktree_changed","inputs":inputs}),
                grace,
            );
        }
    }

    pub(crate) fn observe(
        &mut self,
        elapsed: Duration,
        grace: Duration,
        kind: &str,
        data: &Value,
    ) -> bool {
        let Some(id) = data["call_id"].as_str() else {
            return false;
        };
        if kind == "tool_call" {
            let Some(command) = check_command(data) else {
                return false;
            };
            if !self.seen_calls.insert(id.to_owned()) {
                return false;
            }
            self.sample(elapsed, grace, true);
            let input = &data["input"];
            // Descriptions and changed wait limits do not make a new execution.
            let fingerprint = hash(
                serde_json::to_string(&json!([
                    command,
                    input.get("cwd").or_else(|| input.get("workdir")),
                    self.inputs
                ]))
                .expect("JSON value")
                .as_bytes(),
            );
            let eligible = self.seen_executions.insert(fingerprint.clone());
            let timeout = input["timeout_ms"]
                .as_u64()
                .or_else(|| {
                    (data["name"]
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("bash")))
                    .then(|| input["timeout"].as_u64())
                    .flatten()
                })
                .map(Duration::from_millis)
                .or_else(|| input["timeout_seconds"].as_u64().map(Duration::from_secs))
                .filter(|duration| !duration.is_zero());
            let deadline = elapsed.saturating_add(timeout.unwrap_or(grace));
            self.record(
                elapsed,
                json!({"kind":"check_started","call_id":id,"fingerprint":fingerprint,
                "new_execution_inputs":eligible,"deadline_elapsed_ms":deadline.as_millis(),
                "explicit_deadline":timeout.is_some()}),
                grace,
            );
            self.pending.insert(
                id.to_owned(),
                Pending {
                    fingerprint,
                    started: elapsed,
                    deadline,
                    explicit_deadline: timeout.is_some(),
                    eligible,
                },
            );
            return true;
        } else if kind == "tool_result" {
            self.seen_calls.insert(id.to_owned());
            let Some(pending) = self.pending.remove(id) else {
                return false;
            };
            // Completion is an observation, not proof that the test passed.
            if pending.eligible {
                self.completed = Some(elapsed);
            }
            self.record(
                elapsed,
                json!({"kind":"check_completed","call_id":id,
                "fingerprint":pending.fingerprint,"new_execution_inputs":pending.eligible,
                "exit_code":data["exit_code"],"reported_failed":data["failed"]}),
                grace,
            );
            return true;
        }
        false
    }

    pub(crate) fn expired(&self, elapsed: Duration) -> bool {
        self.pending
            .values()
            .any(|p| p.explicit_deadline && elapsed >= p.deadline)
    }

    pub(crate) fn next_deadline(&self) -> Option<Duration> {
        self.pending
            .values()
            .filter(|p| p.explicit_deadline)
            .map(|p| p.deadline)
            .min()
    }

    pub(crate) fn allowance(&self, elapsed: Duration, grace: Duration) -> Option<Duration> {
        self.work
            .into_iter()
            .chain(self.completed)
            .map(|time| time.saturating_add(grace))
            .chain(
                self.pending
                    .values()
                    .filter(|p| p.eligible)
                    .map(|p| p.deadline),
            )
            .filter(|deadline| *deadline > elapsed)
            .max()
    }

    pub(crate) fn state(&self, elapsed: Duration, grace: Duration) -> Value {
        json!({"observed_elapsed_ms":elapsed.as_millis(),"recent_history":self.recent.iter().filter(|e|
            elapsed.as_millis().saturating_sub(e["elapsed_ms"].as_u64().unwrap_or(0) as u128) <= grace.as_millis()).collect::<Vec<_>>(),
            "inputs_available":self.inputs.is_some(),
            "pending_checks":self.pending.iter().map(|(id,p)|json!({"call_id":id,
                "state":"awaiting_result; process liveness not independently established",
                "fingerprint":p.fingerprint,"started_elapsed_ms":p.started.as_millis(),
                "deadline_elapsed_ms":p.deadline.as_millis(),"explicit_deadline":p.explicit_deadline,
                "new_execution_inputs":p.eligible})).collect::<Vec<_>>()})
    }
}
