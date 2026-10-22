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
pub(crate) const CONFIGURE_GUIDANCE: &str = "\n\nWorkflow choice: read the campaign context file (the selected --context, otherwise context.md if present) and its declared relevant inputs. When these supply precise existing behavioural contracts and component ownership, suggest --supplied-contracts and explain that it plans implementation through original source references, design and tasks without generating replacement specifications. For exploratory work that still develops requirements, suggest the ordinary workflow. Let the user accept the suggestion in the configuration conversation; never silently switch an existing choice. Record the agreed workflow, repeatable --contract PATH arguments naming the exact authoritative files, and optional --acceptance-file PATH arguments naming maintainer-owned checks/fixtures to preserve. Paths are relative to the campaign root; files must already exist, and directories/globs are not supported. Identify supplied acceptance expectations separately from tests the campaign should implement. Acceptance files are protected inputs; listing them does not execute checks. If input completeness or ownership is unclear, explain that uncertainty rather than assuming it. The actual runner uses only the recorded selection and never infers its workflow from context at launch. For new supplied-contract bootstrap/execute runs, also record --obligation-inventory PATH: an existing maintainer-owned version-1 JSON inventory of owned obligations, dependency owners, context and acceptance obligations (including builder-required tests). The inventory is protected automatically; never generate or rewrite it during configure/autoconfigure. Missing or ambiguous authoritative ownership needs a maintainer decision. The inventory format is documented in docs/supplied-assignments.md in opsx-build. Standalone supplied-contract changes need no agenda inventory. Disabling this option for a new run does not change the workflow recorded by an existing resumable run.";
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
    /// Absent in legacy checkpoints. Never inferred or added while resuming.
    #[serde(default)]
    pub obligation_inventory: Option<PathBuf>,
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
            obligation_inventory: None,
            acceptance: None,
        })
    }

    pub fn capture_with_inventory(
        repo: &Path,
        contracts: &[PathBuf],
        acceptance: &[PathBuf],
        inventory: Option<&Path>,
    ) -> Result<Self> {
        let mut paths = contracts.to_vec();
        if let Some(path) = inventory {
            let resolved = repo.join(path).canonicalize()?;
            if !paths
                .iter()
                .any(|p| repo.join(p).canonicalize().ok().as_ref() == Some(&resolved))
            {
                paths.push(path.to_path_buf());
            }
        }
        let mut selected = Self::capture(repo, &paths, acceptance)?;
        // Read the captured target, even when the inventory was also declared
        // through another alias. An unregistered alias must not change authority.
        selected.obligation_inventory =
            inventory.map(|p| repo.join(p).canonicalize()).transpose()?;
        selected.inventory(repo)?;
        Ok(selected)
    }

    pub fn check_inventory_selection(&self, repo: &Path, inventory: Option<&Path>) -> Result<()> {
        if let Some(path) = inventory
            && self
                .obligation_inventory
                .as_ref()
                .map(|p| p.canonicalize())
                .transpose()?
                != Some(repo.join(path).canonicalize()?)
        {
            bail!(
                "obligation inventory differs from the checkpoint; start a new run instead of migrating on resume"
            );
        }
        Ok(())
    }

    fn inventory(&self, repo: &Path) -> Result<Option<crate::assignment::Inventory>> {
        let Some(path) = &self.obligation_inventory else {
            return Ok(None);
        };
        let inventory: crate::assignment::Inventory = crate::assignment::read(path)?;
        let own_path = path.canonicalize()?;
        inventory.validate(|source| {
            let file = source.split('#').next().unwrap();
            if repo.join(decode_link(file)?).canonicalize()? == own_path {
                bail!("inventory entries must reference original inputs, not the obligation inventory itself");
            }
            self.check_reference(&repo.join("inventory-reference"), source)
        })?;
        Ok(Some(inventory))
    }

    pub fn assignment_views(&self, repo: &Path, render: bool) -> Result<()> {
        self.check_inputs(repo)?;
        let inventory = self.inventory(repo)?.context("this checkpoint uses the legacy workflow and has no obligation inventory; no migration is performed")?;
        let assignment: crate::assignment::Assignment =
            crate::assignment::read(&repo.join(crate::assignment::RECORD))?;
        let views = assignment.views(repo, &inventory, self.acceptance.as_ref())?;
        // Do not let a generated output replace any declared supplied input.
        for (relative, _) in &views {
            if repo
                .join(relative)
                .canonicalize()
                .ok()
                .is_some_and(|p| self.inputs.iter().any(|i| i.resolved == p))
            {
                bail!("generated view `{relative}` is a protected supplied input");
            }
        }
        crate::assignment::update_views(repo, &views, render)?;
        self.check_inputs(repo)
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
        let selected = Self::capture_with_inventory(
            repo,
            contracts,
            acceptance,
            self.obligation_inventory.as_deref(),
        )?;
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
        if let Some(inventory) = &self.obligation_inventory {
            if let Ok(executable) = std::env::current_exe() {
                let command = crate::process::shell_quote(&executable.to_string_lossy());
                text.push_str(&format!("\nUse the running runner version for agenda commands: {command} --no-config --no-campaign-config render-agenda (or check-agenda). A different installed binary may not support this workflow.\n"));
            }
            text.push_str(&format!("\n\nASSIGNMENT WORKFLOW v1\nThe protected maintainer-owned obligation inventory is `{}`. Its entries define selected ownership and acceptance obligations, subject to the original contracts and their precedence. Never edit it, invent an authoritative inventory, change an evidence kind, or infer ownership from shared context. Every complete source reference inherits its conditions and exceptions within the supplied boundary.\n\nThe derived work assignment is `{}`. During bootstrap Propose draft it at `{}`; do not create the agenda yet. Apply moves it to its durable path, authors concise slice briefs, and runs `opsx-build render-agenda`. Keep proposal/design/tasks at decomposition level and reference the assignment from a Planning references section; do not duplicate the complete coverage map.\n\nThe JSON format is {{\"version\":1,\"slices\":[{{\"path\":\"0001-delivery.md\",\"requires\":[]}}],\"obligations\":[{{\"id\":\"<inventory obligation ID>\",\"accountable\":\"0001-delivery.md\",\"contributors\":[]}}],\"acceptance\":[{{\"id\":\"<inventory acceptance ID>\",\"slices\":[\"0001-delivery.md\"],\"route\":\"How real assertions or required review establish this obligation\"}}]}}. Include all owned and dependency entries; context entries are not delivery assignments. An obligation may have contributions across slices, with one accountable slice. Include every acceptance entry; builder tests must be delivered before 9999. Every slice has assigned work. Prerequisites must refer to earlier slices; several internal steps can be tasks within one coherent slice. The terminal `9999-project-acceptance.md` is implicit and must exist on disk; do not put it in the slices array. Acceptance routes may include it. Do not introduce invented IDs.\n\n`render-agenda` is a deterministic authoring command: it generates the README, Supplied requirements and Prerequisites sections, and the terminal Required Tests and Project Goal Coverage sections from this record and the pinned runner commands. Author slice titles, Objective, Acceptance Criteria and delivery Required Tests as concise completion boundaries and acceptance routes. Leave implementation/test mechanisms to delivery, except source-mandated mechanisms and decisions necessary for decomposition. Rendering is required after bootstrap Apply/Repair, any affected agenda repair, subdivision and terminal remediation, before committing. In terminal remediation, retain all previous participants and add the remediation as a contribution or acceptance route; rendering must leave 9999 unchanged. Do not render during Verify; run `opsx-build check-agenda` to check the existing views read-only. The runner rejects stale or incomplete assignments.\n\nIndependent semantic review must still reject omitted scope, changed conditions, transferred ownership, impossible dependencies and weakened acceptance. Cite the governing source, defective assignment/prescription and concrete consequence for every blocker. Complete assignment by reference does not require repeated prose conditions; a citation or a test name alone does not prove a correct scope split or real acceptance coverage. Strict structural/source checks still apply. Input ambiguities require BLOCKED; generated mistakes require RETRY.\n", inventory.display(), crate::assignment::RECORD, crate::assignment::DRAFT));
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
        self.check_references(&root.join("proposal.md"))?;
        if change == crate::bootstrap::BOOTSTRAP_CHANGE
            && let Some(inventory) = self.inventory(repo)?
        {
            // Apply has already moved the draft when later stages revisit this boundary.
            let draft = root.join("assignments.json");
            let path = if draft.exists() {
                draft
            } else {
                repo.join(crate::assignment::RECORD)
            };
            let assignment: crate::assignment::Assignment = crate::assignment::read(&path)?;
            assignment.validate(&inventory)?;
        }
        Ok(())
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
        let references = check_reference_errors(errors);
        if self.obligation_inventory.is_some() {
            let assignment = if repo.join(crate::assignment::DRAFT).exists() {
                Err(anyhow::anyhow!(
                    "bootstrap draft assignments.json still exists; move it to the durable assignment instead of maintaining two records"
                ))
            } else {
                self.assignment_views(repo, false)
            };
            if let Err(error) = assignment {
                return match references {
                    Ok(()) => Err(error.context("assignment structural check failed")),
                    Err(references) => Err(anyhow::anyhow!(
                        "{references:#}\n\nAssignment structural check: {error:#}"
                    )),
                };
            }
        }
        references
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
                    "unresolved requirement/section or line reference `{fragment}` in `{}` (referenced by `{}`)",
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
    // Source links commonly identify a line or an inclusive line range.
    if let Some(lines) = fragment.strip_prefix('L') {
        let (start, end) = lines.split_once("-L").unwrap_or((lines, lines));
        if start.bytes().all(|b| b.is_ascii_digit()) && end.bytes().all(|b| b.is_ascii_digit()) {
            return start
                .parse::<usize>()
                .ok()
                .zip(end.parse::<usize>().ok())
                .is_some_and(|(start, end)| {
                    start > 0 && start <= end && end <= text.lines().count()
                });
        }
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

    #[cfg(unix)]
    #[test]
    fn inventory_uses_the_protected_target_when_also_declared_through_an_alias() {
        let f = Fixture::new();
        fs::write(
            f.0.join("contract.md"),
            include_str!("../tests/fixtures/assignment-workflow/contract.md"),
        )
        .unwrap();
        fs::write(
            f.0.join("inventory.json"),
            include_str!("../tests/fixtures/assignment-workflow/inventory.json"),
        )
        .unwrap();
        fs::write(
            f.0.join("acceptance.sh"),
            include_str!("../tests/fixtures/assignment-workflow/acceptance.sh"),
        )
        .unwrap();
        std::os::unix::fs::symlink("inventory.json", f.0.join("inventory-alias.json")).unwrap();
        let contracts = SuppliedContracts::capture_with_inventory(
            &f.0,
            &["contract.md".into(), "inventory.json".into()],
            &["acceptance.sh".into()],
            Some(Path::new("inventory-alias.json")),
        )
        .unwrap();
        fs::write(f.0.join("other.json"), "{}\n").unwrap();
        fs::remove_file(f.0.join("inventory-alias.json")).unwrap();
        std::os::unix::fs::symlink("other.json", f.0.join("inventory-alias.json")).unwrap();
        contracts.check_inputs(&f.0).unwrap();
        assert_eq!(
            contracts.inventory(&f.0).unwrap().unwrap().component,
            "example"
        );
        assert!(
            contracts
                .check_inventory_selection(&f.0, Some(Path::new("inventory-alias.json")))
                .is_err()
        );
    }

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
    fn source_line_references_require_real_lines_in_declared_inputs() {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.join("acceptance.go"),
            "package acceptance\nfunc TestContract() {\n}\n",
        )
        .unwrap();
        let inputs = SuppliedContracts::capture(
            &fixture.0,
            &["contract.md".into()],
            &["acceptance.go".into()],
        )
        .unwrap();
        let proposal = fixture.0.join("openspec/changes/slice/proposal.md");
        for (fragment, valid) in [
            ("L1", true),
            ("L2", true),
            ("L3", true),
            ("L1-L3", true),
            ("L2-L2", true),
            ("TestContract", true),
            ("L0", false),
            ("L4", false),
            ("L2-L1", false),
            ("L0-L2", false),
            ("L2-L4", false),
            ("L999999999999999999999999999999", false),
            ("L", false),
            ("L1-L", false),
            ("L1-L2-L3", false),
            ("L+1", false),
        ] {
            fs::write(&proposal, format!("## Supplied requirements\n- [Contract](../../../contract.md#REQ-01)\n- [Test](../../../acceptance.go#{fragment})\n")).unwrap();
            assert_eq!(
                inputs.check_references(&proposal).is_ok(),
                valid,
                "{fragment}"
            );
        }
        fs::copy(
            fixture.0.join("acceptance.go"),
            fixture.0.join("undeclared.go"),
        )
        .unwrap();
        assert!(
            inputs
                .check_reference(&proposal, "../../../undeclared.go#L2")
                .is_err()
        );
        assert!(!has_fragment("", "L1"));
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
