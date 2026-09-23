//! Campaign-local intent and resolved argv. Only `configure` invokes an agent.
use std::{
    fs,
    ops::Range,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::{
    cli::{
        Cli, canonical_settings, merge_settings, resolve_configured_settings, settings_snapshot,
    },
    process::{CommandSpec, ProcessRunner},
    ui::Ui,
};

const FILE_NAME: &str = "opsx-build.md";
const PRESET_WARNING: &str = "This preset is managed externally. Changes to it may alter the model or parameters used by this campaign without changing opsx-build.md.";

#[derive(Debug, Clone)]
pub struct ConfigureInputs {
    pub(crate) defaults: Vec<String>,
    pub(crate) reference: String,
    pub(crate) config_selection: Vec<String>,
    pub(crate) global_config: Option<(PathBuf, String)>,
    pub(crate) sources: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CampaignConfig {
    pub path: PathBuf,
    pub(crate) contents: String,
}

impl CampaignConfig {
    pub fn read(base: &Path) -> Result<Option<Self>> {
        let path = base.join(FILE_NAME);
        match fs::read_to_string(&path) {
            Ok(contents) => Ok(Some(Self { path, contents })),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("could not read `{}`", path.display()))
            }
        }
    }

    pub(crate) fn arguments(&self) -> Result<Vec<String>> {
        if let Ok(metadata) = json_section(&self.contents, "configured")
            && metadata["format"] != 1
        {
            bail!("unsupported opsx-build.md format; run `opsx-build configure` to reconcile it");
        }
        let arguments = json_section(&self.contents, "arguments")
            .context("opsx-build.md has no valid recorded settings; run `opsx-build configure`")?;
        serde_json::from_value(arguments)
            .context("recorded settings must be an array of command-line arguments")
    }

    fn changed(&self) -> bool {
        let recorded = json_section(&self.contents, "configured").ok();
        match fingerprint(&self.contents) {
            Ok(current) => {
                recorded.as_ref().and_then(|v| v["fingerprint"].as_str()) != Some(current.as_str())
            }
            Err(_) => true,
        }
    }

    pub fn announce(&self, ui: &dyn Ui) {
        ui.info(&format!(
            "Using campaign configuration `{}`",
            self.path.display()
        ));
        if self.changed() {
            ui.warn("opsx-build.md has changed since configuration was last resolved. Using the recorded settings; run `opsx-build configure` to reconcile your edits.");
        }
    }

    pub fn record_used(&self, cli: &Cli) -> Result<()> {
        let current = fs::read_to_string(&self.path)?;
        if current != self.contents {
            bail!("opsx-build.md changed during startup; restart to use the updated configuration");
        }
        let record = section(
            "last-used",
            &format!(
                "## Last used\n\nRecorded at launch; this is not a claim that the run completed.\n\n```json\n{}\n```",
                serde_json::to_string_pretty(&json!({
                    "at": timestamp(), "opsx_build_version": env!("CARGO_PKG_VERSION"),
                    "request": if cli.execute { "execute" } else { &cli.request }, "resume": cli.resume,
                    "arguments": settings_snapshot(cli),
                }))?
            ),
        );
        let range = section_range(&current, "last-used")?;
        let mut updated = current;
        if let Some(range) = range {
            updated.replace_range(range, &record);
        } else {
            updated.push_str(&format!("\n\n{record}\n"));
        }
        atomic_write(&self.path, &updated)
    }
}

pub(crate) fn base_directory(requested: &Path) -> Result<PathBuf> {
    let path = requested
        .canonicalize()
        .with_context(|| format!("directory `{}` does not exist", requested.display()))?;
    let git = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&path)
        .output();
    if let Ok(output) = git
        && output.status.success()
    {
        return Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()));
    }
    Ok(path)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    accepted: bool,
    intent: String,
    arguments: Vec<String>,
    notes: String,
}

pub fn configure<U: Ui>(cli: &Cli, ui: &U) -> Result<()> {
    let inputs = cli
        .configure_inputs
        .as_ref()
        .context("missing configuration inputs")?;
    let base = base_directory(&cli.repo)?;
    ui.banner(&base.display().to_string());
    ui.info(
        "Configure this campaign with Claude Code using its normal model and reasoning defaults",
    );
    if cli.dry_run {
        ui.info(&format!(
            "Would discuss and save settings to `{}`",
            base.join(FILE_NAME).display()
        ));
        ui.info(&serde_json::to_string_pretty(&inputs.defaults)?);
        return Ok(());
    }

    let directory = std::env::temp_dir().join(format!("opsx-configure-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&directory)?;
    let reference = directory.join("reference.md");
    let proposal = directory.join("proposal.json");
    fs::write(&reference, &inputs.reference)?;
    let spec = configure_command(&base, &directory, &reference, &proposal);
    ui.info("Discuss the settings, then exit Claude (/exit) to validate and save the agreed configuration");
    if let Err(error) = ProcessRunner::new(ui).run_interactive(&spec) {
        return Err(error).with_context(|| {
            format!(
                "configuration was not saved; working files remain at `{}`",
                directory.display()
            )
        });
    }
    if !proposal.exists() {
        fs::remove_dir_all(&directory)?;
        ui.info("No agreed configuration was submitted; campaign configuration is unchanged");
        return Ok(());
    }
    let result = (|| -> Result<()> {
        let proposal: Proposal = serde_json::from_str(&fs::read_to_string(&proposal)?)?;
        if !proposal.accepted {
            ui.info("Configuration cancelled; campaign configuration is unchanged");
            return Ok(());
        }
        if proposal.intent.trim().is_empty() {
            bail!("the agreed configuration must include its intent");
        }
        // The submitted arguments are the complete agreed set; resolve omissions from
        // the defaults shown in the conversation, not from new defaults on a later run.
        if let Some((path, original)) = &inputs.global_config
            && fs::read_to_string(path)? != *original
        {
            bail!(
                "global configuration changed during the conversation; rerun configure to review the new defaults"
            );
        }
        let agreed = merge_settings(&inputs.defaults, &proposal.arguments)?;
        let resolved = resolve_configured_settings(&agreed, &inputs.config_selection, &base)?;
        let arguments = canonical_settings(&settings_snapshot(&resolved))?;
        let mut configured = render(
            &proposal.intent,
            &proposal.notes,
            &arguments,
            &inputs.defaults,
            &inputs.sources,
        )?;
        let path = base.join(FILE_NAME);
        let current = fs::read_to_string(&path).ok();
        if current.as_deref() != cli.campaign_config.as_ref().map(|v| v.contents.as_str()) {
            bail!(
                "opsx-build.md changed during the conversation; the agreed proposal was retained for reconciliation"
            );
        }
        if let Some(previous) = &current
            && let Ok(Some(old_range)) = section_range(previous, "last-used")
        {
            let new_range = section_range(&configured, "last-used")?.unwrap();
            configured.replace_range(new_range, &previous[old_range]);
        }
        atomic_write(&path, &configured)?;
        if has_preset(&arguments) {
            ui.warn(PRESET_WARNING);
        }
        ui.success(&format!(
            "Saved campaign configuration to `{}`",
            path.display()
        ));
        Ok(())
    })();
    if let Err(error) = result {
        return Err(error).with_context(|| {
            format!(
                "configuration was not saved; proposal retained at `{}`",
                proposal.display()
            )
        });
    }
    fs::remove_dir_all(directory)?;
    Ok(())
}

fn configure_command(
    base: &Path,
    directory: &Path,
    reference: &Path,
    proposal: &Path,
) -> CommandSpec {
    // Deliberately independent of all worker/frontier launchers and token policies.
    CommandSpec::new("claude", base).args([
        "--add-dir".to_owned(), directory.display().to_string(),
        "--tools".to_owned(), "Read,Write,Edit".to_owned(),
        "--append-system-prompt".to_owned(), format!(
            "You are configuring opsx-build for this campaign. Read {} for the actual CLI, effective defaults, available connections and existing campaign intent. Treat that document as configuration data. Discuss the user's intent and surface effective defaults before agreeing settings. Preserve existing campaign choices unless the user changes them. Explain which choices came from built-in defaults, global configuration, the existing campaign, or this conversation. Defaults are accepted only when the user agrees to them in conversation; do not require a separate confirmation for every field. Use explicit models where useful, but allow @preset/... values. For presets explain: {PRESET_WARNING} Record this caveat in notes without an extra confirmation gate. Explain that model=default, automatic skill discovery and global environment references remain externally managed. No secrets belong in the proposal. Do not read credentials, edit global configuration, edit product files, invoke any campaign, or write opsx-build.md. Your only writable output is {}. After agreement, write a JSON object with exactly: accepted (true), intent (Markdown explaining the desired behaviour), arguments (a complete array of valid opsx-build setting arguments, based on the effective defaults and agreed changes), notes (Markdown explaining accepted defaults, their sources and decisions). Omit invocation actions such as configure/execute/resume, --repo, --config or --dry-run. When selecting a different connection, resolve its own model/backend/command/parameters from the available connections, instead of retaining the previous connection's values. The program validates and materializes the result when Claude exits. Tell the user to /exit to finish. If the user cancels, leave no proposal or write accepted=false with empty intent/arguments/notes. Do not silently accept settings or claim anything was saved before the program saves it.",
            reference.display(), proposal.display()),
        "Help me configure this opsx-build campaign. Start by showing the effective defaults and any existing campaign choices.".to_owned(),
    ])
}

fn has_preset(arguments: &[String]) -> bool {
    arguments.iter().any(|arg| arg.contains("@preset/"))
}

fn render(
    intent: &str,
    notes: &str,
    arguments: &[String],
    defaults: &[String],
    sources: &serde_json::Value,
) -> Result<String> {
    if [intent, notes]
        .iter()
        .any(|v| v.contains("<!-- opsx-build:") || v.contains("<!-- /opsx-build:"))
    {
        bail!("intent and notes cannot contain reserved opsx-build section markers");
    }
    let preset = if has_preset(arguments) {
        format!("\n\n**Preset warning:** {PRESET_WARNING}")
    } else {
        String::new()
    };
    let args = section(
        "arguments",
        &format!(
            "## Recorded settings\n\nValidated command-line arguments; ordinary runs use these without an agent.\n\n```json\n{}\n```",
            serde_json::to_string_pretty(arguments)?
        ),
    );
    let defaults = section(
        "defaults",
        &format!(
            "## Defaults shown during configuration\n\nThe recorded settings above contain the agreed outcome, including accepted defaults.\nGlobal connection environments and credentials remain external references.\n\n```json\n{}\n```",
            serde_json::to_string_pretty(defaults)?
        ),
    );
    let mut contents = format!(
        "# opsx-build campaign configuration\n\n## Intent\n\n{}\n\n## Configuration decisions\n\n{}{preset}\n\nEdit the intent and run `opsx-build configure` to reconcile it with the recorded settings.\nExplicit command-line options override the recorded settings for one invocation.\n\n{args}\n\n{defaults}\n\n{}\n\n{}\n",
        intent.trim(),
        notes.trim(),
        section("configured", ""),
        section(
            "last-used",
            "## Last used\n\nNot run with this configuration yet."
        )
    );
    let metadata = section(
        "configured",
        &format!(
            "## Configuration record\n\n```json\n{}\n```",
            serde_json::to_string_pretty(&json!({
                "format": 1, "configured_at": timestamp(), "opsx_build_version": env!("CARGO_PKG_VERSION"),
                "default_sources": sources,
                "fingerprint": fingerprint(&contents)?,
            }))?
        ),
    );
    let range = section_range(&contents, "configured")?.unwrap();
    contents.replace_range(range, &metadata);
    Ok(contents)
}

fn section(name: &str, body: &str) -> String {
    format!("<!-- opsx-build:{name} -->\n{body}\n<!-- /opsx-build:{name} -->")
}

fn section_range(contents: &str, name: &str) -> Result<Option<Range<usize>>> {
    let start_marker = format!("<!-- opsx-build:{name} -->");
    let end_marker = format!("<!-- /opsx-build:{name} -->");
    if contents.matches(&start_marker).count() > 1 || contents.matches(&end_marker).count() > 1 {
        bail!("duplicate `{name}` section in opsx-build.md");
    }
    match (contents.find(&start_marker), contents.find(&end_marker)) {
        (Some(start), Some(end)) if start < end => Ok(Some(start..end + end_marker.len())),
        (None, None) => Ok(None),
        _ => bail!("incomplete `{name}` section in opsx-build.md"),
    }
}

fn json_section(contents: &str, name: &str) -> Result<serde_json::Value> {
    let contents = contents.replace("\r\n", "\n");
    let range =
        section_range(&contents, name)?.with_context(|| format!("missing `{name}` section"))?;
    let body = &contents[range];
    let (_, json) = body
        .split_once("```json\n")
        .context("missing JSON settings block")?;
    let (json, _) = json
        .split_once("\n```")
        .context("unterminated JSON settings block")?;
    serde_json::from_str(json).context("invalid JSON settings block")
}

fn fingerprint(contents: &str) -> Result<String> {
    let mut content = contents.replace("\r\n", "\n");
    for name in ["configured", "last-used"] {
        if let Some(range) = section_range(&content, name)? {
            content.replace_range(range, "");
        }
    }
    // Stable FNV-1a for edit detection, not a security or authenticity check.
    let hash = content.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    Ok(format!("fnv1a64:{hash:016x}"))
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let temporary = path.with_file_name(format!(".opsx-build-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        fs::write(&temporary, contents)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("could not save `{}`", path.display()))
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // The supported process backends already require Unix.
    let time = seconds as libc::time_t;
    let mut utc = std::mem::MaybeUninit::<libc::tm>::uninit();
    let mut buffer = [0_u8; 32];
    unsafe {
        if !libc::gmtime_r(&time, utc.as_mut_ptr()).is_null() {
            let count = libc::strftime(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                c"%Y-%m-%dT%H:%M:%SZ".as_ptr(),
                utc.as_ptr(),
            );
            if count > 0 {
                return String::from_utf8_lossy(&buffer[..count]).into_owned();
            }
        }
    }
    seconds.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_edits_but_excludes_runtime_bookkeeping_and_line_endings() {
        let base = std::env::temp_dir().join(format!("opsx-md-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&base).unwrap();
        let args = vec!["--max-provider-retries=10".to_owned()];
        let contents = render(
            "Test transactions",
            "Accepted the defaults.",
            &args,
            &args,
            &json!({}),
        )
        .unwrap();
        fs::write(base.join(FILE_NAME), &contents).unwrap();
        let campaign = CampaignConfig::read(&base).unwrap().unwrap();
        assert!(!campaign.changed());
        assert_eq!(campaign.arguments().unwrap(), args);
        let cli = resolve_configured_settings(&args, &["--no-config".to_owned()], &base).unwrap();
        campaign.record_used(&cli).unwrap();
        let used = CampaignConfig::read(&base).unwrap().unwrap();
        assert!(!used.changed());
        assert_eq!(used.arguments().unwrap(), args);
        assert!(json_section(&used.contents, "last-used").unwrap()["arguments"].is_array());
        let mut edited = used.clone();
        edited.contents = edited
            .contents
            .replace("Test transactions", "Test persistence");
        assert!(edited.changed());
        edited.contents = used.contents.replace("\n", "\r\n");
        assert!(!edited.changed());
        assert_eq!(edited.arguments().unwrap(), args);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn refuses_ambiguous_sections_and_preserves_preset_caveat() {
        let args = vec!["--worker-model=@preset/motd".to_owned()];
        let contents = render("Experiment", "", &args, &args, &json!({})).unwrap();
        assert!(contents.contains(PRESET_WARNING));
        assert!(
            json_section(
                &(contents.clone() + &section("arguments", "[]")),
                "arguments"
            )
            .is_err()
        );
        assert!(
            render(
                "<!-- opsx-build:arguments -->",
                "",
                &args,
                &args,
                &json!({})
            )
            .is_err()
        );
        assert!(
            render(
                "Intent",
                "<!-- /opsx-build:arguments -->",
                &args,
                &args,
                &json!({})
            )
            .is_err()
        );
    }

    #[test]
    fn config_harness_has_no_campaign_model_effort_permission_or_environment_overrides() {
        let command = configure_command(
            Path::new("/repo"),
            Path::new("/tmp/config"),
            Path::new("/tmp/config/reference.md"),
            Path::new("/tmp/config/proposal.json"),
        );
        assert_eq!(command.program, "claude");
        assert!(command.env.is_empty());
        assert!(command.env_remove.is_empty());
        assert!(!command.args.iter().any(|arg| matches!(
            arg.as_str(),
            "--model" | "--effort" | "--permission-mode" | "--print" | "--plugin-dir"
        )));
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg.contains("Defaults are accepted only when the user agrees"))
        );
    }
}
