//! The optional workflow for implementing requirements supplied by the maintainer.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "opsx-supplied-contracts";
const SOURCE_REFERENCE_GUIDANCE: &str = "Every Markdown link inside `## Supplied requirements` must point directly to a declared supplied input. Generated proposals (including archived proposals), agenda READMEs, coverage tables and other slices are planning artifacts, not supplied inputs. Keep navigation or planning-evidence links under a separate level-two heading such as `## Planning references`. Replace indirect requirement references with links to the original declared files; moving a link must not remove its requirements from coverage. Do not add generated artifacts to the protected-input list or edit supplied inputs to satisfy this check. Correct all reported references together.";
pub(crate) const CONFIGURE_GUIDANCE: &str = "\n\nWorkflow choice: read the campaign context file (the selected --context, otherwise context.md if present) and its declared relevant inputs. When these supply precise existing behavioural contracts and component ownership, suggest --supplied-contracts and explain that it plans implementation through original source references, design and tasks without generating replacement specifications. For exploratory work that still develops requirements, suggest the ordinary workflow. Let the user accept the suggestion in the configuration conversation; never silently switch an existing choice. Record the agreed workflow, repeatable --contract PATH arguments naming the exact authoritative files, and optional --acceptance-file PATH arguments naming maintainer-owned checks/fixtures to preserve. Paths are relative to the campaign root; files must already exist, and directories/globs are not supported. Identify supplied acceptance expectations separately from tests the campaign should implement. Acceptance files are protected inputs; listing them does not execute checks. If input completeness or ownership is unclear, explain that uncertainty rather than assuming it. The actual runner uses only the recorded selection and never infers its workflow from context at launch. Disabling this option for a new run does not change the workflow recorded by an existing resumable run.";
const ASSETS: &[(&str, &str)] = &[
    (
        "schema.yaml",
        include_str!("../assets/supplied-contracts/schema.yaml"),
    ),
    (
        "templates/proposal.md",
        include_str!("../assets/supplied-contracts/proposal.md"),
    ),
    (
        "templates/design.md",
        include_str!("../assets/supplied-contracts/design.md"),
    ),
    (
        "templates/tasks.md",
        include_str!("../assets/supplied-contracts/tasks.md"),
    ),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Input {
    path: PathBuf,
    resolved: PathBuf,
    hash: String,
    executable: bool,
    contract: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SuppliedContracts {
    inputs: Vec<Input>,
    #[serde(default)]
    pub acceptance: Option<crate::acceptance::AcceptanceGate>,
}

impl SuppliedContracts {
    pub fn paths(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let (contracts, acceptance): (Vec<_>, Vec<_>) =
            self.inputs.iter().partition(|i| i.contract);
        (
            contracts.iter().map(|i| i.path.clone()).collect(),
            acceptance.iter().map(|i| i.path.clone()).collect(),
        )
    }
    pub fn capture(repo: &Path, contracts: &[PathBuf], acceptance: &[PathBuf]) -> Result<Self> {
        if contracts.is_empty() {
            bail!("--supplied-contracts requires at least one --contract PATH");
        }
        let mut inputs = Vec::new();
        let mut seen = BTreeSet::new();
        for (paths, contract) in [(contracts, true), (acceptance, false)] {
            for path in paths {
                let path = repo.join(path);
                let resolved = path.canonicalize().with_context(|| {
                    format!("cannot resolve supplied input `{}`", path.display())
                })?;
                if !resolved.is_file() {
                    bail!(
                        "supplied input `{}` must be a file; list each file explicitly",
                        path.display()
                    );
                }
                if !seen.insert(resolved.clone()) {
                    bail!(
                        "supplied input `{}` was listed more than once",
                        path.display()
                    );
                }
                if contract && fs::read_to_string(&path)?.trim().is_empty() {
                    bail!("supplied contract `{}` is empty", path.display());
                }
                inputs.push(Input {
                    executable: executable(&path)?,
                    path,
                    resolved,
                    hash: String::new(),
                    contract,
                });
            }
        }
        let fingerprints = hashes(
            repo,
            &inputs.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
        )?;
        for (input, hash) in inputs.iter_mut().zip(fingerprints) {
            input.hash = hash;
        }
        Ok(Self {
            inputs,
            acceptance: None,
        })
    }

    pub fn check_selection(
        &self,
        repo: &Path,
        contracts: &[PathBuf],
        acceptance: &[PathBuf],
    ) -> Result<()> {
        if contracts.is_empty() && acceptance.is_empty() {
            return Ok(()); // A plain --resume uses the recorded inputs.
        }
        let selected = Self::capture(repo, contracts, acceptance)?;
        let paths = |value: &Self| {
            value
                .inputs
                .iter()
                .map(|i| (i.resolved.clone(), i.contract))
                .collect::<BTreeSet<_>>()
        };
        if paths(self) != paths(&selected) {
            bail!(
                "supplied input selection differs from the checkpoint; resume with the original inputs or start a new run"
            );
        }
        Ok(())
    }

    pub fn check_inputs(&self, repo: &Path) -> Result<()> {
        let paths = self
            .inputs
            .iter()
            .map(|i| i.path.clone())
            .collect::<Vec<_>>();
        for (input, hash) in self.inputs.iter().zip(hashes(repo, &paths)?) {
            if input.path.canonicalize()? != input.resolved
                || hash != input.hash
                || executable(&input.path)? != input.executable
            {
                bail!(
                    "protected supplied input changed: `{}`; work is preserved. Restore the approved input or start a new run after reviewing the change",
                    input.path.display()
                );
            }
        }
        Ok(())
    }

    pub fn guidance(&self) -> String {
        let mut text = String::from(
            "\n\nSUPPLIED-CONTRACT WORKFLOW\nThe maintainer selected implementation of existing contracts. Read the relevant original inputs listed below before planning, applying, repairing, or verifying; follow their declared reading boundaries and precedence. Generated agenda, proposal, design, tasks and tests cannot replace these requirements. Do not edit supplied inputs or weaken their conditions or exceptions. Internal implementation choices remain yours; only a genuine gap in externally observable requirements needs an external decision.\n\nCreate changes with `openspec new change <name> --schema opsx-supplied-contracts`. Produce proposal, design and tasks only; no spec deltas or new canonical OpenSpec specifications. This workflow replaces spec-generation instructions in the ordinary workflow. Keep its schema and templates unchanged. Propose maps requirements to work; design explains internal decisions; tasks are coherent, testable implementation steps within the assigned slice. Do not subdivide merely to make a task list.\n\nIn each proposal, agenda slice, and agenda README include `## Supplied requirements`. Use Markdown links to the original input files, relative to that document (absolute paths are also accepted). Link to a heading anchor or exact requirement/scenario ID with #fragment where available. Beside each link identify the work/tasks or slices that cover it; do not rewrite the requirement or expected result. Every such document must link at least one declared contract. Link supplied acceptance files where relevant. Keep the normal Objective, Prerequisites, Acceptance Criteria and Required Tests headings in slices, using source references for required behaviour and expectations. Add implementation-specific tests without substituting them for supplied scenarios.\n\nVerify against the original linked requirements and supplied expectations, including conditions and exceptions. Missing generated specs are intentional. Repair incorrect derived plans, code and tests together. Archive the proposal, design and tasks without synchronizing specs. The terminal 9999 slice covers all supplied in-scope requirements and checks; it cannot redefine acceptance.\n\nProtected inputs (versions recorded in the checkpoint):\n",
        );
        for input in &self.inputs {
            text.push_str(&format!(
                "- {}: `{}`\n",
                if input.contract {
                    "contract"
                } else {
                    "acceptance file"
                },
                input.path.display()
            ));
        }
        text.push_str("\nAuthoring rules above apply only to the owning stage: Propose plans, Apply/Repair implement, Verify reads and checks, and Archive archives. Commit stages preserve these inputs and include the bundled workflow schema when it is new; they do not create new plans.\n");
        text.push_str(SOURCE_REFERENCE_GUIDANCE);
        text.push('\n');
        if let Some(gate) = &self.acceptance {
            text.push_str(&gate.guidance());
        }
        text
    }

    pub fn install_schema(&self, repo: &Path) -> Result<()> {
        let root = schema_root(repo);
        for (relative, contents) in ASSETS {
            let path = root.join(relative);
            if path.exists() {
                if fs::read_to_string(&path)? != *contents {
                    bail!(
                        "workflow file `{}` differs from the bundled schema; preserve and reconcile it before starting",
                        path.display()
                    );
                }
            } else {
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(path, contents)?;
            }
        }
        self.check_schema(repo)
    }

    pub fn check_schema(&self, repo: &Path) -> Result<()> {
        for (relative, contents) in ASSETS {
            let path = schema_root(repo).join(relative);
            if fs::read_to_string(&path).ok().as_deref() != Some(*contents) {
                bail!(
                    "supplied-contract workflow file `{}` is missing or changed; restore it before continuing",
                    path.display()
                );
            }
        }
        Ok(())
    }

    pub fn check_proposal(&self, repo: &Path, change: &str, schema: Option<&str>) -> Result<()> {
        if schema != Some(SCHEMA) {
            bail!(
                "change `{change}` must use schema `{SCHEMA}`; correct its .openspec.yaml and replace generated specifications with source references"
            );
        }
        let root = repo.join("openspec/changes").join(change);
        if root.join("specs").exists() {
            bail!(
                "change `{change}` contains a specs directory; remove replacement specifications and reference supplied contracts in proposal/design/tasks"
            );
        }
        self.check_references(&root.join("proposal.md"))
    }

    pub fn check_agenda(&self, repo: &Path) -> Result<()> {
        let root = repo.join("automation/slices");
        let mut documents = vec![root.join("README.md")];
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| crate::agenda::parse_slice_name(name).is_some())
            {
                documents.push(entry.path());
            }
        }
        documents.sort();
        let mut errors = Vec::new();
        for document in documents {
            match self.reference_errors(&document) {
                Ok(issues) => errors.extend(issues),
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        check_reference_errors(errors)
    }

    pub fn check_references(&self, document: &Path) -> Result<()> {
        check_reference_errors(self.reference_errors(document)?)
    }

    fn reference_errors(&self, document: &Path) -> Result<Vec<String>> {
        let text = fs::read_to_string(document).with_context(|| {
            format!("cannot read source references in `{}`", document.display())
        })?;
        let mut in_section = false;
        let mut section = String::new();
        let mut first_line = 1;
        for (index, line) in text.lines().enumerate() {
            if line.trim_end() == "## Supplied requirements" {
                in_section = true;
                first_line = index + 2;
            } else if in_section && (line.starts_with("## ") || line.starts_with("# ")) {
                break;
            } else if in_section {
                section.push_str(line);
                section.push('\n');
            }
        }
        let mut contract_link = false;
        let mut errors = Vec::new();
        for (event, range) in Parser::new(&section).into_offset_iter() {
            let Event::Start(Tag::Link { dest_url, .. }) = event else {
                continue;
            };
            match self.check_reference(document, &dest_url) {
                Ok(is_contract) => contract_link |= is_contract,
                Err(error) => {
                    let line = first_line
                        + section[..range.start]
                            .bytes()
                            .filter(|b| *b == b'\n')
                            .count();
                    errors.push(format!("{error:#} (line {line})"));
                }
            }
        }
        if !contract_link {
            errors.push(format!(
                "`{}` needs `## Supplied requirements` with Markdown links to governing declared contracts",
                document.display()
            ));
        }
        Ok(errors)
    }

    fn check_reference(&self, document: &Path, dest_url: &str) -> Result<bool> {
        let (file, fragment) = dest_url
            .split_once('#')
            .map_or((dest_url, None), |(file, fragment)| (file, Some(fragment)));
        let path = document
            .parent()
            .unwrap()
            .join(decode_link(file)?)
            .canonicalize()
            .with_context(|| {
                format!(
                    "unresolved source reference `{dest_url}` in `{}`",
                    document.display()
                )
            })?;
        let input = self
            .inputs
            .iter()
            .find(|input| input.resolved == path)
            .with_context(|| {
                format!(
                    "reference `{dest_url}` in `{}` is not a declared supplied input",
                    document.display()
                )
            })?;
        if let Some(fragment) = fragment {
            let fragment = decode_link(fragment)?;
            if !has_fragment(&fs::read_to_string(&path)?, &fragment) {
                bail!(
                    "unresolved requirement/section `{fragment}` in `{}` (referenced by `{}`)",
                    path.display(),
                    document.display()
                );
            }
        }
        Ok(input.contract)
    }
}

fn check_reference_errors(errors: Vec<String>) -> Result<()> {
    if !errors.is_empty() {
        bail!(
            "source-reference check found {} issue(s):\n- {}\n\n{SOURCE_REFERENCE_GUIDANCE}",
            errors.len(),
            errors.join("\n- ")
        );
    }
    Ok(())
}

fn schema_root(repo: &Path) -> PathBuf {
    repo.join("openspec/schemas").join(SCHEMA)
}

fn hashes(repo: &Path, paths: &[PathBuf]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["hash-object", "--no-filters", "--"])
        .args(paths)
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        bail!(
            "cannot fingerprint protected supplied inputs: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let hashes = String::from_utf8(output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if hashes.len() != paths.len()
        || hashes
            .iter()
            .any(|hash| hash.len() < 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("incomplete supplied-input fingerprints");
    }
    Ok(hashes)
}

fn executable(path: &Path) -> Result<bool> {
    let metadata = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(false)
    }
}

fn decode_link(value: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut source = value.bytes();
    while let Some(byte) = source.next() {
        if byte == b'%' {
            let high = source.next().and_then(|b| (b as char).to_digit(16));
            let low = source.next().and_then(|b| (b as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                bail!("invalid escaped source link `{value}`")
            };
            bytes.push((high * 16 + low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    Ok(String::from_utf8(bytes)?)
}

fn has_fragment(text: &str, fragment: &str) -> bool {
    if fragment.is_empty() {
        return false;
    }
    // Exact IDs may be in tables or lists rather than headings.
    if text
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .any(|token| token == fragment)
    {
        return true;
    }
    let mut heading = None;
    for event in Parser::new(text) {
        match event {
            Event::Start(Tag::Heading { .. }) => heading = Some(String::new()),
            Event::Text(value) | Event::Code(value) => {
                if let Some(heading) = &mut heading {
                    heading.push_str(&value);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                let slug = heading
                    .take()
                    .unwrap_or_default()
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || c.is_whitespace() || *c == '-' || *c == '_')
                    .map(|c| if c.is_whitespace() { '-' } else { c })
                    .collect::<String>();
                if slug == fragment {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("opsx-contracts-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("openspec/changes/slice")).unwrap();
            fs::write(root.join("contract.md"), "# Object lifecycle\n\n## Create permits\nREQ-01: Respect expiry, including after deletion.\n").unwrap();
            fs::write(root.join("acceptance.bin"), [0, 255, 1]).unwrap();
            Self(root.canonicalize().unwrap())
        }
        fn inputs(&self) -> SuppliedContracts {
            SuppliedContracts::capture(&self.0, &["contract.md".into()], &["acceptance.bin".into()])
                .unwrap()
        }
        fn proposal(&self, reference: &str) -> PathBuf {
            let path = self.0.join("openspec/changes/slice/proposal.md");
            fs::write(&path, format!("## Purpose\nImplement the requirement.\n\n## Supplied requirements\n- [Requirement]({reference}) — task 1.1\n\n## Scope\nOwned work.\n")).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recorded_inputs_survive_resume_and_detect_contract_or_acceptance_changes() {
        let fixture = Fixture::new();
        let recorded: SuppliedContracts =
            serde_json::from_slice(&serde_json::to_vec(&fixture.inputs()).unwrap()).unwrap();
        recorded.check_selection(&fixture.0, &[], &[]).unwrap();
        recorded.check_inputs(&fixture.0).unwrap();
        fs::write(fixture.0.join("implementation.rs"), "unprotected work").unwrap();
        recorded.check_inputs(&fixture.0).unwrap();
        let original = fs::read(fixture.0.join("contract.md")).unwrap();
        fs::write(fixture.0.join("contract.md"), "Expiry restores authority").unwrap();
        assert!(
            recorded
                .check_inputs(&fixture.0)
                .unwrap_err()
                .to_string()
                .contains("protected supplied input changed")
        );
        fs::write(fixture.0.join("contract.md"), original).unwrap();
        fs::write(fixture.0.join("acceptance.bin"), [0, 255, 2]).unwrap();
        assert!(recorded.check_inputs(&fixture.0).is_err());
        assert!(
            recorded
                .check_selection(&fixture.0, &["contract.md".into()], &[])
                .is_err()
        );
    }

    #[test]
    fn supplied_inputs_must_be_existing_unique_nonempty_files() {
        let fixture = Fixture::new();
        assert!(SuppliedContracts::capture(&fixture.0, &[], &[]).is_err());
        assert!(SuppliedContracts::capture(&fixture.0, &["missing".into()], &[]).is_err());
        assert!(SuppliedContracts::capture(&fixture.0, &["openspec".into()], &[]).is_err());
        assert!(
            SuppliedContracts::capture(
                &fixture.0,
                &["contract.md".into()],
                &["contract.md".into()]
            )
            .is_err()
        );
        fs::write(fixture.0.join("empty.md"), " \n").unwrap();
        assert!(SuppliedContracts::capture(&fixture.0, &["empty.md".into()], &[]).is_err());
    }

    #[test]
    fn proposal_requires_correct_schema_and_original_source_references_without_deltas() {
        let fixture = Fixture::new();
        let inputs = fixture.inputs();
        for fragment in ["REQ-01", "create-permits"] {
            fixture.proposal(&format!("../../../contract.md#{fragment}"));
            inputs
                .check_proposal(&fixture.0, "slice", Some(SCHEMA))
                .unwrap();
        }
        assert!(
            inputs
                .check_proposal(&fixture.0, "slice", Some("spec-driven"))
                .is_err()
        );
        fixture.proposal("../../../contract.md#REQ-02");
        assert!(
            inputs
                .check_proposal(&fixture.0, "slice", Some(SCHEMA))
                .is_err()
        );
        fs::write(
            fixture.0.join("derived.md"),
            "REQ-01: a replacement requirement",
        )
        .unwrap();
        fixture.proposal("../../../derived.md#REQ-01");
        assert!(
            inputs
                .check_proposal(&fixture.0, "slice", Some(SCHEMA))
                .is_err()
        );
        fixture.proposal("../../../contract.md#REQ-01");
        fs::create_dir(fixture.0.join("openspec/changes/slice/specs")).unwrap();
        assert!(
            inputs
                .check_proposal(&fixture.0, "slice", Some(SCHEMA))
                .is_err()
        );
    }

    #[test]
    fn agenda_reports_all_bad_source_links_and_allows_separate_planning_navigation() {
        let fixture = Fixture::new();
        let inputs = fixture.inputs();
        let agenda = fixture.0.join("automation/slices");
        let archive = fixture
            .0
            .join("openspec/changes/archive/2026-09-28-bootstrap");
        fs::create_dir_all(&agenda).unwrap();
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("proposal.md"), "Derived plan").unwrap();
        let source = "## Supplied requirements\n- [REQ-01](../../contract.md#REQ-01)\n";
        fs::write(agenda.join("README.md"), source).unwrap();
        fs::write(
            agenda.join("0001-delivery.md"),
            format!("{source}- [Missing](../../contract.md#REQ-99)\n"),
        )
        .unwrap();
        let generated = "- [Bootstrap](../../openspec/changes/archive/2026-09-28-bootstrap/proposal.md)\n- [Coverage](README.md)\n- [Coverage again](README.md)\n";
        fs::write(
            agenda.join("9999-project-acceptance.md"),
            format!("{source}{generated}"),
        )
        .unwrap();
        let error = inputs.check_agenda(&fixture.0).unwrap_err().to_string();
        assert!(error.contains("4 issue(s)"), "{error}");
        for expected in [
            "0001-delivery.md",
            "REQ-99",
            "9999-project-acceptance.md",
            "archive/2026-09-28-bootstrap/proposal.md",
            "reference `README.md`",
            "## Planning references",
            "Correct all reported references together",
            "(line 3)",
            "(line 4)",
            "(line 5)",
        ] {
            assert!(error.contains(expected), "missing {expected}: {error}");
        }
        fs::write(agenda.join("0001-delivery.md"), source).unwrap();
        fs::write(
            agenda.join("9999-project-acceptance.md"),
            format!("{source}\n## Planning references\n{generated}"),
        )
        .unwrap();
        inputs.check_agenda(&fixture.0).unwrap();
        fs::write(
            agenda.join("9999-project-acceptance.md"),
            format!("## Supplied requirements\n{generated}\n## Planning references\n{source}"),
        )
        .unwrap();
        assert!(inputs.check_agenda(&fixture.0).is_err());
    }

    #[test]
    fn source_links_handle_spaces_but_examples_do_not_count_as_references() {
        let fixture = Fixture::new();
        fs::rename(
            fixture.0.join("contract.md"),
            fixture.0.join("contract with spaces.md"),
        )
        .unwrap();
        let inputs =
            SuppliedContracts::capture(&fixture.0, &["contract with spaces.md".into()], &[])
                .unwrap();
        let proposal = fixture.proposal("../../../contract%20with%20spaces.md#create-permits");
        inputs.check_references(&proposal).unwrap();
        fs::write(&proposal, "## Supplied requirements\n```markdown\n[Example](../../../contract%20with%20spaces.md)\n```\n").unwrap();
        assert!(inputs.check_references(&proposal).is_err());
    }

    #[test]
    fn workflow_templates_are_preserved_and_unrelated_schemas_are_untouched() {
        let fixture = Fixture::new();
        let inputs = fixture.inputs();
        fs::create_dir_all(fixture.0.join("openspec/schemas/other")).unwrap();
        inputs.install_schema(&fixture.0).unwrap();
        inputs.install_schema(&fixture.0).unwrap();
        fs::write(
            schema_root(&fixture.0).join("templates/proposal.md"),
            "Rewrite requirements",
        )
        .unwrap();
        assert!(inputs.check_schema(&fixture.0).is_err());
        assert!(inputs.install_schema(&fixture.0).is_err());
        assert!(fixture.0.join("openspec/schemas/other").exists());
    }

    #[cfg(unix)]
    #[test]
    fn changing_a_supplied_symlink_target_or_executable_bit_is_detected() {
        use std::os::unix::{fs::PermissionsExt, fs::symlink};
        let fixture = Fixture::new();
        symlink("contract.md", fixture.0.join("source.md")).unwrap();
        let inputs = SuppliedContracts::capture(&fixture.0, &["source.md".into()], &[]).unwrap();
        fs::copy(fixture.0.join("contract.md"), fixture.0.join("copy.md")).unwrap();
        fs::remove_file(fixture.0.join("source.md")).unwrap();
        symlink("copy.md", fixture.0.join("source.md")).unwrap();
        assert!(inputs.check_inputs(&fixture.0).is_err());
        let inputs = fixture.inputs();
        fs::set_permissions(
            fixture.0.join("acceptance.bin"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(inputs.check_inputs(&fixture.0).is_err());
    }
}
