use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::openspec::ChangeSnapshot;

const SLICES_DIR: &str = "automation/slices";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgendaAssignment {
    pub path: String,
    pub change: String,
    pub title: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgendaSelection {
    Absent,
    Next(AgendaAssignment),
    Complete,
}

pub fn has_subdivision(repo: &Path, assignment: &AgendaAssignment) -> Result<bool> {
    let file_name = Path::new(&assignment.path)
        .file_name()
        .and_then(|name| name.to_str())
        .context("assigned agenda path has no UTF-8 file name")?;
    let (parent, _, _) = parse_slice_name(file_name)
        .context("assigned agenda path does not use a supported slice ordinal")?;
    let directory = repo.join(SLICES_DIR);
    for entry in fs::read_dir(&directory)
        .with_context(|| format!("could not read agenda directory `{}`", directory.display()))?
    {
        let entry = entry?;
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((ordinal, _, _)) = parse_slice_name(&file_name) else {
            continue;
        };
        if ordinal.len() == parent.len() + 1
            && ordinal.starts_with(&parent)
            && ordinal.last() == Some(&1)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug)]
struct SliceFile {
    ordinal: Vec<u32>,
    ordinal_text: String,
    stem: String,
    path: String,
    title: String,
    content: String,
}

pub fn discover(repo: &Path, active: &ChangeSnapshot) -> Result<AgendaSelection> {
    let directory = repo.join(SLICES_DIR);
    if !directory.is_dir() {
        return Ok(AgendaSelection::Absent);
    }

    let mut slices = Vec::new();
    for entry in fs::read_dir(&directory)
        .with_context(|| format!("could not read agenda directory `{}`", directory.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some((ordinal, ordinal_text, stem)) = parse_slice_name(&file_name) else {
            continue;
        };
        let content = fs::read_to_string(entry.path())
            .with_context(|| format!("could not read agenda slice `{file_name}`"))?;
        let title = content
            .lines()
            .find_map(|line| line.strip_prefix("# "))
            .unwrap_or(&stem)
            .to_owned();
        slices.push(SliceFile {
            ordinal,
            ordinal_text,
            stem,
            path: format!("{SLICES_DIR}/{file_name}"),
            title,
            content,
        });
    }

    if slices.is_empty() {
        return Ok(AgendaSelection::Absent);
    }
    slices.sort_by(|left, right| {
        left.ordinal
            .cmp(&right.ordinal)
            .then_with(|| left.stem.cmp(&right.stem))
    });
    for pair in slices.windows(2) {
        if pair[0].ordinal == pair[1].ordinal {
            bail!(
                "agenda contains duplicate slice ordinal {}: `{}` and `{}`",
                pair[0].ordinal_text,
                pair[0].path,
                pair[1].path
            );
        }
    }

    let archive = repo.join("openspec/changes/archive");
    let archived = directory_names(&archive)?;
    for slice in slices {
        if unique_match(&archived, &slice.stem, "archived")?.is_some() {
            continue;
        }
        let change = unique_match(
            &active.changes.keys().cloned().collect::<Vec<_>>(),
            &slice.stem,
            "active",
        )?
        .unwrap_or_else(|| slice.stem.clone());
        return Ok(AgendaSelection::Next(AgendaAssignment {
            path: slice.path,
            change,
            title: slice.title,
            content: slice.content,
        }));
    }

    Ok(AgendaSelection::Complete)
}

fn parse_slice_name(file_name: &str) -> Option<(Vec<u32>, String, String)> {
    let stem = file_name.strip_suffix(".md")?;
    let (ordinal_text, slug) = stem.split_once('-')?;
    let ordinal = ordinal_text
        .split('.')
        .map(|component| {
            (!component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| component.parse().ok())
                .flatten()
        })
        .collect::<Option<Vec<u32>>>()?;
    if ordinal.is_empty()
        || slug.is_empty()
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return None;
    }
    Some((
        ordinal,
        ordinal_text.to_owned(),
        format!("{}-{slug}", ordinal_text.replace('.', "-")),
    ))
}

fn directory_names(directory: &Path) -> Result<Vec<String>> {
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)
        .with_context(|| format!("could not read `{}`", directory.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
        {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

fn unique_match(names: &[String], stem: &str, kind: &str) -> Result<Option<String>> {
    let suffix = format!("-{stem}");
    let matches = names
        .iter()
        .filter(|name| name.as_str() == stem || name.ends_with(&suffix))
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [name] => Ok(Some(name.clone())),
        _ => bail!(
            "multiple {kind} OpenSpec changes match agenda slice `{stem}`: {}",
            matches.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn fixture() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("opsx-build-agenda-{}", Uuid::new_v4()));
        fs::create_dir_all(path.join(SLICES_DIR)).unwrap();
        fs::create_dir_all(path.join("openspec/changes/archive")).unwrap();
        path
    }

    fn write_slice(repo: &Path, name: &str, title: &str) {
        fs::write(
            repo.join(SLICES_DIR).join(name),
            format!("# {title}\n\n## Objective\n\nDo it.\n"),
        )
        .unwrap();
    }

    #[test]
    fn selects_first_unarchived_slice_in_numeric_order() {
        let repo = fixture();
        write_slice(&repo, "002-second.md", "002 - Second");
        write_slice(&repo, "001-first.md", "001 - First");
        fs::create_dir(repo.join("openspec/changes/archive/2026-08-24-001-first")).unwrap();

        let selected = discover(&repo, &ChangeSnapshot::default()).unwrap();
        let AgendaSelection::Next(assignment) = selected else {
            panic!("expected an agenda assignment");
        };
        assert_eq!(assignment.change, "002-second");
        assert_eq!(assignment.path, "automation/slices/002-second.md");
        assert_eq!(assignment.title, "002 - Second");
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn resumes_a_matching_dated_active_change() {
        let repo = fixture();
        write_slice(&repo, "001-first.md", "001 - First");
        let active = ChangeSnapshot {
            changes: BTreeMap::from([(
                "2026-08-24-001-first".to_owned(),
                json!({"name":"2026-08-24-001-first"}),
            )]),
        };

        let AgendaSelection::Next(assignment) = discover(&repo, &active).unwrap() else {
            panic!("expected an agenda assignment");
        };
        assert_eq!(assignment.change, "2026-08-24-001-first");
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn reports_complete_when_every_slice_is_archived() {
        let repo = fixture();
        write_slice(&repo, "001-first.md", "001 - First");
        fs::create_dir(repo.join("openspec/changes/archive/001-first")).unwrap();
        assert_eq!(
            discover(&repo, &ChangeSnapshot::default()).unwrap(),
            AgendaSelection::Complete
        );
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn ignores_a_non_agenda_readme() {
        let repo = fixture();
        fs::write(repo.join(SLICES_DIR).join("README.md"), "instructions").unwrap();
        assert_eq!(
            discover(&repo, &ChangeSnapshot::default()).unwrap(),
            AgendaSelection::Absent
        );
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn accepts_four_digit_and_variable_width_slice_numbers() {
        assert_eq!(
            parse_slice_name("0001-repo-cli-scaffold.md"),
            Some((
                vec![1],
                "0001".to_owned(),
                "0001-repo-cli-scaffold".to_owned()
            ))
        );
        assert_eq!(
            parse_slice_name("12-parser.md"),
            Some((vec![12], "12".to_owned(), "12-parser".to_owned()))
        );
        assert_eq!(parse_slice_name("README.md"), None);
    }

    #[test]
    fn hierarchical_ordinals_sort_numerically_and_map_to_kebab_change_names() {
        let repo = fixture();
        write_slice(&repo, "0010-loops.md", "Loops");
        write_slice(&repo, "0009.10-late-conditionals.md", "Late conditionals");
        write_slice(&repo, "0009.2-early-conditionals.md", "Early conditionals");
        write_slice(&repo, "0009.1-first-conditionals.md", "First conditionals");
        write_slice(&repo, "0009-conditionals-core.md", "Conditionals core");
        fs::create_dir(repo.join("openspec/changes/archive/0009-conditionals-core")).unwrap();
        fs::create_dir(repo.join("openspec/changes/archive/0009-1-first-conditionals")).unwrap();

        let AgendaSelection::Next(assignment) =
            discover(&repo, &ChangeSnapshot::default()).unwrap()
        else {
            panic!("expected an agenda assignment");
        };
        assert_eq!(
            assignment.path,
            "automation/slices/0009.2-early-conditionals.md"
        );
        assert_eq!(assignment.change, "0009-2-early-conditionals");
        assert!(
            has_subdivision(
                &repo,
                &AgendaAssignment {
                    path: "automation/slices/0009-conditionals-core.md".to_owned(),
                    change: "0009-conditionals-core".to_owned(),
                    title: "Conditionals core".to_owned(),
                    content: "original".to_owned(),
                }
            )
            .unwrap()
        );
        fs::remove_dir_all(repo).unwrap();
    }
}
