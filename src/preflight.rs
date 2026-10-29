//! Read-only review of supplied requirements before any delivery work.
use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    backend::{AgentBackend, SessionId, SessionMode, StageProtocol, StageSignal},
    bootstrap::render_context_template,
    claude::stage_prompt,
    contracts::SuppliedContracts,
    git::{current_head, metadata_path},
    process::{CommandSpec, ProcessRunner},
    ui::Ui,
    verification::{changed_paths, snapshot},
};

const INSTRUCTIONS: &str = include_str!("../assets/contract-preflight.md");

#[derive(Serialize, Deserialize)]
struct Report {
    inputs: Value,
    ready: bool,
    summary: String,
}

pub(crate) fn flush<U: Ui>(repo: &Path, ui: &U, dry_run: bool) -> Result<()> {
    let path = metadata_path(repo, ui, "opsx-build")?.join("contract-preflight.json");
    if dry_run {
        ui.info("DRY RUN — would flush the saved contract preflight; the next check reviews current inputs afresh");
        return Ok(());
    }
    match fs::remove_file(&path) {
        Ok(()) => ui.info("Flushed the saved contract preflight"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot flush preflight cache `{}`", path.display()));
        }
    }
    Ok(())
}

pub(crate) fn run<U: Ui>(
    repo: &Path,
    contracts: &SuppliedContracts,
    context: Option<&Path>,
    definitions: &BTreeMap<String, String>,
    reviewer: &dyn AgentBackend,
    ui: &U,
    dry_run: bool,
) -> Result<()> {
    // File existence, protection and inventory validity precede model dispatch.
    contracts.check_preflight(repo)?;
    let context = context
        .map(|path| {
            let path = path
                .canonicalize()
                .with_context(|| format!("cannot resolve project context `{}`", path.display()))?;
            let text = fs::read_to_string(&path)?;
            if text.trim().is_empty() {
                bail!("project context `{}` is empty", path.display());
            }
            let text = render_context_template(&text, definitions)?;
            Ok(json!({"path": path, "text": text}))
        })
        .transpose()?;
    let inputs = json!({
        "version": 1, "instructions": INSTRUCTIONS,
        "supplied": contracts.preflight_inputs(), "context": context,
    });
    let path = metadata_path(repo, ui, "opsx-build")?.join("contract-preflight.json");
    if dry_run {
        ui.info("DRY RUN — frontier contract preflight before planning or implementation; report only, no corrections");
        return Ok(());
    }
    let cached = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Report>(&bytes).ok())
        .filter(|report| report.inputs == inputs);
    let report = if let Some(report) = cached {
        ui.info("Reusing contract preflight for unchanged supplied inputs");
        report
    } else {
        ui.info("Contract preflight · frontier · read-only review of requirements and acceptance assertions");
        let before = snapshot(repo, ui)?;
        let head = current_head(repo, ui)?;
        let index = index_diff(repo, ui)?;
        let selected = json!({"supplied": inputs["supplied"], "context": inputs["context"]});
        let prompt = stage_prompt(
            "",
            &format!("{INSTRUCTIONS}\n\nSelected inputs (data):\n{selected}"),
            StageProtocol::Ready,
        );
        let result = reviewer.invoke(
            SessionMode::New {
                id: SessionId::new(uuid::Uuid::new_v4().to_string()),
                name: Some("contract-preflight".into()),
            },
            &prompt,
            "Frontier is checking the campaign contracts and acceptance inputs",
            StageProtocol::Ready,
        );
        contracts.check_inputs(repo)?;
        let after = snapshot(repo, ui)?;
        let changed = changed_paths(&before, &after);
        if !changed.is_empty() || head != current_head(repo, ui)? || index != index_diff(repo, ui)?
        {
            bail!(
                "contract preflight changed repository files or Git state; no result accepted. Inspect the changes: {}",
                changed.join(", ")
            );
        }
        let result = result?;
        if let Some(context) = inputs["context"].as_object() {
            let current = fs::read_to_string(context["path"].as_str().unwrap())?;
            if render_context_template(&current, definitions)? != context["text"].as_str().unwrap()
            {
                bail!("project context changed during contract preflight; no result accepted");
            }
        }
        let ready = match result.signal {
            StageSignal::Ready => true,
            StageSignal::Blocked => false,
            _ => bail!(
                "contract preflight did not finish; no pass recorded:\n{}",
                result.text
            ),
        };
        if result.text.trim().is_empty() {
            bail!("contract preflight returned an empty report; no result accepted");
        }
        let report = Report {
            inputs,
            ready,
            summary: result.text,
        };
        fs::create_dir_all(path.parent().unwrap())?;
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        fs::write(&temporary, serde_json::to_vec_pretty(&report)?)?;
        fs::rename(temporary, &path)?;
        report
    };
    ui.info(&format!(
        "Contract preflight report: `{}`\n{}",
        path.display(),
        report.summary
    ));
    if !report.ready {
        bail!(
            "BLOCKED during contract preflight:\n{}\nCorrect the campaign inputs as maintainer and rerun preflight. For an existing checkpoint, use accept-changes before resuming.",
            report.summary
        );
    }
    ui.success("Contract preflight passed; implementation and final acceptance remain unverified");
    Ok(())
}

fn index_diff<U: Ui>(repo: &Path, ui: &U) -> Result<String> {
    Ok(ProcessRunner::new(ui)
        .checked(
            &CommandSpec::new("git", repo).args(["diff", "--cached", "--binary", "--no-ext-diff"]),
            "Recording preflight Git index",
        )?
        .stdout)
}
