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
