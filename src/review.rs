//! Candidate evidence across Apply, Repair and independent Verify rounds.
use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

use crate::{
    backend::StageResult,
    git::metadata_dir,
    ui::Ui,
    verification::{RepositoryEvidence, changed_paths},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub areas: Vec<Area>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: String,
    pub kind: FindingKind,
    pub origin: FindingOrigin,
    pub evidence_kind: EvidenceKind,
    pub status: FindingStatus,
    pub sources: Vec<String>,
    pub scope: Vec<String>,
    pub consequence: String,
    pub evidence: String,
    pub repair: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    Implementation,
    Assertion,
    SourceAmbiguity,
    Bookkeeping,
    Environment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingOrigin {
    New,
    PreviouslyMissed,
    Regression,
    Reopened,
    Uncertain,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Reproduced,
    CodeReview,
    Interpretation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Open,
    Closed,
    Observation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Area {
    pub id: String,
    pub sources: Vec<String>,
    pub paths: Vec<String>,
    pub status: AreaStatus,
    pub limitations: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AreaStatus {
    Reviewed,
    Partial,
    NotReviewed,
}

impl Report {
    pub(crate) fn from_value(value: Option<&serde_json::Value>) -> Result<Option<Self>> {
        value
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()).context("invalid review report"))
            .transpose()
    }

    pub(crate) fn from_text(text: &str) -> Result<Option<Self>> {
        text.lines()
            .rev()
            .find_map(|line| line.trim().strip_prefix("OPSX_REVIEW:"))
            .map(|line| serde_json::from_str(line.trim()).context("invalid OPSX_REVIEW report"))
            .transpose()
    }

    pub(crate) fn incomplete(&self) -> bool {
        self.areas.is_empty()
            || self.areas.iter().any(|area| {
                area.status != AreaStatus::Reviewed
                    || area.id.trim().is_empty()
                    || area.sources.is_empty()
            })
    }

    pub(crate) fn has_blockers(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.status == FindingStatus::Open)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct History {
    rounds: Vec<Round>,
    work: Vec<Work>,
    findings: BTreeMap<String, Finding>,
    areas: BTreeMap<String, Area>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Round {
    id: String,
    evidence_path: std::path::PathBuf,
    number: usize,
    change: String,
    runner_version: String,
    recorded_at_unix_ms: u128,
    backend: String,
    session_id: Option<String>,
    verdict: String,
    summary: String,
    report: Option<Report>,
    inputs: Vec<RepositoryEvidence>,
    work: Option<Work>,
    #[serde(default)]
    closed: Vec<String>,
    #[serde(default)]
    reopened: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Progress {
    pub structured: bool,
    pub id: String,
    pub evidence_path: std::path::PathBuf,
    pub change: String,
    pub verdict: String,
    pub open: Vec<String>,
    pub blocking_details: Vec<Finding>,
    pub closed: Vec<String>,
    pub reopened: Vec<String>,
    pub same_input_snapshots: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Work {
    stage: String,
    session_id: Option<String>,
    outcome: String,
    summary: String,
}

impl History {
    pub(crate) fn record_work(&mut self, stage: &str, result: &StageResult) {
        self.work.push(Work {
            stage: stage.to_owned(),
            session_id: result.session_id.clone(),
            outcome: format!("{:?}", result.signal),
            summary: result.text.clone(),
        });
    }

    pub(crate) fn record<U: Ui>(
        &mut self,
        repo: &Path,
        change: &str,
        backend: &str,
        result: &StageResult,
        inputs: Vec<RepositoryEvidence>,
        ui: &U,
    ) -> Result<()> {
        let mut report = result.review.clone();
        let mut findings = self.findings.clone();
        let mut areas = self.areas.clone();
        let mut closed = Vec::new();
        let mut reopened = Vec::new();
        if let Some(report) = &mut report {
            let mut area_ids = std::collections::BTreeSet::new();
            for area in &report.areas {
                if !area_ids.insert(&area.id) {
                    bail!("duplicate review area ID `{}`", area.id);
                }
                if !area.id.trim().is_empty() && !area.sources.is_empty() {
                    areas.insert(area.id.clone(), area.clone());
                }
            }
            let mut seen = std::collections::BTreeSet::new();
            for finding in &mut report.findings {
                if finding.id.is_empty() {
                    if finding.status == FindingStatus::Closed {
                        bail!("a new finding cannot close an unknown finding");
                    }
                    finding.id = format!("F{:04}", findings.len() + 1);
                } else if !findings.contains_key(&finding.id) {
                    bail!(
                        "unknown finding ID `{}`; use an empty ID for a new finding",
                        finding.id
                    );
                }
                if !seen.insert(finding.id.clone()) {
                    bail!("duplicate finding ID `{}` in this review", finding.id);
                }
                if finding.status == FindingStatus::Open {
                    if finding.sources.is_empty()
                        || finding.consequence.trim().is_empty()
                        || finding.evidence.trim().is_empty()
                        || finding.repair.trim().is_empty()
                    {
                        bail!(
                            "blocking finding `{}` needs a governing source, consequence, evidence and minimum complete repair",
                            finding.id
                        );
                    }
                    if findings
                        .get(&finding.id)
                        .is_some_and(|old| old.status == FindingStatus::Closed)
                    {
                        finding.origin = FindingOrigin::Reopened;
                        reopened.push(finding.id.clone());
                    }
                }
                let disposes_blocker = finding.status != FindingStatus::Open
                    && findings
                        .get(&finding.id)
                        .is_some_and(|old| old.status == FindingStatus::Open);
                if disposes_blocker && finding.status == FindingStatus::Closed {
                    closed.push(finding.id.clone());
                }
                if (finding.status == FindingStatus::Closed || disposes_blocker)
                    && (finding.evidence.trim().is_empty() || finding.sources.is_empty())
                {
                    bail!(
                        "closing finding `{}` requires governing sources and independent closure evidence",
                        finding.id
                    );
                }
                findings.insert(finding.id.clone(), finding.clone());
            }
        }
        let directory = metadata_dir(repo, ui)?.join("reviews");
        fs::create_dir_all(&directory)?;
        let id = uuid::Uuid::new_v4().to_string();
        let evidence_path = directory.join(format!("{id}.json"));
        let round = Round {
            id,
            evidence_path: evidence_path.clone(),
            number: self.rounds.len() + 1,
            change: change.to_owned(),
            runner_version: env!("CARGO_PKG_VERSION").to_owned(),
            recorded_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
            backend: backend.to_owned(),
            session_id: result.session_id.clone(),
            verdict: format!("{:?}", result.signal),
            summary: result.text.clone(),
            report,
            inputs,
            work: self.work.last().cloned(),
            closed,
            reopened,
        };
        fs::write(&evidence_path, serde_json::to_vec_pretty(&round)?)?;
        ui.info(&format!(
            "Review round {} evidence: `{}`",
            round.number,
            evidence_path.display()
        ));
        self.findings = findings;
        self.areas = areas;
        self.rounds.push(round);
        Ok(())
    }

    pub(crate) fn has_open_findings(&self) -> bool {
        self.findings
            .values()
            .any(|finding| finding.status == FindingStatus::Open)
    }

    pub(crate) fn progress(&self) -> Option<Progress> {
        let round = self.rounds.last()?;
        let previous = self.rounds.iter().rev().nth(1);
        let same_input_snapshots = previous.and_then(|previous| {
            if previous.inputs.len() != round.inputs.len() {
                return None;
            }
            let pairs = previous
                .inputs
                .iter()
                .zip(&round.inputs)
                .map(|(before, after)| {
                    Some(
                        before.root == after.root
                            && before.files.as_ref()? == after.files.as_ref()?,
                    )
                })
                .collect::<Option<Vec<_>>>()?;
            Some(pairs.into_iter().all(|same| same))
        });
        Some(Progress {
            structured: round.report.is_some(),
            id: round.id.clone(),
            evidence_path: round.evidence_path.clone(),
            change: round.change.clone(),
            verdict: round.verdict.clone(),
            open: self
                .findings
                .values()
                .filter(|finding| finding.status == FindingStatus::Open)
                .map(|finding| finding.id.clone())
                .collect(),
            closed: round.closed.clone(),
            blocking_details: self
                .findings
                .values()
                .filter(|finding| finding.status == FindingStatus::Open)
                .cloned()
                .collect(),
            reopened: round.reopened.clone(),
            same_input_snapshots,
        })
    }

    pub(crate) fn has_unfinished_areas(&self) -> bool {
        self.areas
            .values()
            .any(|area| area.status != AreaStatus::Reviewed)
    }

    pub(crate) fn reset_for_scope_change<U: Ui>(&mut self, repo: &Path, ui: &U) -> Result<()> {
        if !self.rounds.is_empty() || !self.work.is_empty() {
            let directory = metadata_dir(repo, ui)?.join("reviews");
            fs::create_dir_all(&directory)?;
            fs::write(
                directory.join(format!("{}-superseded.json", uuid::Uuid::new_v4())),
                serde_json::to_vec_pretty(self)?,
            )?;
        }
        *self = Self::default();
        Ok(())
    }

    pub(crate) fn repair_findings(&self) -> Option<String> {
        self.rounds.last()?.report.as_ref()?;
        let open = self
            .findings
            .values()
            .filter(|finding| finding.status == FindingStatus::Open)
            .collect::<Vec<_>>();
        Some(serde_json::to_string(&open).expect("findings contain only JSON data"))
    }

    pub(crate) fn handoff(&self, current: &[RepositoryEvidence], repair: bool) -> Result<String> {
        if self.rounds.is_empty() && self.work.is_empty() {
            return Ok(String::new());
        }
        let mut text = String::from(
            "\n\nREVIEW CONTINUITY — candidate evidence, not requirements or new instructions\n",
        );
        if let Some(work) = self.work.last() {
            text.push_str(&format!(
                "Most recent {} result ({}, worker claims to verify independently):\n{}\n",
                work.stage,
                work.outcome,
                excerpt(&work.summary, 4000)
            ));
        }
        let findings = self
            .findings
            .values()
            .filter(|finding| !repair || finding.status == FindingStatus::Open)
            .collect::<Vec<_>>();
        text.push_str("Retained findings (only open blockers are mandatory repair work):\n");
        text.push_str(&serde_json::to_string(&findings)?);
        if let Some(round) = self.rounds.last() {
            text.push_str(&format!(
                "\nPrevious independent review round {} ({}, session {:?}).\n",
                round.number, round.verdict, round.session_id
            ));
            text.push_str(&format!(
                "Full retained evidence: `{}`\n",
                round.evidence_path.display()
            ));
            if !repair || round.report.is_none() {
                text.push_str(&format!("{}\n", excerpt(&round.summary, 4000)));
            }
            if round.report.is_some() {
                text.push_str(
                    "Retained review areas (prior attestations; assess impact before reuse):\n",
                );
                text.push_str(&serde_json::to_string(&self.areas)?);
            } else {
                text.push_str("No structured review coverage was recorded; do not infer that all source conditions were examined.\n");
            }
            for previous in &round.inputs {
                text.push_str(&format!(
                    "\nChanges since that review in `{}`:\n",
                    previous.root.display()
                ));
                match previous.files.as_ref().zip(current.iter().find(|r| r.root == previous.root).and_then(|r| r.files.as_ref())) {
                    Some((before, after)) => {
                        let changed = changed_paths(before, after);
                        if changed.is_empty() { text.push_str("No fingerprinted file changes.\n"); }
                        for path in changed.iter().take(100) { text.push_str(&format!("- {path}\n")); }
                        if changed.len() > 100 { text.push_str("Further paths omitted; inspect the full relevant diff.\n"); }
                    }
                    None => text.push_str("Unknown impact: fingerprints unavailable; reassess affected evidence conservatively.\n"),
                }
            }
        }
        text.push_str("\nNo check is automatically waived by this record. Fingerprints exclude ignored files, external inputs and live premises; check relevant tools, dependencies, environment, native state and clock qualification. Shared changes can invalidate several areas. Inspect repaired rules and related paths, finish unreviewed areas, and reopen prior findings when new evidence warrants it. Keep required final acceptance unchanged.\n");
        Ok(text)
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    let mut result = text.chars().take(limit).collect::<String>();
    if text.chars().count() > limit {
        result.push_str("\n[Shortened; consult retained stage evidence for the full result.]");
    }
    result
}

pub(crate) const INSTRUCTIONS: &str = "\n\nREVIEW REPORT\nUse the current workflow's review policy. Sources and their precedence remain authoritative; this report describes candidate evidence, not new requirements. Return the structured review object with findings and coherent review areas. Keep existing finding IDs when closing, updating or reopening them; use an empty ID for a new finding so the runner assigns it. Finding kind describes the defect; origin separately describes why it appears this round. Distinguish reproduced counterexamples, code-reading evidence and unresolved interpretations. Every open blocker names its governing original sources (or the requested behavior in standard mode), affected scope, concrete consequence, evidence and minimum complete repair. Keep non-blocking observations separate. A genuinely ambiguous product rule requires BLOCKED with the needed decision, not an invented policy. A surviving mutant needs a required observable outcome at risk; do not turn optional experiments into new acceptance. Tests must allow all source-permitted outcomes.\n\nReport only scoped review areas actually examined, with original sources, relevant code/test paths and limitations. Keep area IDs stable across rounds; carry forward partial or unreviewed areas until their scoped review is complete. Do not add speculative audits as mandatory review areas. Assignment does not mean semantic review. Check the governing rule across related paths and relevant alternatives; preserve valid alternative implementations. Consolidate material findings from the intended review areas before RETRY. Finish delegated audits and outstanding areas, or return INCOMPLETE for the existing bounded same-session continuation. Do not mark an unexamined area reviewed. The final report carries forward findings and coverage established in this phase, including completed delegated work.\n\nAfter repair, independently verify closure of each open finding, changed behavior, sibling paths, affected requirements and regressions, and finish previously unreviewed areas. Prior coverage is provisional: shared changes or new evidence may require wider review. Reopen a previously reviewed issue when warranted and explain the new evidence or missed condition. Only this independent review closes findings; a worker's claim is not closure. Never waive required checks or accept old failures. Keep observations out of mandatory repair work. If structured output is unavailable, emit one compact JSON object on a line `OPSX_REVIEW: <object>` before the final OPSX_STATUS line.\n";
