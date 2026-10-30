//! Runner-observed check receipts. These never replace independent acceptance.
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicI32, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::{
    git::metadata_dir,
    process::{configure_stream_process, kill_stream_process, shell_quote},
    runner_executable::RunnerExecutable,
    ui::{TerminalUi, Ui},
    verification::{Files, changed_paths, snapshot},
};

static INTERRUPTED: AtomicI32 = AtomicI32::new(0);
const MAX_DEPENDENCY_ENTRIES: usize = 4096;
const MAX_DEPENDENCY_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(unix)]
extern "C" fn interrupt(signal: libc::c_int) {
    INTERRUPTED.store(signal, Ordering::Relaxed);
}

fn install_interrupt_handler() {
    #[cfg(unix)]
    // This dedicated helper handles signals long enough to stop its process
    // group and retain an interrupted receipt. The handler only stores an atomic.
    unsafe {
        libc::signal(libc::SIGINT, interrupt as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, interrupt as *const () as libc::sighandler_t);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct History {
    scope: uuid::Uuid,
    pub(crate) runner: Option<RunnerExecutable>,
    #[serde(default)]
    generation: u64,
}

impl Default for History {
    fn default() -> Self {
        Self {
            scope: uuid::Uuid::new_v4(),
            runner: None,
            generation: 0,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Context {
    change: String,
    stage: String,
    repositories: Vec<PathBuf>,
    protected: Vec<PathBuf>,
    #[serde(default)]
    generation: u64,
}

impl History {
    pub(crate) fn ensure<U: Ui>(&mut self, repo: &Path, ui: &U) -> Result<()> {
        RunnerExecutable::ensure(&mut self.runner, repo, ui)
    }

    pub(crate) fn independent_check_failed(&mut self) {
        self.generation += 1;
    }

    pub(crate) fn guidance<U: Ui>(
        &self,
        repo: &Path,
        product: Option<&Path>,
        change: &str,
        stage: &str,
        protected: Vec<PathBuf>,
        ui: &U,
    ) -> Result<String> {
        let directory = directory(repo, self.scope, ui)?;
        let context = Context {
            change: change.to_owned(),
            stage: stage.to_owned(),
            repositories: std::iter::once(repo.to_owned())
                .chain(product.map(Path::to_owned))
                .collect(),
            protected,
            generation: self.generation,
        };
        write_json(&directory.join("context.json"), &context)?;
        let runner = self
            .runner
            .as_ref()
            .context("check helper not initialized")?;
        let example = serde_json::json!({
            "key": "unit-tests", "command": "<actual check command>", "cwd": product.unwrap_or(repo),
            "tools": ["<executable tool>"], "additional_inputs": [], "review_only": [],
            "runtime": "unknown", "runtime_reason": "<runtime and dependency limitations>", "timeout_seconds": 600,
        });
        let mut text = format!(
            "\n\nRECORDED EXECUTION EVIDENCE\nFor checks you need to run, prefer {} check --repo {} --scope {} with one JSON object on stdin (not a product file). Example:\n{example}\n",
            shell_quote(&runner.path.to_string_lossy()),
            shell_quote(&repo.to_string_lossy()),
            self.scope,
        );
        text.push_str("Choose checks by coverage within this stage. Inspect what the required project gate actually runs. Do not run a standalone full suite and then a gate that already includes that suite on unchanged inputs. Use focused tests while developing, then one encompassing gate for final validation in this stage; add checks only for uncovered requirements, changed inputs, failures or an explicit fresh-check policy. Independent Verify still reviews the work and runs required fresh checks in its own stage; an Apply pass does not replace that review or those executions. The runner's final acceptance remains a separate execution even if the same gate passed earlier.\n");
        text.push_str("The helper executes /bin/sh -c, retains actual stdout/stderr and exit status under private Git metadata, and returns a JSON receipt with evidence paths. Keep secrets out of commands and logs. Use a stable key for the same check within this change. It may reuse only the latest successful receipt with identical execution inputs and intact logs; a failure never falls back to an older pass. Do not claim reused execution ran in this stage.\nAll tracked and non-ignored files in both repositories are inputs by default, including shared helpers, tests, build/dependency configuration and governing requirements. List ignored/external dependencies and fixtures in additional_inputs (paths relative to cwd or absolute), and all executable tools in tools. The helper fingerprints executable bytes; this does not discover transitive dependencies, libraries, services or toolchains. Use runtime unknown unless you establish that this complete execution boundary and runtime premises are stable; live services, clocks, native/system qualification or unknown impact require fresh execution (runtime live or unknown). Explain the premise in runtime_reason.\nFor an individual evidence-only file that the command does not consume, review_only may contain {\"path\":\"absolute/file/path\",\"reason\":\"why this command does not consume it\"}. Establish that boundary before declaring it. No directory/glob or blanket documentation exclusions; protected inputs cannot be excluded. These declarations are diagnostic claims for independent review, not coverage or acceptance waivers. An affected/shared input, missing dependency/tool identity, new failing evidence or an explicit fresh-check requirement requires execution; use runtime unknown to force it. Review source conditions, test adequacy and declared boundaries independently. Missing receipts do not fabricate failures or require unrelated checks. The runner's final acceptance commands, order and timeouts are unchanged and always independent of this cache.\n");
        text.push_str("Preserve the check's own exit status; do not hide failures behind a pipeline or trailing echo/tail. If the helper is unavailable or unsuitable for the required interface/sandbox, run the required check normally with existing permissions and report its actual outcome and limitations. No reusable receipt is claimed for that execution.\n");
        text.push_str("Input fingerprinting is skipped when runtime is live/unknown or executable tools are undeclared, since those checks must run fresh. For reusable checks, dependency traversal is bounded to 4096 entries and 64 MiB; exceeding the budget disables reuse and executes the command normally. Declare actual dependencies, not entire mutable build/module caches merely to obtain a receipt. An incomplete fingerprint never certifies unchanged inputs.\n");
        let mut records = match latest_records(&directory, change) {
            Ok(records) => records,
            Err(error) => {
                text.push_str(&format!("Receipt history is incomplete/unreadable ({error:#}); no prior pass can be reused. The helper will execute requested checks afresh.\n"));
                return Ok(text);
            }
        };
        records.sort_by(|a, b| a.key.cmp(&b.key));
        let current = records
            .iter()
            .any(|record| record.inputs.is_some())
            .then(|| repository_snapshots(&context, ui));
        for record in records.iter().take(30) {
            text.push_str(&format!(
                "\nCheck {}: {} in {}; recorded at {} during {}. Receipt: {}. stdout: {}; stderr: {}.\n",
                serde_json::to_string(&record.key)?, record.outcome, record.spec.cwd.display(),
                record.started_at_unix_ms, record.stage, record.path.display(),
                record.stdout.display(), record.stderr.display(),
            ));
            if let Some(source) = &record.reused_from {
                text.push_str(&format!(
                    "This receipt reuses {}; no new execution occurred.\n",
                    source.display()
                ));
            }
            let boundary = serde_json::to_string(&record.spec)?;
            text.push_str("Execution boundary (diagnostic claim): ");
            text.extend(boundary.chars().take(1800));
            if boundary.chars().count() > 1800 {
                text.push_str(" [shortened; read the receipt]");
            }
            text.push('\n');
            if !record.reuse_limitations.is_empty() {
                text.push_str("Reuse limitations: ");
                text.extend(record.reuse_limitations.join("; ").chars().take(1000));
                text.push('\n');
            }
            let mut reasons = file_reasons(
                &context,
                record,
                current.as_ref().and_then(|result| result.as_ref().ok()),
            );
            if record.generation != context.generation {
                reasons.push(
                    "independent acceptance failed or was interrupted after this receipt".into(),
                );
            }
            if reasons.is_empty() {
                text.push_str("Recorded file inputs still match. The helper must also revalidate the invoking environment, tools, dependencies, runtime premise and logs before reuse.\n");
            } else {
                text.push_str(&format!("Reassessment required: {}\n", reasons.join("; ")));
            }
        }
        if records.len() > 30 {
            text.push_str(&format!(
                "[Receipt list shortened; full evidence: {}]\n",
                directory.display()
            ));
        }
        Ok(text)
    }
}

#[derive(Parser)]
#[command(
    name = "opsx-build check",
    about = "Run or conservatively reuse a check; read its JSON description from stdin"
)]
struct Args {
    #[arg(long)]
    repo: PathBuf,
    #[arg(long)]
    scope: uuid::Uuid,
}

/// Kept separate from workflow/config parsing: a check never starts an agent.
pub fn run_cli() -> Result<Option<i32>> {
    let mut args = std::env::args_os();
    let program = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new("check")) {
        return Ok(None);
    }
    let args = Args::parse_from(program.into_iter().chain(args));
    let spec: Spec =
        serde_json::from_reader(std::io::stdin()).context("invalid check JSON on stdin")?;
    install_interrupt_handler();
    let ui = TerminalUi::new(false, false, false);
    let repo = args.repo.canonicalize()?;
    let directory = directory(&repo, args.scope, &ui)?;
    let context: Context = serde_json::from_slice(&fs::read(directory.join("context.json"))?)
        .context("check requires the current run's runner-managed context")?;
    if context.repositories.first() != Some(&repo) {
        bail!("check repository does not match the run context");
    }
    let record = run(&repo, &directory, &context, spec, &ui)?;
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "outcome": record.outcome, "exit_code": record.exit_code,
            "executed": record.reused_from.is_none(), "reused_from": record.reused_from,
            "receipt": record.path, "stdout": record.stdout, "stderr": record.stderr,
            "invalidation_reasons": record.invalidation_reasons,
            "reuse_limitations": record.reuse_limitations,
        }))?
    );
    Ok(Some(if record.outcome == "passed" {
        0
    } else if INTERRUPTED.load(Ordering::Relaxed) != 0 {
        128 + INTERRUPTED.load(Ordering::Relaxed)
    } else if record.outcome == "timed out" {
        124
    } else {
        record.exit_code.filter(|code| *code != 0).unwrap_or(1)
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    key: String,
    command: String,
    cwd: PathBuf,
    tools: Vec<String>,
    #[serde(default)]
    additional_inputs: Vec<PathBuf>,
    #[serde(default)]
    review_only: Vec<ReviewOnly>,
    #[serde(default)]
    runtime: Runtime,
    runtime_reason: String,
    timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Runtime {
    Stable,
    Live,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewOnly {
    path: PathBuf,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Inputs {
    repositories: BTreeMap<PathBuf, Files>,
    additional: Files,
    tools: BTreeMap<String, (PathBuf, String, bool)>,
    environment: String,
    platform: String,
}

#[derive(Serialize, Deserialize)]
struct Record {
    format_version: u32,
    runner_version: String,
    path: PathBuf,
    key: String,
    change: String,
    stage: String,
    started_at_unix_ms: u128,
    sequence: u64,
    generation: u64,
    spec: Spec,
    inputs: Option<Inputs>,
    outcome: String,
    exit_code: Option<i32>,
    elapsed_ms: u128,
    stdout: PathBuf,
    stderr: PathBuf,
    log_hashes: Option<(String, String)>,
    reused_from: Option<PathBuf>,
    invalidation_reasons: Vec<String>,
    reuse_limitations: Vec<String>,
}

fn directory<U: Ui>(repo: &Path, scope: uuid::Uuid, ui: &U) -> Result<PathBuf> {
    let directory = metadata_dir(repo, ui)?
        .join("checks")
        .join(scope.to_string());
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn latest_records(directory: &Path, change: &str) -> Result<Vec<Record>> {
    let mut records: BTreeMap<String, Record> = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join("result.json");
        if !path.exists() {
            bail!("missing check receipt {}", path.display());
        }
        let record: Record = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("unreadable check receipt {}", path.display()))?;
        if record.change != change {
            continue;
        }
        if records
            .get(&record.key)
            .is_none_or(|previous| record.sequence > previous.sequence)
        {
            records.insert(record.key.clone(), record);
        }
    }
    Ok(records.into_values().collect())
}

fn absolute(base: &Path, path: &Path) -> Result<PathBuf> {
    let path = base.join(path);
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path
            .parent()
            .context("input has no parent")?
            .canonicalize()?
            .join(path.file_name().context("input has no filename")?)),
        Err(error) => Err(error.into()),
    }
}

fn validate(repo: &Path, context: &Context, spec: &mut Spec) -> Result<()> {
    if spec.key.trim().is_empty()
        || spec.command.trim().is_empty()
        || spec.timeout_seconds == 0
        || spec.runtime_reason.trim().is_empty()
    {
        bail!("check requires a key, command, positive timeout and runtime_reason");
    }
    spec.cwd = repo
        .join(&spec.cwd)
        .canonicalize()
        .context("check cwd does not exist")?;
    for exclusion in &mut spec.review_only {
        exclusion.path = absolute(&spec.cwd, &exclusion.path)?;
        if exclusion.reason.trim().is_empty() || exclusion.path.is_dir() {
            bail!("review_only requires individual files and a reason for each");
        }
        if !context
            .repositories
            .iter()
            .any(|root| exclusion.path.starts_with(root))
        {
            bail!("review_only file must belong to a run repository");
        }
        if context
            .protected
            .iter()
            .any(|path| absolute(repo, path).ok().as_ref() == Some(&exclusion.path))
        {
            bail!(
                "protected input cannot be excluded: {}",
                exclusion.path.display()
            );
        }
    }
    Ok(())
}

fn run<U: Ui>(
    repo: &Path,
    directory: &Path,
    context: &Context,
    mut spec: Spec,
    ui: &U,
) -> Result<Record> {
    validate(repo, context, &mut spec)?;
    // Serialize the same named check, including delegated callers. Sequence is
    // independent of wall-clock adjustments; a later failure must win.
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(format!("{}.lock", hash_bytes(repo, spec.key.as_bytes())?)))?;
    lock.lock()?;
    let (prior, history_error) = match latest_records(directory, &context.change) {
        Ok(records) => (records.into_iter().find(|r| r.key == spec.key), None),
        Err(error) => (
            None,
            Some(format!("receipt history incomplete/unreadable: {error:#}")),
        ),
    };
    let mut limitations = Vec::new();
    if spec.runtime != Runtime::Stable {
        limitations.push(format!(
            "runtime premise is {:?}: {}",
            spec.runtime, spec.runtime_reason
        ));
    }
    if spec.tools.is_empty() {
        limitations.push("executable tool/dependency boundary was not declared".into());
    }
    // Fingerprints can only establish reuse. Fresh runtime checks still retain
    // their actual command, status and logs without walking external caches.
    let inputs = if limitations.is_empty() {
        match capture(repo, context, &spec, ui) {
            Ok(inputs) => Some(inputs),
            Err(error) => {
                limitations.push(format!("execution inputs unavailable: {error:#}"));
                None
            }
        }
    } else {
        None
    };
    let mut reasons = limitations.clone();
    reasons.extend(history_error);
    if let Some(prior) = &prior {
        if prior.outcome != "passed" {
            reasons.push(format!("latest attempt was {}", prior.outcome));
        }
        if prior.spec != spec {
            reasons.push("command, cwd, timeout or declared execution boundary changed".into());
        }
        if prior.runner_version != env!("CARGO_PKG_VERSION") {
            reasons.push("runner version changed".into());
        }
        if prior.generation != context.generation {
            reasons
                .push("independent acceptance failed or was interrupted after this receipt".into());
        }
        reasons.extend(
            prior
                .reuse_limitations
                .iter()
                .map(|reason| format!("prior result: {reason}")),
        );
        reasons.extend(file_reasons(
            context,
            prior,
            inputs.as_ref().map(|inputs| &inputs.repositories),
        ));
        match (&prior.inputs, &inputs) {
            (Some(before), Some(now)) => {
                if before.additional != now.additional {
                    reasons.push("additional dependency/fixture inputs changed".into());
                }
                if before.tools != now.tools {
                    reasons.push("executable tool identities changed".into());
                }
                if before.environment != now.environment {
                    reasons.push("invoking environment changed".into());
                }
                if before.platform != now.platform {
                    reasons.push("platform identity changed".into());
                }
            }
            _ => reasons.push("complete input identities unavailable".into()),
        }
        if log_hashes(repo, &prior.stdout, &prior.stderr).ok() != prior.log_hashes
            || prior.log_hashes.is_none()
        {
            reasons.push("retained execution logs are missing or changed".into());
        }
        if prior
            .reused_from
            .as_ref()
            .is_some_and(|path| !path.is_file())
        {
            reasons.push("original execution receipt is missing".into());
        }
    } else {
        reasons.push("no prior result for this check in this change/run".into());
    }
    let attempt = directory.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&attempt)?;
    let mut record = Record {
        format_version: 1,
        runner_version: env!("CARGO_PKG_VERSION").into(),
        path: attempt.join("result.json"),
        key: spec.key.clone(),
        change: context.change.clone(),
        stage: context.stage.clone(),
        started_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
        sequence: prior.as_ref().map_or(1, |record| record.sequence + 1),
        generation: context.generation,
        spec,
        inputs,
        outcome: "running".into(),
        exit_code: None,
        elapsed_ms: 0,
        stdout: attempt.join("stdout.log"),
        stderr: attempt.join("stderr.log"),
        log_hashes: None,
        reused_from: None,
        invalidation_reasons: reasons,
        reuse_limitations: limitations,
    };
    if record.invalidation_reasons.is_empty() {
        let prior = prior.unwrap();
        record.outcome = "passed".into();
        record.exit_code = prior.exit_code;
        record.stdout = prior.stdout;
        record.stderr = prior.stderr;
        record.log_hashes = prior.log_hashes;
        record.reused_from = Some(prior.reused_from.unwrap_or(prior.path));
        write_json(&record.path, &record)?;
        return Ok(record);
    }
    write_json(&record.path, &record)?;
    execute(&mut record)?;
    if let Some(before) = &record.inputs
        && !capture(repo, context, &record.spec, ui).is_ok_and(|after| *before == after)
    {
        record
            .reuse_limitations
            .push("execution inputs changed during the check or could not be revalidated".into());
    }
    record.log_hashes = log_hashes(repo, &record.stdout, &record.stderr).ok();
    write_json(&record.path, &record)?;
    Ok(record)
}

fn file_reasons(
    context: &Context,
    record: &Record,
    current: Option<&BTreeMap<PathBuf, Files>>,
) -> Vec<String> {
    let Some(before) = &record.inputs else {
        return vec!["no complete recorded file snapshot".into()];
    };
    let Some(current) = current else {
        return vec!["current file identities unavailable".into()];
    };
    if before.repositories.keys().ne(context
        .repositories
        .iter()
        .collect::<std::collections::BTreeSet<_>>())
    {
        return vec!["repository boundary changed".into()];
    }
    let mut reasons = Vec::new();
    for root in &context.repositories {
        let changed = changed_paths(&before.repositories[root], &current[root])
            .into_iter()
            .filter(|path| {
                !record
                    .spec
                    .review_only
                    .iter()
                    .any(|excluded| excluded.path == root.join(path))
            })
            .collect::<Vec<_>>();
        if !changed.is_empty() {
            reasons.push(format!(
                "repository inputs changed in {}: {}{}",
                root.display(),
                changed
                    .iter()
                    .take(12)
                    .map(|path| format!("{path:?}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                if changed.len() > 12 {
                    " [list shortened]"
                } else {
                    ""
                }
            ));
        }
    }
    reasons
}

fn repository_snapshots<U: Ui>(context: &Context, ui: &U) -> Result<BTreeMap<PathBuf, Files>> {
    context
        .repositories
        .iter()
        .map(|root| Ok((root.clone(), snapshot(root, ui)?)))
        .collect()
}

fn capture<U: Ui>(repo: &Path, context: &Context, spec: &Spec, ui: &U) -> Result<Inputs> {
    let repositories = repository_snapshots(context, ui)?;
    let mut additional = Files::new();
    let mut entries = 0;
    let mut bytes = 0;
    for path in &context.protected {
        collect_files(
            &absolute(repo, path)?,
            &mut additional,
            &mut entries,
            &mut bytes,
        )?;
    }
    for path in &spec.additional_inputs {
        collect_files(
            &absolute(&spec.cwd, path)?,
            &mut additional,
            &mut entries,
            &mut bytes,
        )?;
    }
    // Bound traversal before reading any dependency contents, then hash in
    // batches rather than spawning one Git process per cache entry.
    let paths = additional
        .iter()
        .filter(|(_, (hash, _))| hash.is_empty())
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    for batch in paths.chunks(128) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(["hash-object", "--no-filters", "--"])
            .args(batch)
            .output()?;
        let output_text = String::from_utf8(output.stdout)?;
        let hashes = output_text.lines().collect::<Vec<_>>();
        if !output.status.success()
            || hashes.len() != batch.len()
            || hashes
                .iter()
                .any(|hash| hash.len() < 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            bail!("incomplete dependency fingerprints");
        }
        for (path, hash) in batch.iter().zip(hashes) {
            additional.get_mut(path).unwrap().0 = hash.to_owned();
        }
    }
    let mut tools = BTreeMap::new();
    for tool in std::iter::once("/bin/sh").chain(spec.tools.iter().map(String::as_str)) {
        let path = resolve_tool(&spec.cwd, tool)?;
        if !executable(&path)? {
            bail!("declared tool is not executable: {}", path.display());
        }
        tools.insert(
            tool.to_owned(),
            (path.clone(), fingerprint(repo, &path)?, true),
        );
    }
    // Hash values, never persist inherited environment contents (which may be secrets).
    let mut environment = std::env::vars_os()
        .map(|(key, value)| {
            (
                key.as_encoded_bytes().to_vec(),
                value.as_encoded_bytes().to_vec(),
            )
        })
        .collect::<Vec<_>>();
    environment.sort();
    Ok(Inputs {
        repositories,
        additional,
        tools,
        environment: hash_bytes(repo, &serde_json::to_vec(&environment)?)?,
        platform: platform_identity()?,
    })
}

fn collect_files(
    path: &Path,
    files: &mut Files,
    entries: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    *entries += 1;
    if *entries > MAX_DEPENDENCY_ENTRIES {
        bail!("dependency fingerprint budget exceeded (4096 entries)");
    }
    let name = path.to_str().context("dependency path is not UTF-8")?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        bail!("symlink dependency {} requires inspection", path.display());
    }
    if metadata.is_dir() {
        files.insert(name.to_owned(), ("directory".into(), false));
        for entry in fs::read_dir(path)? {
            collect_files(&entry?.path(), files, entries, bytes)?;
        }
    } else if metadata.is_file() {
        *bytes = bytes.saturating_add(metadata.len());
        if *bytes > MAX_DEPENDENCY_BYTES {
            bail!("dependency fingerprint budget exceeded (64 MiB)");
        }
        files.insert(name.to_owned(), (String::new(), executable(path)?));
    } else {
        bail!("non-file dependency {} requires inspection", path.display());
    }
    Ok(())
}

fn resolve_tool(cwd: &Path, tool: &str) -> Result<PathBuf> {
    if Path::new(tool).components().count() > 1 {
        return Ok(cwd.join(tool).canonicalize()?);
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let path = cwd.join(directory).join(tool);
        if path.is_file() && executable(&path)? {
            return Ok(path.canonicalize()?);
        }
    }
    bail!("cannot resolve executable tool {tool:?}")
}

fn executable(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        Ok(path.is_file())
    }
}

fn platform_identity() -> Result<String> {
    let output = Command::new("uname").args(["-snrm"]).output()?;
    if !output.status.success() {
        bail!("native host/kernel identity unavailable");
    }
    Ok(format!(
        "{}-{}: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        String::from_utf8(output.stdout)?.trim()
    ))
}

fn fingerprint(repo: &Path, path: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["hash-object", "--no-filters", "--"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        bail!("cannot fingerprint {}", path.display());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn hash_bytes(repo: &Path, bytes: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child.stdin.take().unwrap().write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("could not fingerprint execution environment");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn log_hashes(repo: &Path, stdout: &Path, stderr: &Path) -> Result<(String, String)> {
    Ok((fingerprint(repo, stdout)?, fingerprint(repo, stderr)?))
}

fn execute(record: &mut Record) -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", &record.spec.command])
        .current_dir(&record.spec.cwd)
        .stdin(Stdio::null())
        .stdout(File::create(&record.stdout)?)
        .stderr(File::create(&record.stderr)?);
    configure_stream_process(&mut command);
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            record.outcome = format!("launch failed: {error}");
            return Ok(());
        }
    };
    let result = loop {
        if INTERRUPTED.load(Ordering::Relaxed) != 0 {
            record.outcome = "interrupted".into();
            break Ok(());
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                record.exit_code = status.code();
                record.outcome = if status.success() { "passed" } else { "failed" }.into();
                break Ok(());
            }
            Err(error) => {
                record.outcome = "interrupted".into();
                break Err(error);
            }
            Ok(None) if started.elapsed() >= Duration::from_secs(record.spec.timeout_seconds) => {
                record.outcome = "timed out".into();
                break Ok(());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
        }
    };
    let cleanup = kill_stream_process(&mut child);
    let reaped = child.wait();
    record.elapsed_ms = started.elapsed().as_millis();
    write_json(&record.path, record)?;
    result?;
    cleanup.context("stopping check process group")?;
    reaped.context("reaping check process")?;
    Ok(())
}
