use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::{
    agenda::parse_slice_name, model_confusions::claude_md_guidance as model_confusion_guidance,
    process::CommandSpec,
};

pub const BOOTSTRAP_CHANGE: &str = "bootstrap-implementation-slices";
pub const BOOTSTRAP_PATH: &str = "automation/bootstrap.md";
const CONFIG_PATH: &str = "openspec/config.yaml";
const CLAUDE_PATH: &str = "CLAUDE.md";
const AGENTS_PATH: &str = "AGENTS.md";
const SLICES_PATH: &str = "automation/slices";
const FINAL_SLICE: &str = "9999-project-acceptance.md";
const MANAGED_START: &str = "<!-- BEGIN OPSX-BUILD MANAGED -->";
const MANAGED_END: &str = "<!-- END OPSX-BUILD MANAGED -->";
const BOOTSTRAP_INSTRUCTIONS: &str = include_str!("../assets/bootstrap/bootstrap.md");
const CLAUDE_FRAGMENT: &str = include_str!("../assets/bootstrap/claude-fragment.md");
const MODEL_CONFUSIONS_PLACEHOLDER: &str = "{{OPSX_BUILD_MODEL_CONFUSIONS}}";
const WORKER_CAPACITY_PLACEHOLDER: &str = "{{OPSX_BUILD_WORKER_CAPACITY}}";

#[derive(Debug)]
pub struct BootstrapScaffold {
    pub context_path: PathBuf,
    config: String,
}

impl BootstrapScaffold {
    pub fn plan(
        repo: &Path,
        context_path: Option<&Path>,
        definitions: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let context_path = match context_path {
            Some(path) => path.to_path_buf(),
            None => {
                let default = repo.join("context.md");
                if !default.is_file() {
                    bail!(
                        "no project context supplied and `{}` is not a file; create it or use --context PATH",
                        default.display()
                    );
                }
                default
            }
        };
        let context_path = context_path.canonicalize().with_context(|| {
            format!(
                "could not resolve bootstrap context `{}`",
                context_path.display()
            )
        })?;
        let context_template = fs::read_to_string(&context_path).with_context(|| {
            format!(
                "could not read bootstrap context `{}`",
                context_path.display()
            )
        })?;
        if context_template.trim().is_empty() {
            bail!("bootstrap context `{}` is empty", context_path.display());
        }
        let context = render_context_template(&context_template, definitions)?;
        if context.trim().is_empty() {
            bail!(
                "bootstrap context `{}` is empty after template substitution",
                context_path.display()
            );
        }

        let config_path = repo.join(CONFIG_PATH);
        if config_path.exists() {
            bail!(
                "OpenSpec is already initialized at `{}`; bootstrap is for a new planning root",
                config_path.display()
            );
        }
        let bootstrap_path = repo.join(BOOTSTRAP_PATH);
        if bootstrap_path.exists() {
            bail!(
                "bootstrap instructions already exist at `{}`; preserve them and resume the existing run instead",
                bootstrap_path.display()
            );
        }
        let slices_path = repo.join(SLICES_PATH);
        if slices_path.is_dir()
            && fs::read_dir(&slices_path)
                .with_context(|| format!("could not inspect `{}`", slices_path.display()))?
                .next()
                .transpose()?
                .is_some()
        {
            bail!(
                "an implementation agenda already exists under `{}`; bootstrap will not replace it",
                slices_path.display()
            );
        }

        let existing_claude = match fs::read_to_string(repo.join(CLAUDE_PATH)) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).context("could not read CLAUDE.md"),
        };
        merge_managed_fragment(&existing_claude, CLAUDE_PATH)?;

        let existing_agents = read_optional_text(&repo.join(AGENTS_PATH), AGENTS_PATH)?;
        merge_managed_fragment(&existing_agents, AGENTS_PATH)?;

        Ok(Self {
            context_path,
            config: render_project_config(&context),
        })
    }

    pub fn write(&self, repo: &Path, frontier_worker: bool) -> Result<()> {
        fs::create_dir_all(repo.join("openspec"))
            .context("could not create the OpenSpec directory")?;
        fs::create_dir_all(repo.join(SLICES_PATH))
            .context("could not create the implementation-agenda directory")?;
        fs::write(repo.join(CONFIG_PATH), &self.config)
            .context("could not write openspec/config.yaml")?;
        fs::write(repo.join(BOOTSTRAP_PATH), instructions(frontier_worker))
            .context("could not write automation/bootstrap.md")?;
        let existing_claude = match fs::read_to_string(repo.join(CLAUDE_PATH)) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).context("could not read CLAUDE.md after OpenSpec init");
            }
        };
        let claude = merge_managed_fragment(&existing_claude, CLAUDE_PATH)?;
        fs::write(repo.join(CLAUDE_PATH), claude).context("could not update CLAUDE.md")?;
        ensure_codex_guidance(repo, false)?;
        Ok(())
    }

    pub fn paths() -> [&'static str; 4] {
        [CONFIG_PATH, BOOTSTRAP_PATH, CLAUDE_PATH, AGENTS_PATH]
    }
}

pub fn init_command(repo: &Path) -> CommandSpec {
    CommandSpec::new("openspec", repo).args(["init", "--tools", "claude", "--no-animation", "."])
}

pub fn instructions(frontier_worker: bool) -> String {
    BOOTSTRAP_INSTRUCTIONS.replace(
        WORKER_CAPACITY_PLACEHOLDER,
        worker_capacity_guidance(frontier_worker),
    )
}

pub fn worker_capacity_guidance(frontier_worker: bool) -> &'static str {
    if frontier_worker {
        "Implementation worker capacity: the worker is a frontier-capable model (--frontier-worker). Size coherent, testable delivery slices for that capability rather than imposing a smaller local model's limitations. The worker still needs explicit acceptance criteria and enough durable context to implement and verify each slice independently."
    } else {
        "Implementation worker capacity: assume the worker is a smaller local coding model, substantially less capable than the frontier planner. Size coherent, testable delivery slices that this worker can implement and verify reliably in one OpenSpec cycle. Provide focused scope, concrete implementation guidance, explicit acceptance criteria, and enough durable context to work without the planner's conversation history."
    }
}

pub fn validate_agenda(repo: &Path) -> Result<usize> {
    let directory = repo.join(SLICES_PATH);
    if !directory.is_dir() {
        bail!("bootstrap did not create `{SLICES_PATH}`");
    }
    if !directory.join("README.md").is_file() {
        bail!("bootstrap agenda omitted `{SLICES_PATH}/README.md`");
    }

    let mut ordinals = BTreeMap::<Vec<u32>, String>::new();
    let mut slices = Vec::new();
    for entry in fs::read_dir(&directory)
        .with_context(|| format!("could not read `{}`", directory.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("agenda contains a non-UTF-8 filename"))?;
        if file_name == "README.md" {
            continue;
        }
        if !file_name.ends_with(".md") {
            continue;
        }
        let (ordinal, ordinal_text, _) = parse_slice_name(&file_name).with_context(|| {
            format!("agenda file `{file_name}` is not named `<number>-<kebab-case-slug>.md`")
        })?;
        if ordinal_text.len() != 4 || !ordinal_text.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("bootstrap agenda file `{file_name}` must begin with one four-digit ordinal");
        }
        if let Some(other) = ordinals.insert(ordinal, file_name.clone()) {
            bail!("agenda files `{other}` and `{file_name}` use the same ordinal");
        }
        let contents = fs::read_to_string(entry.path())
            .with_context(|| format!("could not read agenda slice `{file_name}`"))?;
        if !contents.lines().any(|line| line.starts_with("# ")) {
            bail!("agenda slice `{file_name}` omitted its level-one title");
        }
        for heading in [
            "## Objective",
            "## Prerequisites",
            "## Acceptance Criteria",
            "## Required Tests",
        ] {
            if !contents.lines().any(|line| line.trim_end() == heading) {
                bail!("agenda slice `{file_name}` omitted required heading `{heading}`");
            }
        }
        slices.push((file_name, contents));
    }

    if slices.len() < 2 {
        bail!(
            "bootstrap agenda must contain at least one delivery slice and the final acceptance slice"
        );
    }
    let final_contents = slices
        .iter()
        .find_map(|(name, contents)| (name == FINAL_SLICE).then_some(contents))
        .with_context(|| {
            format!("bootstrap agenda omitted final gate `{SLICES_PATH}/{FINAL_SLICE}`")
        })?;
    if !final_contents
        .lines()
        .any(|line| line.trim_end() == "## Project Goal Coverage")
    {
        bail!("final gate `{FINAL_SLICE}` omitted required heading `## Project Goal Coverage`");
    }
    Ok(slices.len())
}

pub fn render_project_config(context: &str) -> String {
    let mut rendered = String::from(
        "schema: spec-driven\n\n# Project context supplied to artifact-generating agents.\ncontext: |\n",
    );
    for line in context.trim().lines() {
        rendered.push_str("  ");
        rendered.push_str(line.trim_end());
        rendered.push('\n');
    }
    rendered.push_str(
        "\nrules:\n  proposal:\n    - Keep each change small enough to implement and verify independently.\n  specs:\n    - Specify observable behaviour rather than implementation details.\n  design:\n    - Treat third-party dependency choices as revisable design decisions, not requirements.\n    - Validate material dependencies with a minimal end-to-end proof before building substantial work around them.\n    - Record a fallback when a dependency does not naturally support the specified behaviour.\n  tasks:\n    - Include tests and end-to-end verification.\n    - Keep each change bounded enough for the configured worker model to complete reliably.\n\noperations:\n  apply:\n    guidance:\n      - Preserve correct partial work and keep validation summaries concise.\n  archive:\n    guidance:\n      - Summarize the archive outcome before finishing.\n",
    );
    rendered
}

fn render_context_template(
    template: &str,
    definitions: &BTreeMap<String, String>,
) -> Result<String> {
    let mut rendered = String::with_capacity(template.len());
    let mut remaining = template;
    let mut used = BTreeSet::new();

    loop {
        let next_open = remaining.find("{{");
        let next_close = remaining.find("}}");
        if next_close.is_some_and(|close| next_open.is_none_or(|open| close < open)) {
            bail!("bootstrap context contains an unmatched `}}}}`");
        }
        let Some(open) = next_open else {
            rendered.push_str(remaining);
            break;
        };
        rendered.push_str(&remaining[..open]);
        let placeholder = &remaining[open + 2..];
        let close = placeholder
            .find("}}")
            .context("bootstrap context contains an unclosed `{{` placeholder")?;
        let name = placeholder[..close].trim();
        if !valid_template_name(name) {
            bail!(
                "invalid bootstrap context placeholder `{{{{{name}}}}}`; names use letters, digits, and underscores and begin with a letter or underscore"
            );
        }
        let value = definitions
            .get(name)
            .with_context(|| format!("bootstrap context requires `--define {name}=VALUE`"))?;
        rendered.push_str(value);
        used.insert(name.to_owned());
        remaining = &placeholder[close + 2..];
    }

    let unused = definitions
        .keys()
        .filter(|name| !used.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !unused.is_empty() {
        bail!(
            "bootstrap definition(s) have no matching context placeholder: {}",
            unused.join(", ")
        );
    }
    Ok(rendered)
}

fn valid_template_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

pub fn ensure_codex_guidance(repo: &Path, dry_run: bool) -> Result<bool> {
    let path = repo.join(AGENTS_PATH);
    let existing = read_optional_text(&path, AGENTS_PATH)?;
    let merged = merge_managed_fragment(&existing, AGENTS_PATH)?;
    if merged == existing {
        return Ok(false);
    }
    if !dry_run {
        fs::write(&path, merged).context("could not update AGENTS.md")?;
    }
    Ok(true)
}

fn read_optional_text(path: &Path, label: &str) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("could not read {label}")),
    }
}

fn merge_managed_fragment(existing: &str, file_name: &str) -> Result<String> {
    let fragment =
        CLAUDE_FRAGMENT.replace(MODEL_CONFUSIONS_PLACEHOLDER, &model_confusion_guidance());
    let starts = existing.match_indices(MANAGED_START).collect::<Vec<_>>();
    let ends = existing.match_indices(MANAGED_END).collect::<Vec<_>>();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => {
            let mut merged = existing.trim_end().to_owned();
            if !merged.is_empty() {
                merged.push_str("\n\n");
            }
            merged.push_str(fragment.trim());
            merged.push('\n');
            Ok(merged)
        }
        ([(start, _)], [(end, _)]) if start < end => {
            let end = end + MANAGED_END.len();
            let mut merged = String::with_capacity(existing.len() + fragment.len());
            merged.push_str(&existing[..*start]);
            merged.push_str(fragment.trim());
            merged.push_str(&existing[end..]);
            Ok(merged)
        }
        _ => bail!(
            "{file_name} has malformed or repeated opsx-build managed markers; repair them before bootstrapping"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn bootstrap_context_defaults_to_the_campaign_repository() {
        let repo = std::env::temp_dir().join(format!("opsx-context-default-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        let context = repo.join("context.md");
        fs::write(&context, "# Widget\nBuild it in {{language}}.\n").unwrap();

        let scaffold = BootstrapScaffold::plan(
            &repo,
            None,
            &BTreeMap::from([("language".to_owned(), "Go".to_owned())]),
        )
        .unwrap();

        assert_eq!(scaffold.context_path, context.canonicalize().unwrap());
        assert!(scaffold.config.contains("Build it in Go."));
        assert!(!repo.join("openspec").exists());
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn bootstrap_context_explicit_path_takes_precedence_without_fallback() {
        let directory =
            std::env::temp_dir().join(format!("opsx-context-override-{}", Uuid::new_v4()));
        let repo = directory.join("campaign");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("context.md"), "Default context").unwrap();
        let explicit = directory.join("project.md");
        fs::write(&explicit, "Explicit context").unwrap();

        let scaffold = BootstrapScaffold::plan(&repo, Some(&explicit), &BTreeMap::new()).unwrap();

        assert_eq!(scaffold.context_path, explicit.canonicalize().unwrap());
        assert!(scaffold.config.contains("Explicit context"));
        assert!(!scaffold.config.contains("Default context"));
        fs::remove_file(&explicit).unwrap();
        let error = BootstrapScaffold::plan(&repo, Some(&explicit), &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("project.md"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bootstrap_context_requires_a_file_when_no_path_is_supplied() {
        let repo = std::env::temp_dir().join(format!("opsx-context-missing-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        let error = BootstrapScaffold::plan(&repo, None, &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("context.md"));
        assert!(error.to_string().contains("--context PATH"));

        fs::create_dir(repo.join("context.md")).unwrap();
        assert!(BootstrapScaffold::plan(&repo, None, &BTreeMap::new()).is_err());
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn renders_markdown_as_yaml_context() {
        let rendered = render_project_config("# Widget\n\nBuild a useful widget.\n");
        assert!(rendered.contains("context: |\n  # Widget\n  \n  Build a useful widget.\n"));
        assert!(rendered.contains("schema: spec-driven"));
        assert!(rendered.contains("Keep each change bounded enough"));
        assert!(rendered.contains("Treat third-party dependency choices as revisable"));
        assert!(
            rendered.contains("Validate material dependencies with a minimal end-to-end proof")
        );
    }

    #[test]
    fn bootstrap_instructions_assign_agenda_materialization_to_apply() {
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("## Stage ownership"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("exact filename form"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("begin with a level-one Markdown title"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("at least one delivery slice in addition"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("During OpenSpec Propose"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("Do not create or modify"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("During OpenSpec Apply"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("exclusively owns creation"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("Before reporting Apply complete"));
        assert!(BOOTSTRAP_INSTRUCTIONS.contains("final gate must contain"));
    }

    #[test]
    fn bootstrap_file_records_the_selected_worker_capacity() {
        for frontier_worker in [false, true] {
            let repo =
                std::env::temp_dir().join(format!("opsx-worker-capacity-{}", Uuid::new_v4()));
            fs::create_dir_all(&repo).unwrap();
            fs::write(
                repo.join("context.md"),
                "# Widget\nBuild a tested widget.\n",
            )
            .unwrap();
            let scaffold = BootstrapScaffold::plan(&repo, None, &BTreeMap::new()).unwrap();
            scaffold.write(&repo, frontier_worker).unwrap();
            let saved = fs::read_to_string(repo.join(BOOTSTRAP_PATH)).unwrap();
            assert_eq!(saved, instructions(frontier_worker));
            assert!(saved.contains(worker_capacity_guidance(frontier_worker)));
            assert!(!saved.contains(WORKER_CAPACITY_PLACEHOLDER));
            assert_eq!(
                saved.contains("substantially less capable"),
                !frontier_worker
            );
            assert_eq!(saved.contains("frontier-capable model"), frontier_worker);
            assert!(saved.contains("Several files, subsystems, test cases"));
            fs::remove_dir_all(repo).unwrap();
        }
    }

    #[test]
    fn renders_strict_context_variables_without_recursive_expansion() {
        let definitions = BTreeMap::from([
            ("language".to_owned(), "Go".to_owned()),
            (
                "formatter".to_owned(),
                "gofmt {{not_a_variable}}".to_owned(),
            ),
        ]);
        let rendered = render_context_template(
            "Written in {{ language }} and formatted with {{formatter}}.\n",
            &definitions,
        )
        .unwrap();
        assert_eq!(
            rendered,
            "Written in Go and formatted with gofmt {{not_a_variable}}.\n"
        );
    }

    #[test]
    fn rejects_missing_unused_and_malformed_context_variables() {
        let language = BTreeMap::from([("language".to_owned(), "Go".to_owned())]);
        assert!(render_context_template("Written in {{missing}}.", &language).is_err());
        assert!(render_context_template("No variables.", &language).is_err());
        assert!(render_context_template("Broken {{language.", &language).is_err());
        assert!(render_context_template("Broken }}.", &BTreeMap::new()).is_err());
        assert!(render_context_template("Broken {{bad-name}}.", &BTreeMap::new()).is_err());
    }

    #[test]
    fn appends_and_replaces_only_the_managed_claude_fragment() {
        let first = merge_managed_fragment("# User rules\n\nKeep this.\n", CLAUDE_PATH).unwrap();
        assert!(first.starts_with("# User rules\n\nKeep this.\n\n"));
        assert_eq!(first.matches(MANAGED_START).count(), 1);

        let updated = merge_managed_fragment(&first, CLAUDE_PATH).unwrap();
        assert_eq!(updated, first);
        assert!(merge_managed_fragment(MANAGED_START, CLAUDE_PATH).is_err());
    }

    #[test]
    fn managed_fragment_preserves_project_execution_policy() {
        assert!(CLAUDE_FRAGMENT.contains("verify that a language server"));
        assert!(CLAUDE_FRAGMENT.contains("Strongly prefer language-server facilities"));
        assert!(CLAUDE_FRAGMENT.contains("canonical formatter"));
        assert!(CLAUDE_FRAGMENT.contains("Format every source file changed"));
        assert!(CLAUDE_FRAGMENT.contains("### Bounded network operations"));
        assert!(CLAUDE_FRAGMENT.contains("explicit, practical timeouts"));
        assert!(CLAUDE_FRAGMENT.contains("connection and overall timeout controls"));
        assert!(CLAUDE_FRAGMENT.contains("increasingly large"));
        assert!(CLAUDE_FRAGMENT.contains("Treat third-party library and framework choices"));
        assert!(CLAUDE_FRAGMENT.contains("Do not distort specified behaviour"));
        assert!(CLAUDE_FRAGMENT.contains("### Test execution and agent sandboxes"));
        assert!(CLAUDE_FRAGMENT.contains("supported approval or elevated-execution"));
        assert!(CLAUDE_FRAGMENT.contains("substitute synthetic or weaker coverage"));
        assert!(CLAUDE_FRAGMENT.contains("required real checks have run and passed"));
    }

    #[test]
    fn managed_fragment_includes_the_qwen36_confusion_incident() {
        let fragment = merge_managed_fragment("", CLAUDE_PATH).unwrap();
        assert!(fragment.contains("### Known model-specific confusions"));
        assert!(fragment.contains("qwen3.6-openspec-duplicated-s"));
        assert!(fragment.contains("observed with Qwen3.6"));
        assert!(fragment.contains("Spell the product name exactly `OpenSpec`"));
        assert!(fragment.contains("exactly one `s` and two `p`"));
    }

    #[test]
    fn validates_a_complete_ordered_agenda() {
        let repo = std::env::temp_dir().join(format!("opsx-bootstrap-test-{}", Uuid::new_v4()));
        let slices = repo.join(SLICES_PATH);
        fs::create_dir_all(&slices).unwrap();
        fs::write(slices.join("README.md"), "# Plan\n").unwrap();
        let ordinary = "# Scaffold\n## Objective\nOne.\n## Prerequisites\nNone.\n## Acceptance Criteria\n- Works.\n## Required Tests\n- Test.\n";
        fs::write(slices.join("0001-scaffold.md"), ordinary).unwrap();
        let final_gate = "# Project acceptance\n## Objective\nAccept.\n## Prerequisites\nAll.\n## Acceptance Criteria\n- Done.\n## Required Tests\n- End to end.\n## Project Goal Coverage\nEverything.\n";
        fs::write(slices.join(FINAL_SLICE), final_gate).unwrap();

        assert_eq!(validate_agenda(&repo).unwrap(), 2);
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn rejects_an_agenda_without_the_terminal_gate() {
        let repo = std::env::temp_dir().join(format!("opsx-bootstrap-test-{}", Uuid::new_v4()));
        let slices = repo.join(SLICES_PATH);
        fs::create_dir_all(&slices).unwrap();
        fs::write(slices.join("README.md"), "# Plan\n").unwrap();
        let ordinary = "# Slice\n## Objective\nOne.\n## Prerequisites\nNone.\n## Acceptance Criteria\n- Works.\n## Required Tests\n- Test.\n";
        fs::write(slices.join("0001-only.md"), ordinary).unwrap();

        assert!(validate_agenda(&repo).is_err());
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn constructs_noninteractive_openspec_initialization() {
        let command = init_command(Path::new("/tmp/example"));
        assert_eq!(command.program, "openspec");
        assert_eq!(
            command.args,
            ["init", "--tools", "claude", "--no-animation", "."]
        );
    }
}
