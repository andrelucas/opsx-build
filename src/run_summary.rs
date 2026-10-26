//! End-of-run observations; these never establish acceptance or authorize work.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::usage::{Tokens, UsageContext};

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Timing {
    pub started_at_unix_ms: Option<u64>,
    pub finished_at_unix_ms: Option<u64>,
    pub finished_active_ms: Option<u64>,
    changes: BTreeMap<String, ChangeTiming>,
    #[serde(default)]
    unassigned: Option<ChangeTiming>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChangeTiming {
    started_active_ms: u64,
    finished_active_ms: Option<u64>,
    complete: bool,
}

impl Timing {
    pub(crate) fn start() -> Self {
        Self {
            started_at_unix_ms: Some(now_ms()),
            ..Self::default()
        }
    }

    pub(crate) fn boundary(
        &mut self,
        change: Option<&str>,
        stage: &str,
        active_ms: u64,
        known_start: bool,
    ) {
        let started = ChangeTiming {
            started_active_ms: active_ms,
            finished_active_ms: None,
            complete: known_start,
        };
        let Some(change) = change else {
            if !stage.eq_ignore_ascii_case("complete") && !stage.eq_ignore_ascii_case("done") {
                self.unassigned.get_or_insert(started);
            }
            return;
        };
        let started = self.unassigned.take().unwrap_or(started);
        let timing = self.changes.entry(change.to_owned()).or_insert(started);
        if stage.eq_ignore_ascii_case("complete") || stage.eq_ignore_ascii_case("done") {
            timing.finished_active_ms.get_or_insert(active_ms);
        }
    }

    pub(crate) fn elapsed_seconds(&self, change: &str, active_ms: u64) -> Option<u64> {
        let timing = self.changes.get(change)?;
        timing.complete.then(|| {
            timing
                .finished_active_ms
                .unwrap_or(active_ms)
                .saturating_sub(timing.started_active_ms)
                / 1000
        })
    }

    pub(crate) fn interrupted(&mut self, change: Option<&str>) {
        if let Some(timing) = match change {
            Some(change) => self.changes.get_mut(change),
            None => self.unassigned.as_mut(),
        } {
            timing.complete = false;
        }
    }
}

pub(crate) struct Request {
    pub backend: String,
    pub context: UsageContext,
    pub elapsed_ms: u64,
    pub outcome: Option<String>,
    pub dispatches: u32,
}

pub(crate) struct Context {
    pub directory: PathBuf,
    pub timing: Timing,
    pub active_ms: u64,
    pub active_time_has_gap: bool,
    pub requests: BTreeMap<String, Request>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Row {
    pub name: String,
    pub status: String,
    pub elapsed_seconds: Option<u64>,
    pub commit: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct Count {
    known: u64,
    reported_attempts: u64,
}
impl Count {
    fn add(&mut self, value: Option<u64>) {
        if let Some(value) = value {
            self.known = self.known.saturating_add(value);
            self.reported_attempts += 1;
        }
    }
    fn display(&self, attempts: u64, partial: bool) -> String {
        if self.reported_attempts == 0 {
            return "unknown".into();
        }
        format!(
            "{}{}",
            compact(self.known),
            if self.reported_attempts < attempts || partial {
                "+?"
            } else {
                ""
            }
        )
    }
}

#[derive(Debug, Default, Serialize)]
struct Model {
    attempts: u64,
    dispatches: u64,
    input: Count,
    cached_input: Count,
    output: Count,
    reasoning: Count,
    partial_attempts: u64,
    reported_cost_usd: f64,
    cost_reported_attempts: u64,
}

// Do not retain the detailed tool traces or cumulative session snapshots.
#[derive(Deserialize)]
struct Observation {
    id: String,
    requested_model: Option<String>,
    reported_models: Vec<String>,
    tokens: Tokens,
    reported_cost_usd: Option<f64>,
    usage_scope: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Summary {
    format_version: u32,
    pub outcome: String,
    pub current_stage: String,
    pub error: Option<String>,
    pub rows: Vec<Row>,
    started_at_unix_ms: Option<u64>,
    ended_at_unix_ms: u64,
    wall_clock_seconds: Option<u64>,
    active_seconds: u64,
    active_time_has_gap: bool,
    attempts: usize,
    dispatches: u64,
    repair_attempts: usize,
    incomplete_attempts: usize,
    provider_errors: usize,
    request_time_seconds: u64,
    request_time_has_gap: bool,
    usage_records_missing: usize,
    models: BTreeMap<String, Model>,
}

impl Summary {
    pub(crate) fn collect(
        context: &Context,
        outcome: &str,
        current_stage: &str,
        error: Option<String>,
        rows: Vec<Row>,
    ) -> Self {
        let metadata = context
            .directory
            .parent()
            .and_then(|p| p.parent())
            .expect("run metadata root");
        let mut records = BTreeMap::new();
        // Only this policy run's request IDs belong here. Last valid observation wins.
        if let Ok(file) = File::open(metadata.join("usage.jsonl")) {
            for line in BufReader::new(file)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if let Ok(record) = serde_json::from_str::<Observation>(&line)
                    && context.requests.contains_key(&record.id)
                {
                    records.insert(record.id.clone(), record);
                }
            }
        }
        let mut models = BTreeMap::<String, Model>::new();
        for (id, request) in &context.requests {
            let record = records.get(id);
            let model_name = record
                .map(|record| {
                    if !record.reported_models.is_empty() {
                        record
                            .reported_models
                            .iter()
                            .map(String::as_str)
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .collect::<Vec<_>>()
                            .join(" + ")
                    } else {
                        record
                            .requested_model
                            .as_ref()
                            .map_or("unreported".into(), |model| format!("{model} (requested)"))
                    }
                })
                .unwrap_or_else(|| "unreported".into());
            let model = models
                .entry(format!("{} / {model_name}", request.backend))
                .or_default();
            model.attempts += 1;
            model.dispatches += u64::from(request.dispatches);
            if let Some(record) = record {
                model.input.add(record.tokens.input);
                model.cached_input.add(record.tokens.cache_read);
                model.output.add(record.tokens.output);
                model.reasoning.add(record.tokens.reasoning);
                if !matches!(
                    record.usage_scope.as_str(),
                    "thread_delta" | "session_tree_delta" | "observed_steps"
                ) {
                    model.partial_attempts += 1;
                }
                if let Some(cost) = record
                    .reported_cost_usd
                    .filter(|cost| cost.is_finite() && *cost >= 0.0)
                {
                    model.reported_cost_usd += cost;
                    model.cost_reported_attempts += 1;
                }
            }
        }
        let ended = context.timing.finished_at_unix_ms.unwrap_or_else(now_ms);
        Self {
            format_version: 1,
            outcome: outcome.into(),
            current_stage: current_stage.into(),
            error,
            rows,
            started_at_unix_ms: context.timing.started_at_unix_ms,
            ended_at_unix_ms: ended,
            wall_clock_seconds: context
                .timing
                .started_at_unix_ms
                .and_then(|start| ended.checked_sub(start))
                .map(|ms| ms / 1000),
            active_seconds: context
                .timing
                .finished_active_ms
                .unwrap_or(context.active_ms)
                / 1000,
            active_time_has_gap: context.active_time_has_gap,
            attempts: context.requests.len(),
            dispatches: context
                .requests
                .values()
                .map(|r| u64::from(r.dispatches))
                .sum(),
            repair_attempts: context
                .requests
                .values()
                .filter(|r| r.context.stage.as_deref() == Some("Repair"))
                .count(),
            incomplete_attempts: context
                .requests
                .values()
                .filter(|r| r.outcome.as_deref() == Some("incomplete"))
                .count(),
            provider_errors: context
                .requests
                .values()
                .filter(|r| r.outcome.as_deref() == Some("provider_error"))
                .count(),
            request_time_seconds: context.requests.values().map(|r| r.elapsed_ms).sum::<u64>()
                / 1000,
            request_time_has_gap: context.requests.values().any(|r| r.outcome.is_none()),
            usage_records_missing: context.requests.len() - records.len(),
            models,
        }
    }

    pub(crate) fn render(&self) -> String {
        let width = self
            .rows
            .iter()
            .map(|r| r.name.len())
            .max()
            .unwrap_or(0)
            .max("Bootstrap / slice".len());
        let mut text = format!(
            "\nRUN SUMMARY — {}\n  {:width$}  {:>12}  {:12}  Status\n",
            self.outcome.to_uppercase(),
            "Bootstrap / slice",
            "Active time",
            "Commit"
        );
        for row in &self.rows {
            let commit = row
                .commit
                .as_deref()
                .map(|hash| &hash[..hash.len().min(12)])
                .unwrap_or("-");
            text.push_str(&format!(
                "  {:width$}  {:>12}  {:12}  {}\n",
                row.name,
                row.elapsed_seconds.map_or("unknown".into(), duration),
                commit,
                row.status
            ));
        }
        text.push_str(&format!("Wall clock: {} (includes time between resumes)\nRunner active: {}{}\nAgent requests: {} attempts / {} dispatches; {} repair, {} incomplete, {} provider-error attempts\nSummed request time: {}{}\n",
            self.wall_clock_seconds.map_or("unknown".into(), duration), duration(self.active_seconds),
            if self.active_time_has_gap { " (recorded subtotal; an interval is missing)" } else { "" },
            self.attempts, self.dispatches, self.repair_attempts, self.incomplete_attempts, self.provider_errors, duration(self.request_time_seconds),
            if self.request_time_has_gap { " (recorded subtotal; an attempt is unresolved)" } else { "" }));
        for (name, model) in &self.models {
            let partial = model.partial_attempts > 0;
            let cost = if model.cost_reported_attempts == 0 {
                "unavailable".into()
            } else {
                format!(
                    "${:.4}{} reported",
                    model.reported_cost_usd,
                    if model.cost_reported_attempts < model.attempts || partial {
                        "+?"
                    } else {
                        ""
                    }
                )
            };
            text.push_str(&format!(
                "  {name}: {} dispatches; {} input ({} cached), {} output; cost {cost}\n",
                model.dispatches,
                model.input.display(model.attempts, partial),
                model.cached_input.display(model.attempts, partial),
                model.output.display(model.attempts, partial)
            ));
        }
        text.push_str("Cached input is included in input. +? means a known subtotal with missing/partial usage; costs are not estimated.\n");
        if self.usage_records_missing > 0 {
            text.push_str(&format!(
                "Detailed usage unavailable for {} attempt(s).\n",
                self.usage_records_missing
            ));
        }
        text
    }

    pub(crate) fn save(&self, context: &Context) -> Result<PathBuf> {
        for (name, bytes) in [
            ("summary.json", serde_json::to_vec_pretty(self)?),
            ("summary.txt", self.render().into_bytes()),
        ] {
            let path = context.directory.join(name);
            let temporary = path.with_extension("tmp");
            fs::write(&temporary, bytes)?;
            fs::rename(temporary, path)?;
        }
        Ok(context.directory.join("summary.json"))
    }
}

fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1}k", value as f64 / 1000.0)
    } else {
        value.to_string()
    }
}

fn duration(seconds: u64) -> String {
    match (seconds / 3600, seconds / 60 % 60) {
        (0, 0) => format!("{seconds}s"),
        (0, minutes) => format!("{minutes}m {:02}s", seconds % 60),
        (hours, minutes) => format!("{hours}h {minutes:02}m {:02}s", seconds % 60),
    }
}

#[cfg(test)]
mod tests;
