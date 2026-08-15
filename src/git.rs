use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{process::CommandSpec, ui::Ui};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathFingerprint {
    pub status: String,
    pub worktree_hash: Option<String>,
    pub index_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub head: Option<String>,
    pub dirty: BTreeMap<String, PathFingerprint>,
}

pub fn repository_root<U: Ui>(repo: &Path, ui: &U) -> Result<PathBuf> {
    let output = git(repo, &["rev-parse", "--show-toplevel"], ui)?;
    checked_text(output, "not a git repository").map(|root| PathBuf::from(root.trim()))
}

pub fn snapshot<U: Ui>(repo: &Path, ui: &U) -> Result<RepoSnapshot> {
    let head = git(repo, &["rev-parse", "--verify", "HEAD"], ui)
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    let output = git(
        repo,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        ui,
    )?;
    if !output.status.success() {
        bail!("could not inspect git status: {}", stderr(&output));
    }
    let statuses = parse_porcelain_z(&output.stdout)?;
    let mut dirty = BTreeMap::new();
    for (path, status) in statuses {
        dirty.insert(
            path.clone(),
            PathFingerprint {
                status,
                worktree_hash: object_hash(repo, &path, false, ui),
                index_hash: object_hash(repo, &path, true, ui),
            },
        );
    }
    Ok(RepoSnapshot { head, dirty })
}

pub fn assert_paths_unchanged(baseline: &RepoSnapshot, current: &RepoSnapshot) -> Result<()> {
    let changed: Vec<_> = baseline
        .dirty
        .iter()
        .filter_map(|(path, fingerprint)| {
            (current.dirty.get(path) != Some(fingerprint)).then_some(path.as_str())
        })
        .collect();
    if !changed.is_empty() {
        bail!(
            "BLOCKED: pre-existing user changes were modified during the run: {}. The orchestrator will not guess at mixed ownership",
            changed.join(", ")
        );
    }
    Ok(())
}

pub fn changed_baseline_paths<U: Ui>(
    repo: &Path,
    baseline: &RepoSnapshot,
    ui: &U,
) -> Result<Vec<String>> {
    if baseline.dirty.is_empty() {
        return Ok(Vec::new());
    }
    let output = git(
        repo,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        ui,
    )?;
    if !output.status.success() {
        bail!("could not inspect git status: {}", stderr(&output));
    }
    let statuses = parse_porcelain_z(&output.stdout)?;
    Ok(baseline
        .dirty
        .iter()
        .filter_map(|(path, expected)| {
            let status = statuses.get(path)?;
            let actual = PathFingerprint {
                status: status.clone(),
                worktree_hash: object_hash(repo, path, false, ui),
                index_hash: object_hash(repo, path, true, ui),
            };
            (actual != *expected).then_some(path.clone())
        })
        .chain(
            baseline
                .dirty
                .keys()
                .filter(|path| !statuses.contains_key(*path))
                .cloned(),
        )
        .collect())
}

pub fn ensure_no_baseline_overlap(baseline: &RepoSnapshot, path: &str) -> Result<()> {
    let prefix = format!("{}/", path.trim_end_matches('/'));
    let conflicts: Vec<_> = baseline
        .dirty
        .keys()
        .filter(|candidate| candidate.as_str() == path || candidate.starts_with(&prefix))
        .cloned()
        .collect();
    if !conflicts.is_empty() {
        bail!(
            "BLOCKED: proposal path `{path}` already contained user changes before ospx-build started: {}",
            conflicts.join(", ")
        );
    }
    Ok(())
}

pub fn commit_paths<U: Ui>(repo: &Path, paths: &[String], message: &str, ui: &U) -> Result<String> {
    if paths.is_empty() {
        bail!("BLOCKED: no change-owned paths were available for commit `{message}`");
    }

    let mut add = Command::new("git");
    add.current_dir(repo).args(["add", "--all", "--"]);
    add.args(paths);
    ui.command(
        &CommandSpec::new("git", repo)
            .args(["add", "--all", "--"])
            .args(paths.iter().cloned())
            .display(),
    );
    let add_output = add.output().context("failed to launch `git add`")?;
    if !add_output.status.success() {
        bail!("BLOCKED: git add failed: {}", stderr(&add_output));
    }

    let mut diff = Command::new("git");
    diff.current_dir(repo)
        .args(["diff", "--cached", "--quiet", "--"])
        .args(paths);
    ui.command(
        &CommandSpec::new("git", repo)
            .args(["diff", "--cached", "--quiet", "--"])
            .args(paths.iter().cloned())
            .display(),
    );
    let diff_status = diff.status().context("failed to inspect staged proposal")?;
    if diff_status.success() {
        bail!("BLOCKED: no staged changes remained for `{message}`");
    }
    if diff_status.code() != Some(1) {
        bail!("BLOCKED: git diff failed while checking `{message}`");
    }

    let mut commit = Command::new("git");
    commit
        .current_dir(repo)
        .args(["commit", "-m", message, "--"])
        .args(paths);
    ui.command(
        &CommandSpec::new("git", repo)
            .args(["commit", "-m", message, "--"])
            .args(paths.iter().cloned())
            .display(),
    );
    let commit_output = commit.output().context("failed to launch `git commit`")?;
    if !commit_output.status.success() {
        bail!(
            "BLOCKED: git commit failed; scoped files remain staged and no unrelated changes were discarded: {}",
            stderr(&commit_output)
        );
    }

    head(repo, ui)?.ok_or_else(|| anyhow::anyhow!("git commit succeeded but HEAD is unavailable"))
}

pub fn abort_paths<U: Ui>(repo: &Path, paths: &[String], ui: &U) -> Result<()> {
    for path in paths {
        let expression = format!("HEAD:{path}");
        let tracked = git(repo, &["cat-file", "-e", &expression], ui)
            .is_ok_and(|output| output.status.success());
        if tracked {
            let args = [
                "restore",
                "--source=HEAD",
                "--staged",
                "--worktree",
                "--",
                path.as_str(),
            ];
            let output = git(repo, &args, ui)?;
            if !output.status.success() {
                bail!("BLOCKED: could not restore `{path}`: {}", stderr(&output));
            }
            continue;
        }

        let args = ["rm", "--cached", "--ignore-unmatch", "--", path.as_str()];
        let output = git(repo, &args, ui)?;
        if !output.status.success() {
            bail!("BLOCKED: could not unstage `{path}`: {}", stderr(&output));
        }
        let target = repo.join(path);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir(&target)
                .with_context(|| format!("could not remove run-owned directory `{path}`"))?,
            Ok(_) => fs::remove_file(&target)
                .with_context(|| format!("could not remove run-owned file `{path}`"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not inspect run-owned path `{path}`"));
            }
        }
        remove_empty_parents(repo, target.parent());
    }
    Ok(())
}

pub fn final_candidate_paths(
    baseline: &RepoSnapshot,
    current: &RepoSnapshot,
) -> Result<Vec<String>> {
    Ok(current
        .dirty
        .keys()
        .filter(|path| !baseline.dirty.contains_key(*path))
        .cloned()
        .collect())
}

pub fn ensure_head<U: Ui>(repo: &Path, expected: &str, ui: &U) -> Result<()> {
    let actual = head(repo, ui)?.unwrap_or_else(|| "<unborn>".to_owned());
    if actual != expected {
        bail!(
            "BLOCKED: repository HEAD changed unexpectedly during Claude stages (expected {expected}, found {actual}); refusing to fold unknown commits into the milestone"
        );
    }
    Ok(())
}

pub fn current_head<U: Ui>(repo: &Path, ui: &U) -> Result<Option<String>> {
    head(repo, ui)
}

pub fn is_ancestor<U: Ui>(repo: &Path, ancestor: &str, descendant: &str, ui: &U) -> Result<bool> {
    let output = git(
        repo,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        ui,
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "could not compare Git commits `{ancestor}` and `{descendant}`: {}",
            stderr(&output)
        ),
    }
}

pub fn commits_between<U: Ui>(
    repo: &Path,
    ancestor: &str,
    descendant: &str,
    ui: &U,
) -> Result<Vec<String>> {
    let range = format!("{ancestor}..{descendant}");
    let output = git(repo, &["rev-list", "--reverse", &range], ui)?;
    let text = checked_text(output, "could not enumerate descendant commits")?;
    Ok(text.lines().map(str::to_owned).collect())
}

pub fn commit_parent<U: Ui>(repo: &Path, commit: &str, ui: &U) -> Result<String> {
    let expression = format!("{commit}^");
    let output = git(repo, &["rev-parse", "--verify", &expression], ui)?;
    checked_text(output, "could not resolve commit parent").map(|text| text.trim().to_owned())
}

pub fn commit_subject<U: Ui>(repo: &Path, commit: &str, ui: &U) -> Result<String> {
    let output = git(repo, &["log", "-1", "--format=%s", commit], ui)?;
    checked_text(output, "could not read commit subject").map(|text| text.trim().to_owned())
}

pub fn metadata_dir<U: Ui>(repo: &Path, ui: &U) -> Result<PathBuf> {
    let output = git(repo, &["rev-parse", "--git-path", "ospx-build"], ui)?;
    let path = checked_text(output, "could not resolve git metadata path")?;
    let path = PathBuf::from(path.trim());
    let path = if path.is_absolute() {
        path
    } else {
        repo.join(path)
    };
    fs::create_dir_all(&path)
        .with_context(|| format!("could not create metadata directory `{}`", path.display()))?;
    Ok(path)
}

fn head<U: Ui>(repo: &Path, ui: &U) -> Result<Option<String>> {
    let output = git(repo, &["rev-parse", "--verify", "HEAD"], ui)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

fn object_hash<U: Ui>(repo: &Path, path: &str, index: bool, ui: &U) -> Option<String> {
    let output = if index {
        git(repo, &["rev-parse", "--verify", &format!(":{path}")], ui).ok()?
    } else {
        git(repo, &["hash-object", "--no-filters", "--", path], ui).ok()?
    };
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn remove_empty_parents(repo: &Path, mut directory: Option<&Path>) {
    while let Some(path) = directory {
        if path == repo || !path.starts_with(repo) || fs::remove_dir(path).is_err() {
            break;
        }
        directory = path.parent();
    }
}

fn git<U: Ui>(repo: &Path, args: &[&str], ui: &U) -> Result<Output> {
    ui.command(
        &CommandSpec::new("git", repo)
            .args(args.iter().copied())
            .display(),
    );
    Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .with_context(|| format!("failed to launch `git {}`", args.join(" ")))
}

fn checked_text(output: Output, message: &str) -> Result<String> {
    if !output.status.success() {
        bail!("{message}: {}", stderr(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn stderr(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        stderr
    }
}

fn parse_porcelain_z(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let fields: Vec<&[u8]> = bytes.split(|byte| *byte == 0).collect();
    let mut index = 0;
    let mut entries = BTreeMap::new();
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        if field.is_empty() {
            continue;
        }
        if field.len() < 4 || field[2] != b' ' {
            bail!("unexpected `git status --porcelain -z` record");
        }
        let status = String::from_utf8_lossy(&field[..2]).into_owned();
        let path = String::from_utf8(field[3..].to_vec())
            .context("ospx-build currently requires UTF-8 repository paths")?;
        entries.insert(path, status.clone());

        if status.bytes().any(|byte| matches!(byte, b'R' | b'C')) {
            let source = fields
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("rename record omitted its source path"))?;
            index += 1;
            let source = String::from_utf8(source.to_vec())
                .context("ospx-build currently requires UTF-8 repository paths")?;
            entries.insert(source, status);
        }
    }
    Ok(entries)
}

pub fn path_set(snapshot: &RepoSnapshot) -> BTreeSet<&str> {
    snapshot.dirty.keys().map(String::as_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_normal_untracked_and_rename_records() {
        let parsed =
            parse_porcelain_z(b" M src/lib.rs\0?? notes with spaces.md\0R  new.rs\0old.rs\0")
                .unwrap();
        assert_eq!(parsed.get("src/lib.rs"), Some(&" M".to_owned()));
        assert_eq!(parsed.get("notes with spaces.md"), Some(&"??".to_owned()));
        assert!(parsed.contains_key("new.rs"));
        assert!(parsed.contains_key("old.rs"));
    }
}
