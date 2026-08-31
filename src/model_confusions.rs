use std::{collections::BTreeSet, fs, path::Path, path::PathBuf, sync::OnceLock};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{git::metadata_dir, ui::Ui};

const CATALOG_SOURCE: &str = include_str!("../assets/model-confusions.toml");
const PLUGIN_MANIFEST: &str = include_str!("../assets/claude-plugin/.claude-plugin/plugin.json");
const HOOKS_MANIFEST: &str = include_str!("../assets/claude-plugin/hooks/hooks.json");

#[derive(Debug, Deserialize)]
struct Catalog {
    confusion: Vec<ModelConfusion>,
}

#[derive(Debug, Deserialize)]
struct ModelConfusion {
    id: String,
    observed_with: Vec<String>,
    forbidden_literal: String,
    guidance: String,
    diagnostic: String,
}

pub fn ensure_model_confusion_plugin<U: Ui>(repo: &Path, ui: &U) -> Result<PathBuf> {
    let root = metadata_dir(repo, ui)?.join("claude-plugin");
    write_if_changed(&root.join(".claude-plugin/plugin.json"), PLUGIN_MANIFEST)?;
    write_if_changed(&root.join("hooks/hooks.json"), HOOKS_MANIFEST)?;
    write_if_changed(&root.join("model-confusions.toml"), CATALOG_SOURCE)?;
    write_if_changed(
        &root.join("hooks/reject-model-confusions.sh"),
        &render_reject_script(catalog()),
    )?;

    let retired_hook = root.join("hooks/reject-opensspec.sh");
    match fs::remove_file(&retired_hook) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "could not retire old model-confusion hook `{}`",
                    retired_hook.display()
                )
            });
        }
    }
    Ok(root)
}

pub fn prompt_guidance() -> String {
    render_guidance("KNOWN MODEL-SPECIFIC CONFUSIONS", catalog())
}

pub fn claude_md_guidance() -> String {
    render_guidance("### Known model-specific confusions", catalog())
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        parse_catalog(CATALOG_SOURCE).expect("embedded model-confusion catalogue must be valid")
    })
}

fn parse_catalog(source: &str) -> Result<Catalog> {
    let catalog: Catalog = toml::from_str(source).context("invalid model-confusion catalogue")?;
    if catalog.confusion.is_empty() {
        bail!("model-confusion catalogue must contain at least one entry");
    }

    let mut ids = BTreeSet::new();
    let mut literals = BTreeSet::new();
    for confusion in &catalog.confusion {
        if confusion.id.trim().is_empty()
            || confusion.observed_with.is_empty()
            || confusion.forbidden_literal.is_empty()
            || confusion.forbidden_literal.contains(['\n', '\r'])
            || confusion.guidance.trim().is_empty()
            || confusion.diagnostic.trim().is_empty()
        {
            bail!(
                "model-confusion entries require an id, observed model, one-line literal, guidance, and diagnostic"
            );
        }
        if !ids.insert(&confusion.id) {
            bail!("duplicate model-confusion id `{}`", confusion.id);
        }
        let literal = confusion.forbidden_literal.to_ascii_lowercase();
        if !literals.insert(literal) {
            bail!(
                "duplicate model-confusion forbidden literal `{}`",
                confusion.forbidden_literal
            );
        }
    }
    Ok(catalog)
}

fn render_guidance(heading: &str, catalog: &Catalog) -> String {
    let mut output = format!("{heading}\n\n");
    for confusion in &catalog.confusion {
        output.push_str(&format!(
            "- Incident `{}` (observed with {}): {}\n",
            confusion.id,
            confusion.observed_with.join(", "),
            confusion.guidance
        ));
    }
    output.trim_end().to_owned()
}

fn render_reject_script(catalog: &Catalog) -> String {
    let mut script = "#!/bin/sh\n\ninput=$(cat)\n\n".to_owned();
    for confusion in &catalog.confusion {
        script.push_str("if printf '%s' \"$input\" | grep -Fqi -- ");
        script.push_str(&shell_quote(&confusion.forbidden_literal));
        script.push_str("; then\n    printf '%s\\n' ");
        script.push_str(&shell_quote(&format!(
            "Rejected by opsx-build incident `{}`. {}",
            confusion.id, confusion.diagnostic
        )));
        script.push_str(" >&2\n    exit 2\nfi\n\n");
    }
    script.push_str(
        "if printf '%s' \"$input\" | grep -Eq '\"hook_event_name\"[[:space:]]*:[[:space:]]*\"Stop\"'; then\n",
    );
    for confusion in &catalog.confusion {
        script.push_str("    git grep --no-index -I -Fqi -- ");
        script.push_str(&shell_quote(&confusion.forbidden_literal));
        script.push_str(" -- . >/dev/null 2>&1\n    status=$?\n    if [ \"$status\" -eq 0 ]; then\n        printf '%s\\n' ");
        script.push_str(&shell_quote(&format!(
            "Rejected stage completion by opsx-build incident `{}`. Repository content still contains the forbidden literal `{}`. Correct it without bypassing or disabling the guard, then finish the stage again.",
            confusion.id, confusion.forbidden_literal
        )));
        script.push_str(" >&2\n        exit 2\n    fi\n    if [ \"$status\" -gt 1 ]; then\n        printf '%s\\n' 'opsx-build could not verify the repository model-confusion invariant; do not finish until the repository can be checked' >&2\n        exit 2\n    fi\n");
    }
    script.push_str("fi\n\n");
    script.push_str("exit 0\n");
    script
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create `{}`", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("could not write `{}`", path.display()))
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use uuid::Uuid;

    use crate::ui::TerminalUi;

    use super::*;

    fn fixture() -> PathBuf {
        let repo = std::env::temp_dir().join(format!("opsx-build-confusions-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(status.success());
        repo
    }

    fn run_hook(script: &Path, input: &str) -> std::process::Output {
        run_hook_in(script, input, script.parent().unwrap())
    }

    fn run_hook_in(script: &Path, input: &str, cwd: &Path) -> std::process::Output {
        let mut child = Command::new("/bin/sh")
            .arg(script)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn qwen36_incident_blocks_the_duplicated_s_before_a_tool_runs() {
        let repo = fixture();
        let ui = TerminalUi::new(false, false, false);
        let plugin = ensure_model_confusion_plugin(&repo, &ui).unwrap();
        let script = plugin.join("hooks/reject-model-confusions.sh");

        let rejected = run_hook(
            &script,
            r#"{"tool_name":"Bash","tool_input":{"command":"git commit -m 'opensspec: complete slice'"}}"#,
        );
        assert_eq!(rejected.status.code(), Some(2));
        let diagnostic = String::from_utf8(rejected.stderr).unwrap();
        assert!(diagnostic.contains("qwen3.6-openspec-duplicated-s"));
        assert!(diagnostic.contains("Qwen3.6-induced failure mode"));
        assert!(diagnostic.contains("exactly one `s` and two `p`"));
        assert!(diagnostic.contains("Do not bypass, disable, alias"));

        let accepted = run_hook(
            &script,
            r#"{"tool_name":"Bash","tool_input":{"command":"git commit -m 'openspec: complete slice'"}}"#,
        );
        assert!(accepted.status.success());
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn stop_hook_refuses_to_finish_with_a_confusion_in_repository_content() {
        let repo = fixture();
        let ui = TerminalUi::new(false, false, false);
        let plugin = ensure_model_confusion_plugin(&repo, &ui).unwrap();
        let script = plugin.join("hooks/reject-model-confusions.sh");
        let stop = r#"{"hook_event_name":"Stop","last_assistant_message":"Ready"}"#;

        fs::write(repo.join("bad.md"), "use the opensspec command\n").unwrap();
        let rejected = run_hook_in(&script, stop, &repo);
        assert_eq!(rejected.status.code(), Some(2));
        assert!(
            String::from_utf8(rejected.stderr)
                .unwrap()
                .contains("Rejected stage completion")
        );

        fs::write(repo.join("bad.md"), "use the openspec command\n").unwrap();
        assert!(run_hook_in(&script, stop, &repo).status.success());
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn adding_a_catalogue_entry_extends_guidance_and_the_hook() {
        let catalog = parse_catalog(
            r#"
[[confusion]]
id = "model-v1-widget-name"
observed_with = ["Model v1"]
forbidden_literal = "widgte"
guidance = "Spell widget correctly."
diagnostic = "Model v1 transposes the final letters."
"#,
        )
        .unwrap();
        let guidance = render_guidance("CONFUSIONS", &catalog);
        assert!(guidance.contains("model-v1-widget-name"));
        assert!(guidance.contains("observed with Model v1"));

        let directory = std::env::temp_dir().join(format!("opsx-hook-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let script = directory.join("hook.sh");
        fs::write(&script, render_reject_script(&catalog)).unwrap();
        let rejected = run_hook(&script, r#"{"content":"widgte"}"#);
        assert_eq!(rejected.status.code(), Some(2));
        assert!(
            String::from_utf8(rejected.stderr)
                .unwrap()
                .contains("model-v1-widget-name")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn plugin_is_private_git_metadata_and_idempotent() {
        let repo = fixture();
        let ui = TerminalUi::new(false, false, false);
        let first = ensure_model_confusion_plugin(&repo, &ui).unwrap();
        let second = ensure_model_confusion_plugin(&repo, &ui).unwrap();

        assert_eq!(first, second);
        assert!(first.starts_with(repo.join(".git/opsx-build")));
        assert!(first.join(".claude-plugin/plugin.json").is_file());
        assert!(first.join("hooks/hooks.json").is_file());
        assert!(first.join("model-confusions.toml").is_file());
        assert!(
            fs::read_to_string(first.join("hooks/hooks.json"))
                .unwrap()
                .contains("\"Stop\"")
        );
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn embedded_catalogue_is_valid_and_identifies_qwen36() {
        let catalog = parse_catalog(CATALOG_SOURCE).unwrap();
        assert_eq!(catalog.confusion.len(), 1);
        assert_eq!(catalog.confusion[0].observed_with, ["Qwen3.6"]);
    }
}
