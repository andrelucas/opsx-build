//! Decisions about continuing work, never decisions about product acceptance.
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    backend::StageResult, git::metadata_dir, review::Progress, ui::Ui, usage::UsageRecord,
};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Policy {
    pub max_no_progress_reviews: u32,
    pub max_recurring_finding_reviews: u32,
    pub max_requests_per_stage: u32,
    pub max_replans_without_verified_progress: u32,
    pub max_request_seconds: u64,
    pub max_active_seconds: Option<u64>,
    pub acknowledge_unresolved_requests: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_no_progress_reviews: 2,
            max_recurring_finding_reviews: 2,
            max_requests_per_stage: 12,
            max_replans_without_verified_progress: 2,
            max_request_seconds: 3600,
            max_active_seconds: None,
            acknowledge_unresolved_requests: Vec::new(),
        }
    }
}

impl Policy {
    pub(crate) fn read(path: &Path) -> Result<Self> {
        let policy: Self = serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("invalid operating policy {}", path.display()))?;
        if policy.max_no_progress_reviews == 0
            || policy.max_recurring_finding_reviews == 0
            || policy.max_requests_per_stage == 0
            || policy.max_replans_without_verified_progress == 0
            || policy.max_request_seconds == 0
            || policy.max_active_seconds == Some(0)
        {
            bail!("operating-policy limits must be positive");
        }
        Ok(policy)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Run {
    directory: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Request {
    started_at_unix_ms: u64,
    activity: String,
    context: crate::usage::UsageContext,
    backend: String,
    session: String,
    dispatches: u32,
    outcome: Option<String>,
    elapsed_ms: u64,
    reported_cost_usd: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Ledger {
    format_version: u32,
    policy: Policy,
    revisions: Vec<Policy>,
    active_ms: u64,
    active_time_has_gap: bool,
    requests: BTreeMap<String, Request>,
    stage_key: String,
    stage_requests: u32,
    change: Option<String>,
    next_stage: String,
    last_verified: Option<String>,
    reviews: Vec<Progress>,
    #[serde(default)]
    incomplete_review: Option<Progress>,
    #[serde(default)]
    unfinished_stage: Option<String>,
    #[serde(default)]
    summary_timing: crate::run_summary::Timing,
    no_progress: u32,
    unassessable: u32,
    recurrences: BTreeMap<String, u32>,
    replans_without_progress: u32,
    unresolved: Vec<String>,
    cancellation_notes: Vec<String>,
}

/// Owned for the runner process lifetime; serialized independently of stage state.
pub struct Guard {
    directory: PathBuf,
    ledger: Mutex<Ledger>,
    active_since: Instant,
    saved_active_ms: u64,
    request_started: Mutex<Option<Instant>>,
    failure: Mutex<Option<String>>,
    owner_lock: File,
}

#[derive(Debug)]
pub struct PolicyStop(String);
impl std::fmt::Display for PolicyStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for PolicyStop {}

pub(crate) fn is_stop(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PolicyStop>().is_some()
}

impl Run {
    pub(crate) fn new<U: Ui>(repo: &Path, policy: Policy, ui: &U) -> Result<Self> {
        let directory = metadata_dir(repo, ui)?
            .join("policy")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&directory)?;
        write(
            &directory.join("ledger.json"),
            &Ledger {
                format_version: 1,
                policy,
                revisions: Vec::new(),
                active_ms: 0,
                active_time_has_gap: false,
                requests: BTreeMap::new(),
                stage_key: String::new(),
                stage_requests: 0,
                change: None,
                next_stage: String::new(),
                last_verified: None,
                reviews: Vec::new(),
                incomplete_review: None,
                unfinished_stage: None,
                summary_timing: crate::run_summary::Timing::start(),
                no_progress: 0,
                unassessable: 0,
                recurrences: BTreeMap::new(),
                replans_without_progress: 0,
                unresolved: Vec::new(),
                cancellation_notes: Vec::new(),
            },
        )?;
        Ok(Self { directory })
    }

    pub(crate) fn open(&self, update: Option<Policy>) -> Result<Guard> {
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.directory.join("owner.lock"))?;
        lock.try_lock()
            .context("another process owns this operating-policy run")?;
        let mut ledger: Ledger =
            serde_json::from_slice(&fs::read(self.directory.join("ledger.json"))?)
                .context("operating-policy ledger unavailable; cannot safely reset its history")?;
        let acknowledgements = update
            .as_ref()
            .map(|policy| policy.acknowledge_unresolved_requests.clone())
            .unwrap_or_default();
        if let Some(policy) = update
            && policy != ledger.policy
        {
            ledger.revisions.push(ledger.policy.clone());
            ledger.policy = policy;
        }
        for (id, request) in &ledger.requests {
            if request.outcome.is_none() && !ledger.unresolved.contains(id) {
                ledger.unresolved.push(id.clone());
                ledger.active_time_has_gap = true;
            }
        }
        let interrupted_changes = ledger
            .requests
            .values()
            .filter(|request| request.outcome.is_none())
            .map(|request| request.context.change.clone())
            .collect::<Vec<_>>();
        for change in interrupted_changes {
            ledger.summary_timing.interrupted(change.as_deref());
        }
        for id in acknowledgements {
            if ledger.unresolved.contains(&id)
                && let Some(request) = ledger.requests.get_mut(&id)
            {
                request.outcome =
                    Some("reconciled_on_explicit_resume; cost remains unknown".into());
                ledger.unresolved.retain(|pending| *pending != id);
            }
        }
        let guard = Guard {
            directory: self.directory.clone(),
            saved_active_ms: ledger.active_ms,
            ledger: Mutex::new(ledger),
            active_since: Instant::now(),
            request_started: Mutex::new(None),
            failure: Mutex::new(None),
            owner_lock: lock,
        };
        guard.save()?;
        Ok(guard)
    }
}

impl Guard {
    /// Explicit maintainer correction grants another local attempt, preserving
    /// all requests, reviews, usage and unresolved backend operations.
    pub(crate) fn accept_changes(&self) -> Result<()> {
        {
            let mut ledger = self.ledger.lock().unwrap();
            if !ledger.unresolved.is_empty() {
                bail!(
                    "a backend request is still unresolved; establish that it has stopped before accepting changed inputs"
                );
            }
            ledger.stage_key.clear();
            ledger.stage_requests = 0;
            ledger.no_progress = 0;
            ledger.unassessable = 0;
            ledger.recurrences.clear();
            ledger.replans_without_progress = 0;
            ledger.incomplete_review = None;
            ledger.unfinished_stage = None;
        }
        self.save()
    }

    fn elapsed_ms(&self) -> u64 {
        self.saved_active_ms.saturating_add(
            self.active_since
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        )
    }

    fn save(&self) -> Result<()> {
        let mut ledger = self.ledger.lock().unwrap();
        ledger.active_ms = self.elapsed_ms();
        write(&self.directory.join("ledger.json"), &*ledger)
    }

    pub(crate) fn boundary(&self, change: Option<&str>, next_stage: &str) -> Result<()> {
        {
            let mut ledger = self.ledger.lock().unwrap();
            let known_start = !ledger
                .requests
                .values()
                .any(|request| request.context.change.as_deref() == change);
            ledger
                .summary_timing
                .boundary(change, next_stage, self.elapsed_ms(), known_start);
            if ledger.change.as_deref() != change
                || !ledger
                    .next_stage
                    .replace('-', " ")
                    .eq_ignore_ascii_case(next_stage)
            {
                ledger.unfinished_stage = None;
                ledger.incomplete_review = None;
            }
            ledger.change = change.map(str::to_owned);
            ledger.next_stage = next_stage.to_owned();
        }
        self.save()?;
        self.check(false)
    }

    pub(crate) fn before_request(&self, usage: &UsageRecord) -> Result<()> {
        self.check(true)?;
        let key = format!(
            "{:?}:{:?}:{:?}",
            usage.context.iteration, usage.context.change, usage.context.stage
        );
        {
            let mut ledger = self.ledger.lock().unwrap();
            if ledger.stage_key != key {
                ledger.stage_key = key;
                ledger.stage_requests = 0;
            }
            if ledger.stage_requests >= ledger.policy.max_requests_per_stage {
                drop(ledger);
                return self.stop("request_limit", "the stage exhausted its request allowance without reaching its next durable stage result");
            }
            ledger.stage_requests += 1;
            ledger.summary_timing.finished_at_unix_ms = None;
            ledger.summary_timing.finished_active_ms = None;
            let request = ledger
                .requests
                .entry(usage.id.clone())
                .or_insert_with(|| Request {
                    started_at_unix_ms: usage.started_at_unix_ms,
                    activity: usage.activity.clone(),
                    context: usage.context.clone(),
                    backend: usage.backend.clone(),
                    session: usage.session_id.clone(),
                    dispatches: 0,
                    outcome: None,
                    elapsed_ms: 0,
                    reported_cost_usd: None,
                });
            request.dispatches += 1;
            request.outcome = None;
        }
        *self.request_started.lock().unwrap() = Some(Instant::now());
        self.save()
    }

    pub(crate) fn record_usage(&self, usage: &UsageRecord) {
        {
            let mut ledger = self.ledger.lock().unwrap();
            if let Some(request) = ledger.requests.get_mut(&usage.id) {
                request.session = usage.session_id.clone();
                request.outcome = Some(usage.outcome.clone());
                request.elapsed_ms = usage.elapsed_ms;
                request.reported_cost_usd = usage
                    .reported_cost_usd
                    .filter(|cost| cost.is_finite() && *cost >= 0.0);
            }
        }
        *self.request_started.lock().unwrap() = None;
        if let Err(error) = self.save() {
            *self.failure.lock().unwrap() =
                Some(format!("could not preserve policy accounting: {error:#}"));
        }
    }

    pub(crate) fn incomplete_stage(&self, result: &StageResult) -> Result<()> {
        let mut remaining = result.text.clone();
        if let Some(report) = &result.review {
            remaining.push('\n');
            remaining.push_str(&report.unfinished_work());
        }
        self.ledger.lock().unwrap().unfinished_stage = Some(remaining);
        self.save()
    }

    pub(crate) fn checkpoint_path(&self) -> PathBuf {
        self.directory
            .parent()
            .and_then(Path::parent)
            .expect("policy metadata root")
            .join("last-run.json")
    }

    pub(crate) fn change_elapsed_seconds(&self, change: &str) -> Option<u64> {
        let ledger = self.ledger.lock().unwrap();
        ledger
            .summary_timing
            .elapsed_seconds(change, self.elapsed_ms())
    }

    pub(crate) fn summary_work_started(&self) {
        let mut ledger = self.ledger.lock().unwrap();
        ledger.summary_timing.finished_at_unix_ms = None;
        ledger.summary_timing.finished_active_ms = None;
    }

    pub(crate) fn summary_context(&self, complete: bool) -> Result<crate::run_summary::Context> {
        let active_ms = self.elapsed_ms();
        let mut ledger = self.ledger.lock().unwrap();
        if complete {
            ledger
                .summary_timing
                .finished_at_unix_ms
                .get_or_insert_with(crate::run_summary::now_ms);
            ledger
                .summary_timing
                .finished_active_ms
                .get_or_insert(active_ms);
        }
        let context = crate::run_summary::Context {
            directory: self.directory.clone(),
            timing: ledger.summary_timing.clone(),
            active_ms,
            active_time_has_gap: ledger.active_time_has_gap,
            requests: ledger
                .requests
                .iter()
                .map(|(id, request)| {
                    (
                        id.clone(),
                        crate::run_summary::Request {
                            backend: request.backend.clone(),
                            context: request.context.clone(),
                            elapsed_ms: request.elapsed_ms,
                            outcome: request.outcome.clone(),
                            dispatches: request.dispatches,
                        },
                    )
                })
                .collect(),
        };
        drop(ledger);
        self.save()?;
        Ok(context)
    }

    pub(crate) fn incomplete_review(&self, mut progress: Progress) -> Result<()> {
        let mut ledger = self.ledger.lock().unwrap();
        if let Some(previous) = &ledger.incomplete_review
            && previous.change == progress.change
        {
            if previous.id == progress.id {
                return Ok(());
            }
            for id in &previous.closed {
                if !progress.closed.contains(id) {
                    progress.closed.push(id.clone());
                }
            }
        }
        for id in &progress.reopened {
            *ledger.recurrences.entry(id.clone()).or_default() += 1;
        }
        ledger.incomplete_review = Some(progress);
        drop(ledger);
        self.save()
    }

    pub(crate) fn review(&self, mut progress: Progress) -> Result<()> {
        let mut ledger = self.ledger.lock().unwrap();
        if ledger.reviews.iter().any(|old| old.id == progress.id) {
            return Ok(());
        }
        if let Some(pending) = ledger.incomplete_review.take()
            && pending.change == progress.change
        {
            for id in pending.closed {
                if !progress.closed.contains(&id) {
                    progress.closed.push(id);
                }
            }
        }
        let same_change = ledger
            .reviews
            .last()
            .is_some_and(|previous| previous.change == progress.change);
        if !same_change {
            ledger.no_progress = 0;
            ledger.unassessable = 0;
            ledger.recurrences.clear();
        }
        let comparable =
            progress.structured && ledger.reviews.last().is_some_and(|prior| prior.structured);
        if !comparable {
            ledger.no_progress = 0;
        }
        if progress.verdict == "Retry"
            && same_change
            && !progress.structured
            && ledger.reviews.last().is_some_and(|prior| !prior.structured)
        {
            ledger.unassessable += 1;
        } else {
            ledger.unassessable = 0;
        }
        if progress.verdict == "Retry" && same_change && comparable && progress.closed.is_empty() {
            ledger.no_progress += 1;
        } else if !progress.closed.is_empty() || progress.verdict == "Verified" {
            ledger.no_progress = 0;
        }
        for id in &progress.reopened {
            *ledger.recurrences.entry(id.clone()).or_default() += 1;
        }
        ledger.reviews.push(progress);
        drop(ledger);
        self.save()
    }

    pub(crate) fn verified(&self, milestone: String) -> Result<()> {
        let mut ledger = self.ledger.lock().unwrap();
        ledger.last_verified = Some(milestone);
        ledger.replans_without_progress = 0;
        drop(ledger);
        self.save()
    }

    pub(crate) fn before_replan(&self) -> Result<()> {
        self.check(true)?;
        let mut ledger = self.ledger.lock().unwrap();
        if ledger.replans_without_progress >= ledger.policy.max_replans_without_verified_progress {
            drop(ledger);
            return self.stop("replan_limit", "repeated decomposition has produced no independently verified milestone; reassess the recovery strategy before another rollback");
        }
        ledger.replans_without_progress += 1;
        drop(ledger);
        self.save()
    }

    pub(crate) fn poll(&self) -> Result<()> {
        let request_elapsed = self
            .request_started
            .lock()
            .unwrap()
            .map(|started| started.elapsed());
        if request_elapsed.is_some_and(|elapsed| {
            elapsed >= Duration::from_secs(self.ledger.lock().unwrap().policy.max_request_seconds)
        }) {
            return self.stop("request_timeout", "the active model request exceeded its time allowance; its stage is incomplete and requires diagnosis before another attempt");
        }
        self.check(false)
    }

    pub(crate) fn remaining_request_time(&self) -> Duration {
        let ledger = self.ledger.lock().unwrap();
        let request = self
            .request_started
            .lock()
            .unwrap()
            .map(|started| started.elapsed())
            .unwrap_or_default();
        let mut remaining =
            Duration::from_secs(ledger.policy.max_request_seconds).saturating_sub(request);
        if let Some(limit) = ledger.policy.max_active_seconds {
            remaining = remaining.min(
                Duration::from_secs(limit).saturating_sub(Duration::from_millis(self.elapsed_ms())),
            );
        }
        remaining
    }

    pub(crate) fn cancellation_unconfirmed(&self, reason: String) {
        let mut ledger = self.ledger.lock().unwrap();
        let pending = ledger
            .requests
            .iter()
            .filter(|(_, request)| request.outcome.is_none())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in pending {
            if !ledger.unresolved.contains(&id) {
                ledger.unresolved.push(id);
            }
        }
        ledger.cancellation_notes.push(reason);
        drop(ledger);
        if let Err(error) = self.save() {
            *self.failure.lock().unwrap() = Some(error.to_string());
        }
    }

    pub(crate) fn finish_stop(&self) -> Result<()> {
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(self.directory.join("stop.json"))?)?;
        let checkpoint_path = self
            .directory
            .parent()
            .and_then(Path::parent)
            .context("policy metadata root missing")?
            .join("last-run.json");
        let checkpoint: serde_json::Value = serde_json::from_slice(&fs::read(checkpoint_path)?)?;
        {
            let mut ledger = self.ledger.lock().unwrap();
            ledger.next_stage = checkpoint["stage"].as_str().unwrap_or("unknown").to_owned();
            ledger.change = checkpoint["change"].as_str().map(str::to_owned);
        }
        self.stop(
            report["trigger"].as_str().unwrap_or("policy_stop"),
            report["reason"].as_str().unwrap_or("operating-policy stop"),
        )
    }

    fn check(&self, before_request: bool) -> Result<()> {
        if let Some(failure) = self.failure.lock().unwrap().clone() {
            return self.stop("accounting_unavailable", &failure);
        }
        let ledger = self.ledger.lock().unwrap();
        let trigger = if ledger
            .policy
            .max_active_seconds
            .is_some_and(|limit| self.elapsed_ms() >= limit.saturating_mul(1000))
        {
            Some((
                "active_time_limit",
                "the run reached its selected cumulative active-time limit",
            ))
        } else if ledger.no_progress >= ledger.policy.max_no_progress_reviews {
            Some((
                "no_verified_progress",
                "successive repair/review rounds left retained findings unresolved without independent closure evidence",
            ))
        } else if ledger.unassessable >= ledger.policy.max_no_progress_reviews {
            Some((
                "review_evidence_gap",
                "successive repair/review rounds lack structured independent evidence to assess progress; consolidate the review evidence before another product repair",
            ))
        } else if ledger
            .recurrences
            .values()
            .any(|count| *count >= ledger.policy.max_recurring_finding_reviews)
        {
            Some((
                "recurring_findings",
                "independently closed findings repeatedly reopened; the current repair strategy is not dependable",
            ))
        } else if before_request && !ledger.unresolved.is_empty() {
            Some((
                "unresolved_request",
                "a previous runner process ended with an unresolved request; establish that its backend work has stopped before authorizing more work",
            ))
        } else {
            None
        };
        drop(ledger);
        if let Some((kind, reason)) = trigger {
            return self.stop(kind, reason);
        }
        Ok(())
    }

    fn stop<T>(&self, kind: &str, reason: &str) -> Result<T> {
        let ledger = self.ledger.lock().unwrap();
        let known_cost: f64 = ledger
            .requests
            .values()
            .filter_map(|request| request.reported_cost_usd)
            .sum();
        let unknown_cost = ledger
            .requests
            .values()
            .filter(|request| request.reported_cost_usd.is_none())
            .count();
        let latest_review = ledger.incomplete_review.as_ref().or(ledger.reviews.last());
        let open = latest_review
            .map(|review| review.open.clone())
            .unwrap_or_default();
        let action = match kind {
            "review_evidence_gap" => {
                "Inspect the retained review records and reporting failure. Establish structured independent findings and closure evidence before another product repair. Missing evidence does not prove the implementation is defective or the work was wasted. Supply the reporting correction with --direction and explicitly increase the review allowance only for a justified bounded resume."
            }
            "no_verified_progress" => {
                "Inspect the retained finding IDs and repair claims. Establish the governing-rule correction and a regression that observes it, or finish independent closure if the implementation already satisfies it. Supply the changed strategy with --direction; explicitly raise the selected review allowance with --operating-policy PATH for a bounded resume."
            }
            "recurring_findings" => {
                "Inspect reopened finding histories and related paths. Correct the shared rule and add a regression that fails on the recurrence. Resume with explicit strategy guidance and a bounded policy change; do not suppress the finding."
            }
            "replan_limit" => {
                "Review the decomposition and preserved candidate before permitting more recovery. Resolve the scope/dependency decision, then explicitly increase the replan allowance if another bounded attempt is justified."
            }
            "unresolved_request" => {
                "Check the recorded backend/session identities and stop or reconcile any outstanding owned work. Use an explicit policy resume acknowledgement only after it is accounted for; do not reset the checkpoint or usage history."
            }
            _ => {
                "Inspect the recorded stage, session, outcomes and partial work. Resolve the provider/protocol, delegation or runtime problem, or provide a justified bounded limit increase with --resume --operating-policy PATH. Preserve the original inputs and acceptance gates."
            }
        };
        let report = serde_json::json!({
            "outcome": "operating-policy-stop", "trigger": kind, "reason": reason,
            "next_stage": ledger.next_stage, "change": ledger.change, "last_verified_milestone": ledger.last_verified,
            "remaining_findings": open, "reviews": ledger.reviews, "policy": ledger.policy,
            "remaining_work": latest_review.map(|review| &review.blocking_details),
            "incomplete_review": ledger.incomplete_review,
            "unfinished_stage": ledger.unfinished_stage,
            "active_elapsed_known_ms": self.elapsed_ms(), "active_time_has_gap": ledger.active_time_has_gap,
            "reported_known_cost_subtotal_usd": known_cost,
            "cost_unknown_attempts": unknown_cost, "requests": ledger.requests,
            "trend": {"no_progress_reviews": ledger.no_progress, "unassessable_review_transitions": ledger.unassessable,
                "reopened_finding_rounds": ledger.recurrences,
                "replans_without_verified_progress": ledger.replans_without_progress, "stage_requests": ledger.stage_requests},
            "unresolved_requests": ledger.unresolved, "recommended_next_action": action,
            "cancellation_notes": ledger.cancellation_notes,
            "preserved": "checkpoint, candidate work, protected inputs, findings, check/review logs and recovery baselines",
            "uncertainty": "review classifications and closure are independent reviewer evidence; matching snapshots do not prove a defect pre-existed or that repeated checking was waste",
        });
        write(&self.directory.join("stop.json"), &report)?;
        let details = latest_review
            .map(|review| {
                review
                    .blocking_details
                    .iter()
                    .take(5)
                    .map(|finding| {
                        format!(
                            "{}: {} Sources: {}. Required correction/decision: {}",
                            finding.id,
                            finding.consequence,
                            finding.sources.join(", "),
                            finding.repair
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let pending = ledger
            .incomplete_review
            .as_ref()
            .map(|review| {
                let areas = review
                    .unfinished_areas
                    .iter()
                    .map(|area| {
                        format!(
                            "{} ({:?}): {} Sources: {}. Paths: {}.",
                            area.id,
                            area.status,
                            area.limitations,
                            area.sources.join(", "),
                            area.paths.join(", ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Unfinished independent review: {}\n{areas}\nFull retained evidence: {}",
                    review.summary,
                    review.evidence_path.display()
                )
            })
            .unwrap_or_default();
        let unfinished = ledger.unfinished_stage.as_deref().unwrap_or_default();
        let trend = format!(
            "No-progress rounds: {}/{}; unassessable review transitions: {}/{}; stage requests: {}/{}; replans without independent progress: {}/{}.",
            ledger.no_progress,
            ledger.policy.max_no_progress_reviews,
            ledger.unassessable,
            ledger.policy.max_no_progress_reviews,
            ledger.stage_requests,
            ledger.policy.max_requests_per_stage,
            ledger.replans_without_progress,
            ledger.policy.max_replans_without_verified_progress
        );
        let text = format!(
            "OPERATING POLICY STOP: {reason}\nChange {:?}; next stage {}. Last verified milestone: {}.\nRemaining findings: {}. Known active elapsed: {:.1}s; missing interval: {}. Known reported cost subtotal: ${known_cost:.4}; {unknown_cost} attempt(s) have unknown cost.\n{trend}\n{details}\n{pending}\nUnfinished stage work: {unfinished}\n{action}\nWork and evidence are preserved. Report: {}\nThis is incomplete delivery, not accepted completion or a capacity escalation.",
            ledger.change,
            ledger.next_stage,
            ledger.last_verified.as_deref().unwrap_or("none recorded"),
            open.join(", "),
            self.elapsed_ms() as f64 / 1000.0,
            ledger.active_time_has_gap,
            self.directory.join("stop.json").display()
        );
        fs::write(self.directory.join("stop.txt"), &text)?;
        Err(PolicyStop(text).into())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.save();
        // A fork may briefly hold a duplicate until exec closes it. End this
        // owner's lock explicitly instead of relying on the final descriptor.
        let _ = self.owner_lock.unlock();
    }
}

fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?).with_context(|| {
        format!(
            "cannot preserve operating-policy evidence {}",
            temporary.display()
        )
    })?;
    fs::rename(temporary, path).with_context(|| {
        format!(
            "cannot finalize operating-policy evidence {}",
            path.display()
        )
    })?;
    Ok(())
}
