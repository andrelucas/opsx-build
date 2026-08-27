use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    process::{CommandSpec, ProcessRunner},
    ui::Ui,
};

pub fn repository_root<U: Ui>(repo: &Path, ui: &U) -> Result<PathBuf> {
    let spec = CommandSpec::new("git", repo).args(["rev-parse", "--show-toplevel"]);
    let output = ProcessRunner::new(ui).checked(&spec, "Reading Git repository")?;
    Ok(PathBuf::from(output.stdout.trim()))
}

pub fn current_head<U: Ui>(repo: &Path, ui: &U) -> Result<Option<String>> {
    let spec = CommandSpec::new("git", repo).args(["rev-parse", "--verify", "HEAD"]);
    let output = ProcessRunner::new(ui).run(&spec, "Reading Git HEAD")?;
    if !output.success {
        return Ok(None);
    }
    Ok(Some(output.stdout.trim().to_owned()))
}

pub fn untracked_paths<U: Ui>(repo: &Path, ui: &U) -> Result<Vec<String>> {
    let spec =
        CommandSpec::new("git", repo).args(["ls-files", "--others", "--exclude-standard", "-z"]);
    let output = ProcessRunner::new(ui).checked(&spec, "Recording untracked baseline")?;
    let mut paths = output
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SliceBaseline {
    pub head: String,
    pub tracked_stash: Option<String>,
    pub untracked: Vec<String>,
    pub status: String,
    snapshot_id: String,
}

pub fn capture_slice_baseline<U: Ui>(repo: &Path, ui: &U) -> Result<SliceBaseline> {
    let head =
        current_head(repo, ui)?.context("cannot record a pre-Propose baseline without HEAD")?;
    let runner = ProcessRunner::new(ui);
    let snapshot_id = Uuid::new_v4().to_string();
    let status = repository_status(repo, ui)?;
    let stash = runner.checked(
        &CommandSpec::new("git", repo).args([
            "stash",
            "create",
            "opsx-build pre-Propose rollback baseline",
        ]),
        "Recording tracked rollback baseline",
    )?;
    let tracked_stash = (!stash.stdout.trim().is_empty()).then(|| stash.stdout.trim().to_owned());
    if let Some(stash) = &tracked_stash {
        runner.checked(
            &CommandSpec::new("git", repo).args([
                "update-ref",
                &format!("refs/opsx-build/baselines/{snapshot_id}"),
                stash,
            ]),
            "Preserving tracked rollback baseline",
        )?;
    }
    let untracked = untracked_paths(repo, ui)?;
    let snapshot_root = metadata_dir(repo, ui)?.join("baselines").join(&snapshot_id);
    for path in &untracked {
        let relative = safe_relative_path(path)?;
        copy_path(&repo.join(relative), &snapshot_root.join(relative))?;
    }
    Ok(SliceBaseline {
        head,
        tracked_stash,
        untracked,
        status,
        snapshot_id,
    })
}

pub fn release_slice_baseline<U: Ui>(repo: &Path, baseline: &SliceBaseline, ui: &U) -> Result<()> {
    if baseline.tracked_stash.is_some() {
        ProcessRunner::new(ui).checked(
            &CommandSpec::new("git", repo).args([
                "update-ref",
                "-d",
                &format!("refs/opsx-build/baselines/{}", baseline.snapshot_id),
            ]),
            "Releasing rollback baseline",
        )?;
    }
    let snapshot_root = metadata_dir(repo, ui)?
        .join("baselines")
        .join(&baseline.snapshot_id);
    match fs::remove_dir_all(&snapshot_root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("could not remove `{}`", snapshot_root.display()))
        }
    }
}

pub fn repository_status<U: Ui>(repo: &Path, ui: &U) -> Result<String> {
    let output = ProcessRunner::new(ui).checked(
        &CommandSpec::new("git", repo).args(["status", "--porcelain=v1", "-z"]),
        "Reading Git status",
    )?;
    Ok(output.stdout)
}

pub fn committed_paths_since<U: Ui>(repo: &Path, baseline: &str, ui: &U) -> Result<Vec<String>> {
    let range = format!("{baseline}..HEAD");
    let spec = CommandSpec::new("git", repo).args(["diff", "--name-only", "-z", &range, "--"]);
    let output = ProcessRunner::new(ui).checked(&spec, "Checking frontier planning commit")?;
    Ok(output
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

pub fn head_descends_from<U: Ui>(repo: &Path, baseline: &str, ui: &U) -> Result<bool> {
    let output = ProcessRunner::new(ui).run(
        &CommandSpec::new("git", repo).args(["merge-base", "--is-ancestor", baseline, "HEAD"]),
        "Checking frontier commit ancestry",
    )?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("could not determine whether the frontier commit descends from the baseline"),
    }
}

pub fn baseline_matches_except<U: Ui>(
    repo: &Path,
    baseline: &SliceBaseline,
    excluded_prefix: Option<&str>,
    ui: &U,
) -> Result<bool> {
    let worktree_reference = baseline.tracked_stash.as_deref().unwrap_or(&baseline.head);
    let index_reference = baseline
        .tracked_stash
        .as_ref()
        .map_or_else(|| baseline.head.clone(), |stash| format!("{stash}^2"));
    if !diff_matches(repo, worktree_reference, false, excluded_prefix, ui)?
        || !diff_matches(repo, &index_reference, true, excluded_prefix, ui)?
    {
        return Ok(false);
    }

    let current_untracked = untracked_paths(repo, ui)?;
    let retain = |path: &&String| excluded_prefix.is_none_or(|prefix| !path.starts_with(prefix));
    let expected = baseline.untracked.iter().filter(retain).collect::<Vec<_>>();
    let current = current_untracked.iter().filter(retain).collect::<Vec<_>>();
    if current != expected {
        return Ok(false);
    }

    let snapshot_root = metadata_dir(repo, ui)?
        .join("baselines")
        .join(&baseline.snapshot_id);
    for path in expected {
        let relative = safe_relative_path(path)?;
        if !paths_equal(&snapshot_root.join(relative), &repo.join(relative))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn diff_matches<U: Ui>(
    repo: &Path,
    reference: &str,
    cached: bool,
    excluded_prefix: Option<&str>,
    ui: &U,
) -> Result<bool> {
    let mut args = vec!["diff".to_owned()];
    if cached {
        args.push("--cached".to_owned());
    }
    args.extend(["--quiet".to_owned(), reference.to_owned(), "--".to_owned()]);
    if let Some(prefix) = excluded_prefix {
        args.push(".".to_owned());
        args.push(format!(":(exclude){prefix}**"));
    }
    let output = ProcessRunner::new(ui).run(
        &CommandSpec::new("git", repo).args(args),
        "Comparing pre-existing working-tree state",
    )?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!("could not compare the current repository with its pre-Propose baseline"),
    }
}

pub struct RollbackReport {
    pub recovery_ref: Option<String>,
    pub diagnostic_path: PathBuf,
}

pub fn reset_to_slice_baseline<U: Ui>(
    repo: &Path,
    baseline: &SliceBaseline,
    ui: &U,
) -> Result<RollbackReport> {
    let runner = ProcessRunner::new(ui);
    let baseline_head = &baseline.head;
    let recovery_id = Uuid::new_v4();
    let directory = metadata_dir(repo, ui)?.join("recovery");
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "could not create recovery directory `{}`",
            directory.display()
        )
    })?;
    let diagnostic_path = directory.join(format!("{recovery_id}.txt"));

    let status = runner.checked(
        &CommandSpec::new("git", repo).args(["status", "--short"]),
        "Capturing failed worker status",
    )?;
    let diff = runner.checked(
        &CommandSpec::new("git", repo).args(["diff", "--binary", baseline_head, "--"]),
        "Capturing failed worker diff",
    )?;
    fs::write(
        &diagnostic_path,
        format!(
            "baseline: {baseline_head}\n\nstatus:\n{}\n\ndiff:\n{}",
            status.stdout, diff.stdout
        ),
    )
    .with_context(|| {
        format!(
            "could not write recovery diagnostic `{}`",
            diagnostic_path.display()
        )
    })?;

    let head = current_head(repo, ui)?;
    let recovery_ref = head
        .as_deref()
        .filter(|head| *head != baseline_head)
        .map(|head| {
            let reference = format!("refs/opsx-build/recovery/{recovery_id}");
            runner
                .checked(
                    &CommandSpec::new("git", repo).args(["update-ref", &reference, head]),
                    "Preserving failed worker commits",
                )
                .map(|_| reference)
        })
        .transpose()?;

    let current_untracked = untracked_paths(repo, ui)?;
    let baseline_paths = baseline
        .untracked
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let created = current_untracked
        .iter()
        .filter(|path| !baseline_paths.contains(path.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    for path in &created {
        safe_relative_path(path)?;
    }

    runner.checked(
        &CommandSpec::new("git", repo).args(["reset", "--hard", baseline_head]),
        "Restoring pre-Propose Git baseline",
    )?;
    if !created.is_empty() {
        let mut args = vec!["clean".to_owned(), "-f".to_owned(), "--".to_owned()];
        args.extend(created);
        runner.checked(
            &CommandSpec::new("git", repo).args(args),
            "Removing files created by the failed worker",
        )?;
    }
    let snapshot_root = metadata_dir(repo, ui)?
        .join("baselines")
        .join(&baseline.snapshot_id);
    for path in &baseline.untracked {
        let relative = safe_relative_path(path)?;
        copy_path(&snapshot_root.join(relative), &repo.join(relative))?;
    }
    if let Some(stash) = &baseline.tracked_stash {
        runner.checked(
            &CommandSpec::new("git", repo).args(["stash", "apply", "--index", stash]),
            "Restoring pre-Propose tracked edits",
        )?;
    }
    let restored_status = repository_status(repo, ui)?;
    if restored_status != baseline.status || !baseline_matches_except(repo, baseline, None, ui)? {
        bail!(
            "rollback could not reproduce the pre-Propose Git-visible working-tree state; failed-attempt diagnostics remain at `{}`",
            diagnostic_path.display()
        );
    }
    Ok(RollbackReport {
        recovery_ref,
        diagnostic_path,
    })
}

fn safe_relative_path(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!(
            "Git returned unsafe repository-relative path `{}`",
            path.display()
        );
    }
    Ok(path)
}

fn copy_path(source: &Path, destination: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect `{}`", source.display()));
        }
    };
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create `{}`", parent.display()))?;
    }
    match fs::symlink_metadata(destination) {
        Ok(existing) if existing.is_dir() => fs::remove_dir_all(destination)?,
        Ok(_) => fs::remove_file(destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not inspect `{}`", destination.display()));
        }
    }
    if metadata.file_type().is_symlink() {
        copy_symlink(source, destination)
    } else if metadata.is_file() {
        fs::copy(source, destination)
            .map(|_| ())
            .with_context(|| format!("could not copy `{}`", source.display()))
    } else {
        Ok(())
    }
}

fn paths_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata = match fs::symlink_metadata(left) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect `{}`", left.display()));
        }
    };
    let right_metadata = match fs::symlink_metadata(right) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("could not inspect `{}`", right.display()));
        }
    };
    if left_metadata.file_type().is_symlink() && right_metadata.file_type().is_symlink() {
        return Ok(fs::read_link(left)? == fs::read_link(right)?);
    }
    if left_metadata.is_file() && right_metadata.is_file() {
        return Ok(fs::read(left)? == fs::read(right)?);
    }
    Ok(false)
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source)
        .with_context(|| format!("could not read symlink `{}`", source.display()))?;
    std::os::unix::fs::symlink(target, destination)
        .with_context(|| format!("could not create symlink `{}`", destination.display()))
}

#[cfg(not(unix))]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination)
        .map(|_| ())
        .with_context(|| format!("could not copy `{}`", source.display()))
}

pub fn metadata_dir<U: Ui>(repo: &Path, ui: &U) -> Result<PathBuf> {
    let path = metadata_path(repo, ui, "opsx-build")?;
    fs::create_dir_all(&path)
        .with_context(|| format!("could not create metadata directory `{}`", path.display()))?;
    Ok(path)
}

pub fn legacy_metadata_dir<U: Ui>(repo: &Path, ui: &U) -> Result<PathBuf> {
    metadata_path(repo, ui, "ospx-build")
}

fn metadata_path<U: Ui>(repo: &Path, ui: &U, name: &str) -> Result<PathBuf> {
    let spec = CommandSpec::new("git", repo).args(["rev-parse", "--git-path", name]);
    let output = ProcessRunner::new(ui).checked(&spec, "Locating run metadata")?;
    let path = PathBuf::from(output.stdout.trim());
    Ok(if path.is_absolute() {
        path
    } else {
        repo.join(path)
    })
}

pub fn remove_metadata<U: Ui>(repo: &Path, ui: &U) -> Result<bool> {
    let mut removed = false;
    for path in [
        metadata_dir(repo, ui)?.join("last-run.json"),
        legacy_metadata_dir(repo, ui)?.join("last-run.json"),
    ] {
        match fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not remove checkpoint `{}`", path.display()));
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use crate::ui::TerminalUi;

    use super::*;

    fn fixture() -> PathBuf {
        let repo = std::env::temp_dir().join(format!("opsx-build-git-{}", Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.name", "opsx-build test"]);
        git(
            &repo,
            &["config", "user.email", "opsx-build@example.invalid"],
        );
        fs::write(repo.join("tracked.txt"), "committed\n").unwrap();
        fs::write(repo.join("staged.txt"), "committed\n").unwrap();
        git(&repo, &["add", "tracked.txt", "staged.txt"]);
        git(&repo, &["commit", "-qm", "baseline"]);
        repo
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn rollback_restores_a_dirty_baseline_and_preserves_failed_commits() {
        let repo = fixture();
        let ui = TerminalUi::new(false, false, false);
        fs::write(repo.join("tracked.txt"), "pre-existing unstaged edit\n").unwrap();
        fs::write(repo.join("staged.txt"), "pre-existing staged edit\n").unwrap();
        git(&repo, &["add", "staged.txt"]);
        fs::write(repo.join("notes.txt"), "pre-existing untracked note\n").unwrap();

        let baseline = capture_slice_baseline(&repo, &ui).unwrap();

        fs::write(repo.join("tracked.txt"), "worker rewrote it\n").unwrap();
        fs::write(repo.join("staged.txt"), "worker rewrote this too\n").unwrap();
        fs::write(repo.join("notes.txt"), "worker overwrote the note\n").unwrap();
        fs::write(repo.join("worker.txt"), "failed implementation\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "failed worker commit"]);
        fs::write(repo.join("scratch.tmp"), "uncommitted worker debris\n").unwrap();

        let report = reset_to_slice_baseline(&repo, &baseline, &ui).unwrap();

        assert_eq!(
            current_head(&repo, &ui).unwrap().as_deref(),
            Some(&*baseline.head)
        );
        assert_eq!(repository_status(&repo, &ui).unwrap(), baseline.status);
        assert_eq!(
            fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "pre-existing unstaged edit\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("staged.txt")).unwrap(),
            "pre-existing staged edit\n"
        );
        assert_eq!(
            fs::read_to_string(repo.join("notes.txt")).unwrap(),
            "pre-existing untracked note\n"
        );
        assert!(!repo.join("worker.txt").exists());
        assert!(!repo.join("scratch.tmp").exists());
        assert!(report.recovery_ref.is_some());
        assert!(report.diagnostic_path.is_file());
        fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn frontier_comparison_allows_agenda_commits_but_detects_changed_user_edits() {
        let repo = fixture();
        let ui = TerminalUi::new(false, false, false);
        fs::create_dir_all(repo.join("automation/slices")).unwrap();
        fs::write(
            repo.join("automation/slices/0009-conditionals.md"),
            "# Conditionals\n",
        )
        .unwrap();
        git(&repo, &["add", "automation/slices/0009-conditionals.md"]);
        git(&repo, &["commit", "-qm", "add agenda"]);

        fs::write(repo.join("tracked.txt"), "pre-existing edit\n").unwrap();
        fs::write(repo.join("staged.txt"), "pre-existing staged edit\n").unwrap();
        git(&repo, &["add", "staged.txt"]);
        fs::write(repo.join("notes.txt"), "pre-existing untracked note\n").unwrap();
        let baseline = capture_slice_baseline(&repo, &ui).unwrap();

        fs::write(
            repo.join("automation/slices/0009-conditionals.md"),
            "# Smaller conditionals\n",
        )
        .unwrap();
        git(&repo, &["add", "automation/slices/0009-conditionals.md"]);
        git(
            &repo,
            &[
                "commit",
                "-qm",
                "replan agenda",
                "--",
                "automation/slices/0009-conditionals.md",
            ],
        );

        assert!(
            baseline_matches_except(&repo, &baseline, Some("automation/slices/"), &ui).unwrap()
        );
        fs::write(repo.join("tracked.txt"), "frontier changed the user edit\n").unwrap();
        assert!(
            !baseline_matches_except(&repo, &baseline, Some("automation/slices/"), &ui).unwrap()
        );
        fs::remove_dir_all(repo).unwrap();
    }
}
