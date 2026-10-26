//! Maintainer-owned obligations and the derived, durable work assignment.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{acceptance::AcceptanceGate, agenda::parse_slice_name};

pub(crate) const RECORD: &str = "automation/slices/assignments.json";
pub(crate) const DRAFT: &str = "openspec/changes/bootstrap-implementation-slices/assignments.json";
const FINAL: &str = "9999-project-acceptance.md";
const DIRECTORY: &str = "automation/slices";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Inventory {
    pub version: u32,
    pub component: String,
    pub obligations: Vec<Obligation>,
    pub acceptance: Vec<Acceptance>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Role {
    Owned,
    Dependency,
    Context,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Obligation {
    pub id: String,
    /// Relative to the campaign root, including an optional source fragment.
    pub source: String,
    pub role: Role,
    /// Required for dependency contracts; owned work belongs to `component`.
    #[serde(default)]
    pub owner: Option<String>,
    /// A maintainer-supplied reading or ownership boundary, not generated scope.
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EvidenceKind {
    SuppliedTest,
    BuilderTest,
    Review,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Acceptance {
    pub id: String,
    pub source: String,
    pub kind: EvidenceKind,
    pub obligations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Assignment {
    pub version: u32,
    pub slices: Vec<Slice>,
    pub obligations: Vec<Allocation>,
    pub acceptance: Vec<Route>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Slice {
    /// A delivery filename, without automation/slices/. The terminal is implicit.
    pub path: String,
    pub requires: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Allocation {
    pub id: String,
    pub accountable: String,
    pub contributors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Route {
    pub id: String,
    pub slices: Vec<String>,
    /// Explains the real evidence route; it cannot change the inventory's kind.
    pub route: String,
}

pub(crate) fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("cannot read `{}`", path.display()))?,
    )
    .with_context(|| format!("invalid assignment-workflow JSON in `{}`", path.display()))
}

fn identifier(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn require_unique<'a>(values: impl IntoIterator<Item = &'a str>, label: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            bail!("duplicate {label} `{value}`");
        }
    }
    Ok(())
}

impl Inventory {
    pub fn validate(&self, mut source: impl FnMut(&str) -> Result<bool>) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported obligation inventory version {}; expected 1",
                self.version
            );
        }
        if self.component.trim().is_empty() {
            bail!("inventory component must be nonempty");
        }
        require_unique(
            self.obligations
                .iter()
                .map(|o| o.id.as_str())
                .chain(self.acceptance.iter().map(|a| a.id.as_str())),
            "inventory ID",
        )?;
        if !self.obligations.iter().any(|o| o.role == Role::Owned) {
            bail!("inventory must select at least one owned obligation");
        }
        for obligation in &self.obligations {
            if !identifier(&obligation.id) {
                bail!("invalid obligation ID `{}`", obligation.id);
            }
            if !source(&obligation.source)? {
                bail!(
                    "obligation `{}` must reference a declared contract",
                    obligation.id
                );
            }
            if obligation.role == Role::Dependency
                && obligation
                    .owner
                    .as_ref()
                    .is_none_or(|s| s.trim().is_empty() || s == &self.component)
            {
                bail!(
                    "dependency `{}` needs a different component's owner",
                    obligation.id
                );
            }
            if obligation.role == Role::Owned
                && obligation
                    .owner
                    .as_ref()
                    .is_some_and(|s| s != &self.component)
            {
                bail!(
                    "owned obligation `{}` cannot transfer ownership",
                    obligation.id
                );
            }
        }
        for check in &self.acceptance {
            if !identifier(&check.id) || check.obligations.is_empty() {
                bail!(
                    "acceptance `{}` needs a valid ID and governed obligations",
                    check.id
                );
            }
            let contract = source(&check.source)?;
            if check.kind == EvidenceKind::SuppliedTest && contract {
                bail!(
                    "supplied test `{}` must reference a protected --acceptance-file",
                    check.id
                );
            }
            require_unique(
                check.obligations.iter().map(String::as_str),
                "acceptance obligation",
            )?;
            for id in &check.obligations {
                if !self
                    .obligations
                    .iter()
                    .any(|o| o.id == *id && o.role != Role::Context)
                {
                    bail!(
                        "acceptance `{}` refers to unknown or context-only obligation `{id}`",
                        check.id
                    );
                }
            }
        }
        for obligation in self.obligations.iter().filter(|o| o.role == Role::Owned) {
            if !self
                .acceptance
                .iter()
                .any(|a| a.obligations.contains(&obligation.id))
            {
                bail!(
                    "owned obligation `{}` has no acceptance obligation",
                    obligation.id
                );
            }
        }
        Ok(())
    }
}

impl Assignment {
    /// Recovery may redistribute the failed slice, but cannot quietly reassign
    /// completed work. Later slices may add prerequisites on new subdivision
    /// children. Terminal remediation only adds contributions/routes.
    pub fn check_recovery(&self, previous: &Self, failed_slice: Option<&str>) -> Result<()> {
        let subdivision = failed_slice
            .and_then(parse_slice_name)
            .map(|(parent, _, _)| {
                let children = self
                    .slices
                    .iter()
                    .filter(|slice| !previous.slices.iter().any(|old| old.path == slice.path))
                    .filter_map(|slice| {
                        let (ordinal, _, _) = parse_slice_name(&slice.path)?;
                        (ordinal.len() > parent.len() && ordinal.starts_with(&parent))
                            .then_some(slice.path.as_str())
                    })
                    .collect::<BTreeSet<_>>();
                (parent, children)
            });
        for old in &previous.slices {
            let current = self
                .slices
                .iter()
                .find(|s| s.path == old.path)
                .with_context(|| format!("recovery removed existing slice `{}`", old.path))?;
            if Some(old.path.as_str()) != failed_slice && old.requires != current.requires {
                let adds_child_prerequisites =
                    subdivision.as_ref().is_some_and(|(parent, children)| {
                        parse_slice_name(&old.path).is_some_and(|(ordinal, _, _)| ordinal > *parent)
                            && current
                                .requires
                                .iter()
                                .filter(|path| !children.contains(path.as_str()))
                                .eq(old.requires.iter())
                    });
                if !adds_child_prerequisites {
                    bail!(
                        "recovery changed prerequisites of unaffected slice `{}`; only later slices may add newly introduced subdivision children while retaining existing prerequisites",
                        old.path
                    );
                }
            }
        }
        for old in &previous.obligations {
            let current = self
                .obligations
                .iter()
                .find(|a| a.id == old.id)
                .with_context(|| format!("recovery removed obligation `{}`", old.id))?;
            if Some(old.accountable.as_str()) != failed_slice
                && current.accountable != old.accountable
            {
                bail!(
                    "recovery reassigned accountable completed work for `{}`",
                    old.id
                );
            }
            for path in &old.contributors {
                if Some(path.as_str()) != failed_slice
                    && !current.contributors.contains(path)
                    && current.accountable != *path
                {
                    bail!("recovery dropped an existing contribution for `{}`", old.id);
                }
            }
        }
        for old in &previous.acceptance {
            let current = self
                .acceptance
                .iter()
                .find(|r| r.id == old.id)
                .with_context(|| format!("recovery removed acceptance `{}`", old.id))?;
            for path in &old.slices {
                if Some(path.as_str()) != failed_slice && !current.slices.contains(path) {
                    bail!(
                        "recovery dropped an existing acceptance route for `{}`",
                        old.id
                    );
                }
            }
        }
        Ok(())
    }

    pub fn validate(&self, inventory: &Inventory) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported assignment version {}; expected 1",
                self.version
            );
        }
        if self.slices.is_empty() {
            bail!("assignment needs at least one delivery slice");
        }
        require_unique(self.slices.iter().map(|s| s.path.as_str()), "slice")?;
        let mut ordinals = BTreeSet::new();
        let mut order = BTreeMap::new();
        for slice in &self.slices {
            let (ordinal, ordinal_text, _) = parse_slice_name(&slice.path)
                .with_context(|| format!("invalid delivery filename `{}`", slice.path))?;
            if slice.path.contains('/')
                || ordinal_text.split('.').next().unwrap().len() != 4
                || ordinal[0] >= 9999
                || !ordinals.insert(ordinal.clone())
            {
                bail!(
                    "invalid, terminal or duplicate delivery ordinal `{}`",
                    slice.path
                );
            }
            order.insert(slice.path.as_str(), ordinal);
        }
        for slice in &self.slices {
            require_unique(slice.requires.iter().map(String::as_str), "prerequisite")?;
            for dependency in &slice.requires {
                let prior = order.get(dependency.as_str()).with_context(|| {
                    format!(
                        "slice `{}` has unknown prerequisite `{dependency}`",
                        slice.path
                    )
                })?;
                if prior >= &order[slice.path.as_str()] {
                    bail!(
                        "prerequisite `{dependency}` must precede `{}`; forward dependencies and cycles are invalid",
                        slice.path
                    );
                }
            }
        }
        require_unique(
            self.obligations.iter().map(|a| a.id.as_str()),
            "allocated obligation",
        )?;
        for allocation in &self.obligations {
            if !inventory
                .obligations
                .iter()
                .any(|o| o.id == allocation.id && o.role != Role::Context)
            {
                bail!(
                    "unknown or context-only allocated obligation `{}`",
                    allocation.id
                );
            }
            require_unique(
                std::iter::once(allocation.accountable.as_str())
                    .chain(allocation.contributors.iter().map(String::as_str)),
                "obligation participant",
            )?;
            for path in std::iter::once(&allocation.accountable).chain(&allocation.contributors) {
                if !order.contains_key(path.as_str()) {
                    bail!(
                        "obligation `{}` references unknown delivery slice `{path}`",
                        allocation.id
                    );
                }
            }
        }
        for obligation in inventory
            .obligations
            .iter()
            .filter(|o| o.role != Role::Context)
        {
            if !self.obligations.iter().any(|a| a.id == obligation.id) {
                bail!("unassigned obligation `{}`", obligation.id);
            }
        }
        require_unique(
            self.acceptance.iter().map(|r| r.id.as_str()),
            "acceptance route",
        )?;
        for route in &self.acceptance {
            let check = inventory
                .acceptance
                .iter()
                .find(|a| a.id == route.id)
                .with_context(|| format!("unknown acceptance route `{}`", route.id))?;
            if route.route.trim().is_empty() || route.slices.is_empty() {
                bail!(
                    "acceptance `{}` needs slices and a nonempty evidence route",
                    route.id
                );
            }
            require_unique(route.slices.iter().map(String::as_str), "acceptance slice")?;
            for path in &route.slices {
                if path != FINAL && !order.contains_key(path.as_str()) {
                    bail!(
                        "acceptance `{}` references unknown slice `{path}`",
                        route.id
                    );
                }
            }
            if check.kind == EvidenceKind::BuilderTest && route.slices.iter().all(|s| s == FINAL) {
                bail!(
                    "builder test `{}` must be delivered before terminal acceptance",
                    route.id
                );
            }
        }
        for check in &inventory.acceptance {
            if !self.acceptance.iter().any(|r| r.id == check.id) {
                bail!("unassigned acceptance obligation `{}`", check.id);
            }
        }
        for slice in &self.slices {
            if !self
                .obligations
                .iter()
                .any(|a| a.accountable == slice.path || a.contributors.contains(&slice.path))
                && !self
                    .acceptance
                    .iter()
                    .any(|r| r.slices.contains(&slice.path))
            {
                bail!("slice `{}` has no assigned work", slice.path);
            }
        }
        Ok(())
    }

    /// Build every output before writing any, so malformed assignments cannot
    /// partially replace the existing views. Checking uses these same bytes.
    pub fn views(
        &self,
        repo: &Path,
        inventory: &Inventory,
        gate: Option<&AcceptanceGate>,
    ) -> Result<Vec<(String, String)>> {
        self.validate(inventory)?;
        let mut slices = self.slices.iter().collect::<Vec<_>>();
        slices.sort_by_key(|s| parse_slice_name(&s.path).unwrap().0);
        let expected = slices
            .iter()
            .map(|s| s.path.clone())
            .chain(std::iter::once(FINAL.to_owned()))
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        for entry in fs::read_dir(repo.join(DIRECTORY))? {
            let name = entry?
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("agenda contains a non-UTF-8 filename"))?;
            if name.ends_with(".md") && name != "README.md" {
                actual.insert(name);
            }
        }
        if actual != expected {
            bail!(
                "assignment delivery files differ from the agenda: expected {expected:?}, found {actual:?}"
            );
        }
        let mut outputs = Vec::new();
        let mut readme = format!(
            "# {} implementation agenda\n\nGenerated from `assignments.json` and the protected obligation inventory.\nEdit the assignment and run `opsx-build render-agenda`; do not edit this view.\n\n## Execution order\n\n",
            escape(&inventory.component)
        );
        for path in slices
            .iter()
            .map(|s| s.path.as_str())
            .chain(std::iter::once(FINAL))
        {
            let relative = format!("{DIRECTORY}/{path}");
            let document = fs::read_to_string(repo.join(&relative))?;
            for heading in [
                "## Objective",
                "## Acceptance Criteria",
                "## Required Tests",
            ] {
                if !document.lines().any(|line| line.trim_end() == heading) {
                    bail!("`{relative}` omitted author-owned heading `{heading}`");
                }
            }
            let title = document
                .lines()
                .find_map(|l| l.strip_prefix("# "))
                .with_context(|| format!("`{relative}` needs a level-one title"))?;
            readme.push_str(&format!("- [{}]({path})\n", escape(title)));
            let mut ids = self
                .obligations
                .iter()
                .filter(|a| {
                    path == FINAL
                        || a.accountable == path
                        || a.contributors.iter().any(|s| s == path)
                })
                .map(|a| a.id.as_str())
                .collect::<BTreeSet<_>>();
            let checks = inventory
                .acceptance
                .iter()
                .filter(|a| {
                    path == FINAL
                        || self
                            .acceptance
                            .iter()
                            .any(|r| r.id == a.id && r.slices.iter().any(|s| s == path))
                })
                .collect::<Vec<_>>();
            for check in &checks {
                ids.extend(check.obligations.iter().map(String::as_str));
            }
            let mut sources = String::from(
                "A complete reference inherits the source's conditions and exceptions within its supplied ownership boundary.\n\n",
            );
            for obligation in inventory
                .obligations
                .iter()
                .filter(|o| ids.contains(o.id.as_str()))
            {
                sources.push_str(&obligation_line(obligation, inventory));
            }
            for check in checks {
                sources.push_str(&source_line(&check.id, &check.source));
            }
            let mut rendered = section(&document, "Supplied requirements", &sources)?;
            let prerequisites = if path == FINAL {
                "All delivery and remediation work in the [assignment coverage](README.md#assignment-coverage) must be complete.\n".to_owned()
            } else {
                let slice = slices.iter().find(|s| s.path == path).unwrap();
                if slice.requires.is_empty() {
                    "None.\n".to_owned()
                } else {
                    slice
                        .requires
                        .iter()
                        .map(|s| format!("- [{s}]({s})\n"))
                        .collect()
                }
            };
            rendered = section(&rendered, "Prerequisites", &prerequisites)?;
            if path == FINAL {
                rendered = section(&rendered, "Required Tests", &terminal_commands(gate))?;
                rendered = section(
                    &rendered,
                    "Project Goal Coverage",
                    "Satisfy every owned obligation and acceptance obligation in the [generated assignment coverage](README.md#assignment-coverage), including integration across slices. Consumed dependency contracts retain their supplied ownership. The pinned commands establish their actual coverage; they cannot waive other supplied obligations.\n",
                )?;
            }
            outputs.push((relative, rendered));
        }
        readme.push_str("\n## Supplied requirements\n\n");
        for obligation in &inventory.obligations {
            readme.push_str(&obligation_line(obligation, inventory));
        }
        for check in &inventory.acceptance {
            readme.push_str(&source_line(&check.id, &check.source));
        }
        readme.push_str("\n## Assignment coverage\n\n| Obligation | Role / owner | Accountable slice | Contributions | Supplied boundary |\n| --- | --- | --- | --- | --- |\n");
        for obligation in &inventory.obligations {
            let allocation = self.obligations.iter().find(|a| a.id == obligation.id);
            readme.push_str(&format!(
                "| {} | {} / {} | {} | {} | {} |\n",
                obligation.id,
                match obligation.role {
                    Role::Owned => "owned",
                    Role::Dependency => "dependency",
                    Role::Context => "context",
                },
                escape(obligation.owner.as_deref().unwrap_or(&inventory.component)),
                allocation.map_or("Context only".to_owned(), |a| slice_link(&a.accountable)),
                allocation.map_or(String::new(), |a| a
                    .contributors
                    .iter()
                    .map(|p| slice_link(p))
                    .collect::<Vec<_>>()
                    .join(", ")),
                escape(
                    obligation
                        .scope
                        .as_deref()
                        .unwrap_or("Complete source reference")
                )
            ));
        }
        readme.push_str("\nEvery owned obligation is also checked at the terminal gate.\n\n| Acceptance obligation | Evidence kind | Assigned slices | Evidence route |\n| --- | --- | --- | --- |\n");
        for check in &inventory.acceptance {
            let route = self.acceptance.iter().find(|r| r.id == check.id).unwrap();
            readme.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                check.id,
                match check.kind {
                    EvidenceKind::SuppliedTest => "supplied test",
                    EvidenceKind::BuilderTest => "builder test",
                    EvidenceKind::Review => "review",
                },
                route
                    .slices
                    .iter()
                    .map(|p| slice_link(p))
                    .collect::<Vec<_>>()
                    .join(", "),
                escape(&route.route)
            ));
        }
        outputs.push((format!("{DIRECTORY}/README.md"), readme));
        Ok(outputs)
    }
}

fn slice_link(path: &str) -> String {
    format!("[{path}]({path})")
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "&#124;")
        .replace('[', "&#91;")
        .replace(']', "&#93;")
        .replace(['\n', '\r'], " ")
}
fn source_line(id: &str, source: &str) -> String {
    // Sources are campaign-root-relative; every generated view is two levels down.
    let prefix = if Path::new(source.split('#').next().unwrap()).is_absolute() {
        ""
    } else {
        "../../"
    };
    let escaped = source
        .replace('<', "%3C")
        .replace('>', "%3E")
        .replace('\n', "%0A")
        .replace('\r', "%0D");
    format!("- [{id}](<{prefix}{escaped}>)\n")
}

fn obligation_line(obligation: &Obligation, inventory: &Inventory) -> String {
    let source = source_line(&obligation.id, &obligation.source);
    let role = match obligation.role {
        Role::Owned => format!("owned by {}", inventory.component),
        Role::Dependency => format!(
            "consume dependency owned by {}",
            obligation.owner.as_deref().unwrap()
        ),
        Role::Context => "context only".to_owned(),
    };
    let scope = obligation
        .scope
        .as_ref()
        .map(|s| format!("; {s}"))
        .unwrap_or_default();
    format!(
        "{} — {}{}\n",
        source.trim_end(),
        escape(&role),
        escape(&scope)
    )
}

fn terminal_commands(gate: Option<&AcceptanceGate>) -> String {
    let Some(gate) = gate else {
        return "No runner-executed commands were selected. Independently verify all supplied acceptance obligations; no command selection is a claim of complete coverage.\n".to_owned();
    };
    let mut text = format!(
        "The runner independently executes these pinned commands from the campaign root, in order, with {} seconds per command. Preserve their real exit statuses.\n",
        gate.timeout_seconds
    );
    for command in &gate.commands {
        let fence = "`".repeat(
            command
                .split(|c| c != '`')
                .map(str::len)
                .max()
                .unwrap_or(0)
                .max(2)
                + 1,
        );
        text.push_str(&format!("\n{fence}sh\n{command}\n{fence}\n"));
    }
    text
}

/// Replace only runner-owned level-two sections; code fences are excluded by
/// the Markdown parser. Other author-written content is preserved.
fn section(document: &str, name: &str, body: &str) -> Result<String> {
    use pulldown_cmark::{Event, HeadingLevel, Parser, Tag};
    let headings = Parser::new(document)
        .into_offset_iter()
        .filter_map(|(event, span)| match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1 | HeadingLevel::H2,
                ..
            }) => Some(span),
            _ => None,
        })
        .collect::<Vec<_>>();
    let matching = headings
        .iter()
        .enumerate()
        .filter(|(_, span)| {
            document[span.start..span.end]
                .lines()
                .next()
                .is_some_and(|s| s.trim_end() == format!("## {name}"))
        })
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        bail!("duplicate runner-owned section `## {name}`");
    }
    let replacement = format!("## {name}\n\n{}\n\n", body.trim_end());
    match matching.first() {
        Some((index, span)) => {
            let end = headings.get(index + 1).map_or(document.len(), |h| h.start);
            Ok(format!(
                "{}{}{}",
                &document[..span.start],
                replacement,
                &document[end..]
            ))
        }
        None => Ok(format!("{}\n\n{replacement}", document.trim_end())),
    }
}

pub(crate) fn update_views(repo: &Path, views: &[(String, String)], render: bool) -> Result<()> {
    if fs::symlink_metadata(repo.join(DIRECTORY))?
        .file_type()
        .is_symlink()
    {
        bail!("agenda directory must not be a symlink");
    }
    // Reject redirects before writing anything, including redirects to protected inputs.
    for (relative, _) in views {
        let path = repo.join(relative);
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (!metadata.is_file() || metadata.file_type().is_symlink())
        {
            bail!("generated view `{relative}` must be a regular file");
        }
        if path.parent().unwrap().canonicalize()? != repo.join(DIRECTORY).canonicalize()? {
            bail!("generated view `{relative}` escaped the agenda directory");
        }
    }
    for (relative, contents) in views {
        let path = repo.join(relative);
        if fs::read_to_string(&path).ok().as_deref() != Some(contents) {
            if render {
                let temporary =
                    path.with_extension(format!("opsx-render-{}", uuid::Uuid::new_v4()));
                fs::write(&temporary, contents)?;
                fs::rename(&temporary, &path)?;
            } else {
                bail!(
                    "generated view `{relative}` is stale; authoring stages must run `opsx-build render-agenda` before verification or committing an agenda update"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const INVENTORY: &str = include_str!("../tests/fixtures/assignment-workflow/inventory.json");
    const ASSIGNMENT: &str = include_str!("../tests/fixtures/assignment-workflow/assignments.json");
    fn inventory() -> Inventory {
        serde_json::from_str(INVENTORY).unwrap()
    }
    fn assignment() -> Assignment {
        serde_json::from_str(ASSIGNMENT).unwrap()
    }
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("opsx-assignment-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join(DIRECTORY)).unwrap();
            for name in ["0001-delivery.md", FINAL] {
                fs::write(root.join(DIRECTORY).join(name), "# Delivery\n\n## Objective\nImplement complete assigned sources.\n\n## Acceptance Criteria\nSatisfy their full conditions and exceptions.\n\n## Required Tests\nAdd real assertions for the assigned observations.\n").unwrap();
            }
            Self(root)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rejects_incomplete_inventories_and_unowned_dependencies() {
        for (field, value, expected) in [
            ("version", json!(2), "unsupported"),
            ("acceptance", json!([]), "no acceptance"),
            ("obligations", json!([]), "owned obligation"),
        ] {
            let mut raw: Value = serde_json::from_str(INVENTORY).unwrap();
            raw[field] = value;
            let inventory: Inventory = serde_json::from_value(raw).unwrap();
            assert!(
                inventory
                    .validate(|s| Ok(!s.ends_with(".sh")))
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
        let mut inv = inventory();
        inv.obligations[2].owner = None;
        assert!(
            inv.validate(|s| Ok(!s.ends_with(".sh")))
                .unwrap_err()
                .to_string()
                .contains("owner")
        );
        let mut inv = inventory();
        inv.acceptance[0].kind = EvidenceKind::SuppliedTest;
        assert!(
            inv.validate(|_| Ok(true))
                .unwrap_err()
                .to_string()
                .contains("--acceptance-file")
        );
        inventory().validate(|s| Ok(!s.ends_with(".sh"))).unwrap();
    }

    #[test]
    fn rejects_lost_scope_routes_and_invalid_dependency_order() {
        let baseline: Value = serde_json::from_str(ASSIGNMENT).unwrap();
        let mut cases = Vec::new();
        let mut raw = baseline.clone();
        raw["obligations"].as_array_mut().unwrap().remove(0);
        cases.push((raw, "unassigned obligation"));
        let mut raw = baseline.clone();
        raw["acceptance"].as_array_mut().unwrap().remove(0);
        cases.push((raw, "unassigned acceptance"));
        let mut raw = baseline.clone();
        raw["obligations"][0]["id"] = json!("CONTEXT-01");
        cases.push((raw, "context-only"));
        let mut raw = baseline.clone();
        raw["obligations"][0]["accountable"] = json!(FINAL);
        cases.push((raw, "unknown delivery"));
        let mut raw = baseline.clone();
        raw["slices"][0]["requires"] = json!(["0001-delivery.md"]);
        cases.push((raw, "cycles"));
        let mut raw = baseline.clone();
        raw["slices"][0]["requires"] = json!(["0002-later.md"]);
        cases.push((raw, "unknown prerequisite"));
        let mut raw = baseline.clone();
        raw["acceptance"][1]["slices"] = json!([FINAL]);
        cases.push((raw, "before terminal"));
        let mut raw = baseline.clone();
        raw["acceptance"][1]["route"] = json!(" ");
        cases.push((raw, "nonempty"));
        let mut raw = baseline.clone();
        raw["slices"]
            .as_array_mut()
            .unwrap()
            .push(baseline["slices"][0].clone());
        cases.push((raw, "duplicate slice"));
        for (raw, expected) in cases {
            let record: Assignment = serde_json::from_value(raw).unwrap();
            let error = record.validate(&inventory()).unwrap_err().to_string();
            assert!(error.contains(expected), "{error}; expected {expected}");
        }
        let mut raw = baseline;
        raw["acceptance"][0]["kind"] = json!("review");
        assert!(serde_json::from_value::<Assignment>(raw).is_err());
    }

    #[test]
    fn renders_complete_sources_and_exact_pinned_commands_idempotently() {
        let f = Fixture::new();
        let gate = AcceptanceGate::new(
            vec![
                "printf '%s\\n' 'literal $() and ```'\nsh acceptance.sh".into(),
                "sh second.sh".into(),
            ],
            Some(42),
        );
        let outputs = assignment()
            .views(&f.0, &inventory(), gate.as_ref())
            .unwrap();
        update_views(&f.0, &outputs, true).unwrap();
        let second = assignment()
            .views(&f.0, &inventory(), gate.as_ref())
            .unwrap();
        assert_eq!(outputs, second);
        update_views(&f.0, &second, false).unwrap();
        let terminal = fs::read_to_string(f.0.join(DIRECTORY).join(FINAL)).unwrap();
        for command in &gate.as_ref().unwrap().commands {
            assert!(terminal.contains(command));
        }
        assert!(terminal.contains("42 seconds"));
        assert!(terminal.contains("````sh"));
        let brief = fs::read_to_string(f.0.join(DIRECTORY).join("0001-delivery.md")).unwrap();
        assert!(brief.contains("../../contract.md#OWN-01"));
        assert!(brief.contains("Add real assertions"));
        assert!(!brief.contains("A verified result includes a profile"));
        fs::write(
            f.0.join(DIRECTORY).join(FINAL),
            terminal.replace("sh second.sh", "true"),
        )
        .unwrap();
        let stale = assignment()
            .views(&f.0, &inventory(), gate.as_ref())
            .unwrap();
        assert!(
            update_views(&f.0, &stale, false)
                .unwrap_err()
                .to_string()
                .contains("stale")
        );
        // Read-only validation leaves the weakened view unchanged.
        assert!(
            !fs::read_to_string(f.0.join(DIRECTORY).join(FINAL))
                .unwrap()
                .contains("sh second.sh")
        );
    }

    #[test]
    fn remediation_updates_coverage_without_changing_the_terminal() {
        let f = Fixture::new();
        let old = assignment();
        let outputs = old.views(&f.0, &inventory(), None).unwrap();
        update_views(&f.0, &outputs, true).unwrap();
        let terminal = fs::read(f.0.join(DIRECTORY).join(FINAL)).unwrap();
        let mut new = old.clone();
        new.slices.push(Slice {
            path: "0002-remediation.md".into(),
            requires: vec!["0001-delivery.md".into()],
        });
        new.obligations[0]
            .contributors
            .push("0002-remediation.md".into());
        new.acceptance[1].slices.push("0002-remediation.md".into());
        fs::copy(
            f.0.join(DIRECTORY).join("0001-delivery.md"),
            f.0.join(DIRECTORY).join("0002-remediation.md"),
        )
        .unwrap();
        new.check_recovery(&old, None).unwrap();
        let views = new.views(&f.0, &inventory(), None).unwrap();
        update_views(&f.0, &views, true).unwrap();
        assert_eq!(terminal, fs::read(f.0.join(DIRECTORY).join(FINAL)).unwrap());
        let mut lost = new.clone();
        lost.obligations[0].accountable = "0002-remediation.md".into();
        assert!(lost.check_recovery(&old, None).is_err());
        lost.check_recovery(&old, Some("0001-delivery.md")).unwrap();
        let mut lost = new;
        lost.acceptance[0].slices.clear();
        assert!(lost.check_recovery(&old, None).is_err());
    }

    #[test]
    fn subdivision_allows_only_new_child_prerequisites_on_later_slices() {
        for (failed, child, later) in [
            (
                "0002-recovery.md",
                "0002.1-accounting.md",
                "0003-bindings.md",
            ),
            (
                "0002.1-recovery.md",
                "0002.1.1-accounting.md",
                "0002.2-bindings.md",
            ),
        ] {
            let mut old = assignment();
            old.slices.push(Slice {
                path: failed.into(),
                requires: vec!["0001-delivery.md".into()],
            });
            old.slices.push(Slice {
                path: later.into(),
                requires: vec!["0001-delivery.md".into(), failed.into()],
            });
            old.obligations[0]
                .contributors
                .extend([failed.into(), later.into()]);
            old.validate(&inventory()).unwrap();
            let mut new = old.clone();
            new.slices.push(Slice {
                path: child.into(),
                requires: vec![failed.into()],
            });
            new.obligations[0].contributors.push(child.into());
            new.slices[2].requires.push(child.into());
            new.validate(&inventory()).unwrap();
            new.check_recovery(&old, Some(failed)).unwrap();
            assert!(new.check_recovery(&old, None).is_err()); // No subdivision allowance for terminal remediation.

            let mut invalid = new.clone();
            invalid.slices[2].requires.retain(|path| path != failed);
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
            let mut invalid = new.clone();
            invalid.slices[2].requires.push("0001-delivery.md".into());
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
            let mut invalid = new.clone();
            invalid.slices[0].requires.push(child.into());
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
            let mut invalid = new.clone();
            invalid.slices.push(Slice {
                path: "0001.1-unrelated.md".into(),
                requires: vec![],
            });
            invalid.slices[2]
                .requires
                .push("0001.1-unrelated.md".into());
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
            let mut invalid = new.clone();
            invalid.slices[2].requires.push("0002.8-missing.md".into());
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
            let mut existing = new.clone();
            existing.slices[2].requires = old.slices[2].requires.clone();
            existing.validate(&inventory()).unwrap();
            // An existing child is not a newly introduced prerequisite.
            assert!(new.check_recovery(&existing, Some(failed)).is_err());
            let mut invalid = new.clone();
            invalid.obligations[0].accountable = child.into();
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
            let mut invalid = new.clone();
            invalid.obligations[0]
                .contributors
                .retain(|path| path != later);
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
            let mut invalid = new;
            invalid.acceptance[0].slices.clear();
            assert!(invalid.check_recovery(&old, Some(failed)).is_err());
        }
    }

    #[test]
    fn rejects_missing_files_and_symlinked_views_before_writing() {
        let f = Fixture::new();
        let outputs = assignment().views(&f.0, &inventory(), None).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                f.0.join(DIRECTORY).join("0001-delivery.md"),
                f.0.join(DIRECTORY).join("README.md"),
            )
            .unwrap();
            let old = fs::read(f.0.join(DIRECTORY).join("0001-delivery.md")).unwrap();
            assert!(update_views(&f.0, &outputs, true).is_err());
            assert_eq!(
                old,
                fs::read(f.0.join(DIRECTORY).join("0001-delivery.md")).unwrap()
            );
            fs::remove_file(f.0.join(DIRECTORY).join("README.md")).unwrap();
        }
        fs::remove_file(f.0.join(DIRECTORY).join("0001-delivery.md")).unwrap();
        assert!(assignment().views(&f.0, &inventory(), None).is_err());
    }
}
