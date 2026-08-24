use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    process::{CommandSpec, ProcessRunner},
    ui::Ui,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChangeSnapshot {
    pub changes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningStatus {
    pub is_complete: bool,
    pub next_steps: Vec<String>,
}

impl ChangeSnapshot {
    pub fn names(&self) -> BTreeSet<&str> {
        self.changes.keys().map(String::as_str).collect()
    }
}

pub fn list_command(repo: &Path) -> CommandSpec {
    CommandSpec::new("openspec", repo).args(["list", "--json"])
}

pub fn snapshot<U: Ui>(repo: &Path, ui: &U) -> Result<ChangeSnapshot> {
    let output = ProcessRunner::new(ui).checked(&list_command(repo), "Reading OpenSpec changes")?;
    parse_list_json(&output.stdout)
}

pub fn planning_status<U: Ui>(repo: &Path, change: &str, ui: &U) -> Result<PlanningStatus> {
    let spec = CommandSpec::new("openspec", repo).args(["status", "--change", change, "--json"]);
    let output = ProcessRunner::new(ui).checked(&spec, "Checking proposal completeness")?;
    parse_planning_status_json(&output.stdout)
}

pub fn parse_planning_status_json(json: &str) -> Result<PlanningStatus> {
    let value: Value = serde_json::from_str(json)
        .context("invalid JSON from `openspec status --change NAME --json`")?;
    let is_complete = value
        .get("isPlanningComplete")
        .and_then(Value::as_bool)
        .context("OpenSpec status JSON omitted boolean `isPlanningComplete`")?;
    let next_steps = value
        .get("nextSteps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    Ok(PlanningStatus {
        is_complete,
        next_steps,
    })
}

pub fn parse_list_json(json: &str) -> Result<ChangeSnapshot> {
    let value: Value =
        serde_json::from_str(json).context("invalid JSON from `openspec list --json`")?;
    let entries = value
        .get("changes")
        .and_then(Value::as_array)
        .or_else(|| value.get("data").and_then(Value::as_array))
        .or_else(|| value.as_array())
        .ok_or_else(|| anyhow::anyhow!("OpenSpec JSON contained no `changes` array"))?;

    let mut changes = BTreeMap::new();
    for entry in entries {
        let name = entry
            .as_str()
            .or_else(|| entry.get("name").and_then(Value::as_str))
            .or_else(|| entry.get("id").and_then(Value::as_str))
            .or_else(|| {
                entry
                    .get("change")
                    .and_then(|change| change.get("id"))
                    .and_then(Value::as_str)
            })
            .ok_or_else(|| anyhow::anyhow!("OpenSpec change entry had no name: {entry}"))?;
        changes.insert(name.to_owned(), entry.clone());
    }
    Ok(ChangeSnapshot { changes })
}

pub fn identify_change(before: &ChangeSnapshot, after: &ChangeSnapshot) -> Result<String> {
    let new: Vec<_> = after
        .changes
        .keys()
        .filter(|name| !before.changes.contains_key(*name))
        .cloned()
        .collect();
    match new.as_slice() {
        [name] => return Ok(name.clone()),
        [] => {}
        _ => bail!(
            "proposal created multiple OpenSpec changes ({}) and the intended change is ambiguous",
            new.join(", ")
        ),
    }

    let modified: Vec<_> = after
        .changes
        .iter()
        .filter_map(|(name, value)| {
            before
                .changes
                .get(name)
                .filter(|before_value| *before_value != value)
                .map(|_| name.clone())
        })
        .collect();
    match modified.as_slice() {
        [name] => return Ok(name.clone()),
        [] => {}
        _ => bail!(
            "proposal updated multiple existing OpenSpec changes ({}) and the intended change is ambiguous",
            modified.join(", ")
        ),
    }

    if after.changes.len() == 1 {
        return Ok(after
            .changes
            .keys()
            .next()
            .expect("one change exists")
            .clone());
    }

    bail!(
        "could not identify the proposed OpenSpec change safely: no unique new or modified change was found among {} active changes",
        after.changes.len()
    )
}

pub fn identify_assigned_change(
    before: &ChangeSnapshot,
    after: &ChangeSnapshot,
    assigned: &str,
) -> Result<String> {
    let changed = before
        .changes
        .keys()
        .chain(after.changes.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| before.changes.get(*name) != after.changes.get(*name))
        .map(String::as_str)
        .collect::<Vec<_>>();

    if !after.changes.contains_key(assigned) {
        let detail = if changed.is_empty() {
            "no active change was created or updated".to_owned()
        } else {
            format!("instead changed {}", changed.join(", "))
        };
        bail!("Propose did not create or continue assigned OpenSpec change `{assigned}`; {detail}")
    }
    if !changed.contains(&assigned) {
        bail!("Propose left assigned OpenSpec change `{assigned}` unchanged")
    }
    let unexpected = changed
        .into_iter()
        .filter(|name| *name != assigned)
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        bail!(
            "Propose changed unassigned OpenSpec change(s) while working on `{assigned}`: {}",
            unexpected.join(", ")
        )
    }
    Ok(assigned.to_owned())
}

pub fn select_existing_change(active: &ChangeSnapshot, requested: Option<&str>) -> Result<String> {
    if let Some(change) = requested {
        if active.changes.contains_key(change) {
            return Ok(change.to_owned());
        }
        bail!("OpenSpec change `{change}` is not active");
    }

    match active
        .changes
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .as_slice()
    {
        [change] => Ok(change.clone()),
        [] => bail!("no active OpenSpec change exists to continue"),
        changes => bail!(
            "multiple OpenSpec changes are active ({}); select one with `--change NAME`",
            changes.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_openspec_shape() {
        let snapshot = parse_list_json(
            r#"{
                "changes": [
                    {"name":"beta", "lastModified":"2026-08-14T12:00:00Z"},
                    {"name":"alpha", "status":"in-progress"}
                ],
                "root": {"path":"/repo"}
            }"#,
        )
        .unwrap();
        assert_eq!(snapshot.names(), BTreeSet::from(["alpha", "beta"]));
    }

    #[test]
    fn parses_compatible_array_and_data_shapes() {
        let array = parse_list_json(r#"[{"id":"one"}, "two"]"#).unwrap();
        assert_eq!(array.names(), BTreeSet::from(["one", "two"]));

        let data = parse_list_json(r#"{"data":[{"name":"three"}]}"#).unwrap();
        assert_eq!(data.names(), BTreeSet::from(["three"]));
    }

    #[test]
    fn identifies_one_new_change_among_preexisting_changes() {
        let before = parse_list_json(r#"{"changes":[{"name":"old"}]}"#).unwrap();
        let after =
            parse_list_json(r#"{"changes":[{"name":"new-slice"},{"name":"old"}]}"#).unwrap();
        assert_eq!(identify_change(&before, &after).unwrap(), "new-slice");
    }

    #[test]
    fn parses_planning_completeness_and_next_steps() {
        let incomplete = parse_planning_status_json(
            r#"{"isPlanningComplete":false,"nextSteps":["Create proposal","Create tasks"]}"#,
        )
        .unwrap();
        assert!(!incomplete.is_complete);
        assert_eq!(incomplete.next_steps, ["Create proposal", "Create tasks"]);

        let complete =
            parse_planning_status_json(r#"{"isPlanningComplete":true,"nextSteps":[]}"#).unwrap();
        assert!(complete.is_complete);
        assert!(complete.next_steps.is_empty());
        assert!(parse_planning_status_json(r#"{"nextSteps":[]}"#).is_err());
    }

    #[test]
    fn identifies_one_modified_existing_change() {
        let before = parse_list_json(
            r#"{"changes":[{"name":"slice","lastModified":"one"},{"name":"other"}]}"#,
        )
        .unwrap();
        let after = parse_list_json(
            r#"{"changes":[{"name":"slice","lastModified":"two"},{"name":"other"}]}"#,
        )
        .unwrap();
        assert_eq!(identify_change(&before, &after).unwrap(), "slice");
    }

    #[test]
    fn rejects_ambiguous_new_changes() {
        let before = ChangeSnapshot::default();
        let after = parse_list_json(r#"{"changes":[{"name":"one"},{"name":"two"}]}"#).unwrap();
        assert!(identify_change(&before, &after).is_err());
    }

    #[test]
    fn assigned_change_must_be_the_only_changed_openspec_change() {
        let before = parse_list_json(r#"{"changes":[{"name":"unrelated"}]}"#).unwrap();
        let after = parse_list_json(
            r#"{"changes":[{"name":"002-next","lastModified":"now"},{"name":"unrelated"}]}"#,
        )
        .unwrap();
        assert_eq!(
            identify_assigned_change(&before, &after, "002-next").unwrap(),
            "002-next"
        );

        let extra = parse_list_json(
            r#"{"changes":[{"name":"002-next"},{"name":"003-wrong"},{"name":"unrelated"}]}"#,
        )
        .unwrap();
        assert!(identify_assigned_change(&before, &extra, "002-next").is_err());
        assert!(identify_assigned_change(&before, &after, "003-wrong").is_err());

        let removed_unrelated = parse_list_json(r#"{"changes":[{"name":"002-next"}]}"#).unwrap();
        assert!(identify_assigned_change(&before, &removed_unrelated, "002-next").is_err());
    }

    #[test]
    fn selects_only_active_change_or_explicit_change() {
        let active =
            parse_list_json(r#"{"changes":[{"name":"slice-m"},{"name":"test-infrastructure"}]}"#)
                .unwrap();
        assert_eq!(
            select_existing_change(&active, Some("test-infrastructure")).unwrap(),
            "test-infrastructure"
        );
        assert!(select_existing_change(&active, None).is_err());

        let one = parse_list_json(r#"{"changes":[{"name":"test-infrastructure"}]}"#).unwrap();
        assert_eq!(
            select_existing_change(&one, None).unwrap(),
            "test-infrastructure"
        );
    }
}
