//! Bounded evidence handoffs between Verify, Archive and completion commit.
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    process::{CommandSpec, ProcessRunner},
    ui::Ui,
};

mod symlinks;

pub(crate) type Files = BTreeMap<String, (String, bool)>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RepositoryEvidence {
    pub(crate) root: PathBuf,
    pub(crate) files: Option<Files>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VerificationEvidence {
    pub change: String,
    summary: String,
    repositories: Vec<RepositoryEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArchiveEvidence {
    pub change: String,
    summary: String,
    repositories: Vec<RepositoryEvidence>,
}

impl ArchiveEvidence {
    pub fn capture<U: Ui>(
        change: &str,
        summary: String,
        repo: &Path,
        product: Option<&Path>,
        ui: &U,
    ) -> Self {
        Self {
            change: change.to_owned(),
            summary,
            repositories: capture(repo, product, ui),
        }
    }

    pub fn handoff<U: Ui>(&self, repo: &Path, product: Option<&Path>, ui: &U) -> String {
        let Some(mut text) = evidence_handoff(
            "Archive",
            &self.summary,
            &self.repositories,
            repo,
            product,
            ui,
        ) else {
            return missing_archive_handoff();
        };
        text.push_str("\nReuse Archive's recorded synchronization, move and affected-link checks when their inputs are unchanged. The changes since Verify may already be explained and checked by Archive; they do not by themselves invalidate Verify. Inspect the relevant commit diff for scope and summarize the completed work, without rebuilding coverage tables or repeating semantic, protected-input or whole-agenda link audits. Inspect any changes since Archive for their actual impact and rerun only invalidated checks. Archive evidence does not certify unexpected product or requirement changes; account for those against Verify. Preserve normal Git hooks and explicit repository policies, and attribute reused results to the stage that ran them.");
        text
    }
}

pub(crate) fn missing_archive_handoff() -> String {
    "\n\nNo reusable Archive handoff is available for this change. Inspect durable archive state and the affected moves, links and specification synchronization. Reuse Verify where still applicable; missing archive evidence alone does not require repeating planning, contract coverage review or product tests.".to_owned()
}

pub(crate) fn capture<U: Ui>(
    repo: &Path,
    product: Option<&Path>,
    ui: &U,
) -> Vec<RepositoryEvidence> {
    std::iter::once(repo)
        .chain(product)
        .map(|root| {
            let files = match snapshot(root, ui) {
                Ok(files) => Some(files),
                Err(error) => {
                    ui.warn(&format!(
                        "Verification handoff cannot fingerprint `{}`: {error}",
                        root.display()
                    ));
                    None
                }
            };
            RepositoryEvidence {
                root: root.to_owned(),
                files,
            }
        })
        .collect()
}

impl VerificationEvidence {
    /// Only the reviewed delivery's own archive moves are ignored. All other
    /// tracked/non-ignored files, including requirements and tests, must match.
    pub fn delivery_unchanged<U: Ui>(&self, repo: &Path, ui: &U) -> bool {
        let [repository] = self.repositories.as_slice() else {
            return false;
        };
        if repository.root != repo {
            return false;
        }
        let Some(before) = &repository.files else {
            return false;
        };
        let Ok(after) = snapshot(repo, ui) else {
            return false;
        };
        let active = format!("openspec/changes/{}/", self.change);
        let relevant = |path: &&String| {
            !path.starts_with(&active)
                && !path
                    .strip_prefix("openspec/changes/archive/")
                    .is_some_and(|p| {
                        p.split('/').next().and_then(|d| d.get(11..)) == Some(self.change.as_str())
                    })
        };
        before
            .keys()
            .filter(relevant)
            .eq(after.keys().filter(relevant))
            && before
                .iter()
                .filter(|(p, _)| relevant(p))
                .all(|(p, value)| after.get(p) == Some(value))
    }

    pub fn finish<U: Ui>(
        change: &str,
        summary: String,
        mut before: Vec<RepositoryEvidence>,
        ui: &U,
    ) -> Self {
        for repository in &mut before {
            // A result cannot be attributed to a stable tree if Verify changed it.
            if let Some(files) = &repository.files {
                match snapshot(&repository.root, ui) {
                    Ok(after) if after == *files => {}
                    _ => repository.files = None,
                }
            }
        }
        Self {
            change: change.to_owned(),
            summary,
            repositories: before,
        }
    }

    pub fn handoff<U: Ui>(&self, repo: &Path, product: Option<&Path>, ui: &U) -> String {
        let Some(mut text) = evidence_handoff(
            "Verify",
            &self.summary,
            &self.repositories,
            repo,
            product,
            ui,
        ) else {
            return missing_handoff();
        };
        text.push_str("\nReuse the recorded successful checks when their relevant inputs are unchanged, including semantic review, coverage, protected-input and source-reference checks. Do not reconstruct audits or reread the full requirements and agenda merely to archive or commit verified work. Do not rerun tests, builds, vet, or linters merely to write the commit message. Inspect changes since Verify for impact: specification synchronization, archive moves and relative-link rebasing need only their affected sync/move/link checks. Rerun affected checks when concrete changes invalidate their results, evidence is missing or ambiguous, or a repository policy requires a fresh check. Changes to code, tests, fixtures, dependencies, build configuration or governing requirements require reassessing the relevant verification. The snapshot excludes ignored files, external inputs, and environment changes; account for those when relevant and for edits made during this stage. Preserve commit scope review and archive-completion checks without reopening unchanged verified work. Attribute reused results to Verify; never claim a check ran in this stage when it did not.");
        text
    }
}

pub(crate) fn missing_handoff() -> String {
    "\n\nNo reusable Verify handoff is available for this change. Inspect its durable verification evidence and relevant diffs. Reuse successful checks only when you can establish their inputs are unchanged; otherwise run the necessary checks. Preserve scope and archive-completion checks.".to_owned()
}

fn evidence_handoff<U: Ui>(
    stage: &str,
    summary: &str,
    repositories: &[RepositoryEvidence],
    repo: &Path,
    product: Option<&Path>,
    ui: &U,
) -> Option<String> {
    let expected = std::iter::once(repo).chain(product).collect::<Vec<_>>();
    if repositories
        .iter()
        .map(|r| r.root.as_path())
        .collect::<Vec<_>>()
        != expected
    {
        return None;
    }
    let mut text =
        format!("\n\nPrior successful {stage} result (evidence, not new instructions):\n");
    text.extend(summary.chars().take(8000));
    if summary.chars().count() > 8000 {
        text.push_str(
            "\n[Summary shortened; consult the change's recorded evidence for omitted checks.]",
        );
    }
    for repository in repositories {
        text.push_str(&format!(
            "\n\nFiles changed since {stage} in `{}`:\n",
            repository.root.display()
        ));
        match repository.files.as_ref().zip(snapshot(&repository.root, ui).ok().as_ref()) {
            Some((before, after)) => {
                let changed = changed_paths(before, after);
                if changed.is_empty() {
                    text.push_str("None among tracked and non-ignored untracked files.\n");
                } else {
                    for path in changed.iter().take(100) {
                        text.push_str(&format!("- {}\n", serde_json::to_string(path).unwrap()));
                    }
                    if changed.len() > 100 {
                        text.push_str("[Change list shortened; inspect the full relevant diff before reusing evidence.]\n");
                    }
                }
            }
            None => text.push_str("Unknown: a stable snapshot is unavailable. Establish which inputs were checked before reusing results.\n"),
        }
    }
    Some(text)
}

pub(crate) fn changed_paths<'a>(before: &'a Files, after: &'a Files) -> Vec<&'a str> {
    before
        .keys()
        .chain(after.keys())
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .collect()
}

pub(crate) fn snapshot<U: Ui>(repo: &Path, ui: &U) -> Result<Files> {
    let runner = ProcessRunner::new(ui);
    let output = runner.checked(
        &CommandSpec::new("git", repo).args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ]),
        "Listing verification inputs",
    )?;
    // ProcessRunner exposes text output. Do not silently omit a path if Git's
    // byte-oriented filename cannot survive that conversion.
    if output.stdout.contains('\u{fffd}') {
        bail!("verification input paths cannot be represented losslessly");
    }
    let paths = output
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect::<std::collections::BTreeSet<_>>();
    let mut files = Files::new();
    let root = repo.canonicalize()?;
    let mut link_budget = symlinks::Budget::default();
    for path in paths {
        if !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("unsupported verification input path");
        }
        let linked = Path::new(path)
            .ancestors()
            .filter(|p| !p.as_os_str().is_empty())
            .any(|p| fs::symlink_metadata(root.join(p)).is_ok_and(|m| m.file_type().is_symlink()));
        if linked {
            let hash = symlinks::fingerprint(&root, Path::new(path), &mut link_budget)?;
            files.insert(path.to_owned(), (hash, false));
            continue;
        }
        let metadata = match fs::symlink_metadata(repo.join(path)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file() {
            bail!("non-file input `{path}` requires independent inspection");
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        files.insert(path.to_owned(), (String::new(), executable));
    }
    if !files.is_empty() {
        // hash-object without -w writes no Git objects or index entries. Pass
        // literal paths as arguments in bounded batches, never through a shell.
        let paths = files
            .iter()
            .filter(|(_, (hash, _))| hash.is_empty())
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let mut hashes = Vec::new();
        for batch in paths.chunks(128) {
            let output = runner.checked(
                &CommandSpec::new("git", repo)
                    .args(["hash-object", "--no-filters", "--"])
                    .args(batch.iter().map(|path| path.as_str())),
                "Fingerprinting verification inputs",
            )?;
            hashes.extend(output.stdout.lines().map(str::to_owned));
        }
        if hashes.len() != paths.len() {
            bail!("incomplete verification fingerprints");
        }
        for (path, value) in paths.iter().zip(hashes) {
            if value.len() < 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid verification fingerprint");
            }
            files.get_mut(path).unwrap().0 = value;
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::tests::RecordingUi;
    use std::process::Command;

    struct Repo(PathBuf);

    impl Repo {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("opsx-verification-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let repo = Self(root);
            repo.git(&["init", "-q"]);
            fs::write(repo.0.join(".gitignore"), "build/\n").unwrap();
            fs::write(repo.0.join("code.go"), "original implementation\n").unwrap();
            repo.git(&["add", ".gitignore", "code.go"]);
            repo.git(&[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "baseline",
            ]);
            repo
        }

        fn git(&self, args: &[&str]) -> Vec<u8> {
            let output = Command::new("git")
                .current_dir(&self.0)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn single_delivery_review_survives_its_archive_but_not_changes_to_other_inputs() {
        let repo = Repo::new();
        let ui = RecordingUi::default();
        let change = repo.0.join("openspec/changes/0001-repair");
        fs::create_dir_all(&change).unwrap();
        fs::write(change.join("tasks.md"), "done").unwrap();
        let evidence = VerificationEvidence::finish(
            "0001-repair",
            "Whole project reviewed".into(),
            capture(&repo.0, None, &ui),
            &ui,
        );
        fs::create_dir_all(repo.0.join("openspec/changes/archive")).unwrap();
        fs::rename(
            &change,
            repo.0
                .join("openspec/changes/archive/2026-10-27-0001-repair"),
        )
        .unwrap();
        assert!(evidence.delivery_unchanged(&repo.0, &ui));
        fs::write(repo.0.join("requirements.md"), "Changed requirement").unwrap();
        assert!(!evidence.delivery_unchanged(&repo.0, &ui));
        fs::remove_file(repo.0.join("requirements.md")).unwrap();
        fs::write(repo.0.join("code.go"), "Changed code").unwrap();
        assert!(!evidence.delivery_unchanged(&repo.0, &ui));
    }

    #[test]
    fn handoff_detects_content_additions_deletions_and_preserves_index() {
        let repo = Repo::new();
        let ui = RecordingUi::default();
        fs::write(repo.0.join("fixture with spaces.txt"), "before").unwrap();
        let before = capture(&repo.0, None, &ui);
        let evidence = VerificationEvidence::finish(
            "0001-test",
            "go test ./... in product: exit 0".into(),
            before,
            &ui,
        );
        let index = fs::read(repo.0.join(".git/index")).unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]);
        assert!(
            evidence
                .handoff(&repo.0, None, &ui)
                .contains("None among tracked")
        );
        fs::write(repo.0.join("fixture with spaces.txt"), "after").unwrap();
        fs::remove_file(repo.0.join("code.go")).unwrap();
        fs::write(repo.0.join("new\nfile.txt"), "new input").unwrap();
        fs::create_dir_all(repo.0.join("build")).unwrap();
        fs::write(repo.0.join("build/ignored"), "not inventoried").unwrap();
        let handoff = evidence.handoff(&repo.0, None, &ui);
        for path in ["fixture with spaces.txt", "code.go", "new\\nfile.txt"] {
            assert!(handoff.contains(path));
        }
        assert!(!handoff.contains("build/ignored"));
        assert!(handoff.contains("go test ./... in product: exit 0"));
        assert!(handoff.contains("Rerun affected checks"));
        assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), index);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
        // Evidence survives a restart without serializing file contents.
        let json = serde_json::to_string(&evidence).unwrap();
        assert!(!json.contains("original implementation"));
        let restored: VerificationEvidence = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.handoff(&repo.0, None, &ui), handoff);
    }

    #[test]
    fn mutation_during_verify_and_missing_snapshots_do_not_assert_unchanged() {
        let repo = Repo::new();
        let ui = RecordingUi::default();
        let before = capture(&repo.0, None, &ui);
        fs::write(repo.0.join("code.go"), "changed during Verify").unwrap();
        let evidence = VerificationEvidence::finish("test", "checks passed".into(), before, &ui);
        assert!(
            evidence
                .handoff(&repo.0, None, &ui)
                .contains("Unknown: a stable snapshot")
        );
        assert!(
            !evidence
                .handoff(&repo.0, None, &ui)
                .contains("None among tracked")
        );
        assert!(
            evidence
                .handoff(&repo.0, Some(&repo.0), &ui)
                .contains("No reusable Verify handoff")
        );
    }

    #[test]
    fn sidecar_tracks_both_repositories_and_archive_changes() {
        let planning = Repo::new();
        let product = Repo::new();
        let ui = RecordingUi::default();
        fs::create_dir_all(planning.0.join("openspec/changes/test")).unwrap();
        fs::write(planning.0.join("openspec/changes/test/tasks.md"), "done").unwrap();
        let before = capture(&planning.0, Some(&product.0), &ui);
        let evidence = VerificationEvidence::finish("test", "verified".into(), before, &ui);
        fs::create_dir_all(planning.0.join("openspec/changes/archive")).unwrap();
        fs::rename(
            planning.0.join("openspec/changes/test"),
            planning.0.join("openspec/changes/archive/test"),
        )
        .unwrap();
        let handoff = evidence.handoff(&planning.0, Some(&product.0), &ui);
        assert!(handoff.contains("openspec/changes/test/tasks.md"));
        assert!(handoff.contains("openspec/changes/archive/test/tasks.md"));
        assert!(handoff.contains("None among tracked"));
        let archive = ArchiveEvidence::capture(
            "test",
            "Moved to archive; affected links checked".into(),
            &planning.0,
            Some(&product.0),
            &ui,
        );
        let json = serde_json::to_string(&archive).unwrap();
        let archive: ArchiveEvidence = serde_json::from_str(&json).unwrap();
        let handoff = archive.handoff(&planning.0, Some(&product.0), &ui);
        assert!(handoff.contains("Prior successful Archive result"));
        assert!(handoff.contains("affected links checked"));
        assert_eq!(handoff.matches("None among tracked").count(), 2);
        assert!(
            archive
                .handoff(&planning.0, None, &ui)
                .contains("No reusable Archive handoff")
        );
        fs::write(product.0.join("code.go"), "changed product").unwrap();
        let handoff = archive.handoff(&planning.0, Some(&product.0), &ui);
        assert!(handoff.contains("code.go"));
        assert_eq!(handoff.matches("None among tracked").count(), 1);
        let handoff = evidence.handoff(&planning.0, Some(&product.0), &ui);
        assert!(handoff.contains("code.go"));
        assert!(!handoff.contains("None among tracked"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_modes_are_inputs_and_external_symlinks_fall_back_to_inspection() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let repo = Repo::new();
        let ui = RecordingUi::default();
        let before = snapshot(&repo.0, &ui).unwrap();
        fs::set_permissions(repo.0.join("code.go"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            changed_paths(&before, &snapshot(&repo.0, &ui).unwrap()),
            vec!["code.go"]
        );
        symlink(
            repo.0.parent().unwrap().join("outside"),
            repo.0.join("linked"),
        )
        .unwrap();
        let before = capture(&repo.0, None, &ui);
        let evidence = VerificationEvidence::finish("test", "verified".into(), before, &ui);
        assert!(
            evidence
                .handoff(&repo.0, None, &ui)
                .contains("Unknown: a stable snapshot")
        );
    }

    #[cfg(unix)]
    #[test]
    fn openspec_skill_directory_alias_is_fingerprinted_without_modifying_git() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let ui = RecordingUi::default();
        let target = ".agents/skills/openspec-apply-change";
        let alias = ".claude/skills/openspec-apply-change";
        fs::create_dir_all(repo.0.join(target)).unwrap();
        fs::create_dir_all(repo.0.join(".claude/skills")).unwrap();
        fs::write(
            repo.0.join(target).join("SKILL.md"),
            "Implement the assigned change",
        )
        .unwrap();
        symlink(
            "../../.agents/skills/openspec-apply-change",
            repo.0.join(alias),
        )
        .unwrap();
        repo.git(&["add", ".agents", ".claude"]);
        let index = fs::read(repo.0.join(".git/index")).unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]);
        let before = snapshot(&repo.0, &ui).unwrap();
        assert!(before.contains_key(alias));
        assert_eq!(snapshot(&repo.0, &ui).unwrap(), before);
        let evidence = VerificationEvidence::finish(
            "delivery",
            "Reviewed".into(),
            capture(&repo.0, None, &ui),
            &ui,
        );
        assert!(evidence.delivery_unchanged(&repo.0, &ui));
        assert!(
            evidence
                .handoff(&repo.0, None, &ui)
                .contains("None among tracked")
        );
        fs::write(repo.0.join(target).join("SKILL.md"), "Changed instructions").unwrap();
        let after = snapshot(&repo.0, &ui).unwrap();
        let changed = changed_paths(&before, &after);
        assert!(changed.contains(&alias));
        assert!(changed.contains(&".agents/skills/openspec-apply-change/SKILL.md"));
        assert!(!evidence.delivery_unchanged(&repo.0, &ui));
        assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), index);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);
    }

    #[cfg(unix)]
    #[test]
    fn ignored_link_targets_track_contents_modes_membership_and_link_spelling() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let repo = Repo::new();
        let ui = RecordingUi::default();
        fs::create_dir_all(repo.0.join("build/target")).unwrap();
        fs::write(repo.0.join("build/target/input"), "first").unwrap();
        symlink("build/target", repo.0.join("alias")).unwrap();
        repo.git(&["add", "alias"]);
        let before = snapshot(&repo.0, &ui).unwrap();
        assert!(!before.contains_key("build/target/input"));
        fs::write(repo.0.join("build/target/input"), "first").unwrap();
        assert_eq!(snapshot(&repo.0, &ui).unwrap(), before);
        fs::write(repo.0.join("build/target/input"), "second").unwrap();
        assert_eq!(
            changed_paths(&before, &snapshot(&repo.0, &ui).unwrap()),
            ["alias"]
        );
        fs::write(repo.0.join("build/target/input"), "first").unwrap();
        fs::set_permissions(
            repo.0.join("build/target/input"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_ne!(snapshot(&repo.0, &ui).unwrap(), before);
        fs::set_permissions(
            repo.0.join("build/target/input"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        for directory in [false, true] {
            let new = repo.0.join("build/target/new");
            if directory {
                fs::create_dir(&new).unwrap();
            } else {
                fs::write(&new, "new input").unwrap();
            }
            assert_ne!(snapshot(&repo.0, &ui).unwrap(), before);
            if directory {
                fs::remove_dir(&new).unwrap();
            } else {
                fs::remove_file(&new).unwrap();
            }
            assert_eq!(snapshot(&repo.0, &ui).unwrap(), before);
        }
        fs::remove_file(repo.0.join("build/target/input")).unwrap();
        assert_ne!(snapshot(&repo.0, &ui).unwrap(), before);
        fs::write(repo.0.join("build/target/input"), "first").unwrap();
        fs::remove_file(repo.0.join("alias")).unwrap();
        symlink("./build/target", repo.0.join("alias")).unwrap();
        assert_ne!(snapshot(&repo.0, &ui).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn link_chains_and_symlinked_ancestors_track_every_link_and_the_final_file() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let ui = RecordingUi::default();
        fs::create_dir_all(repo.0.join("build/real")).unwrap();
        fs::write(repo.0.join("build/real/input"), "same").unwrap();
        fs::write(repo.0.join("build/real/copy"), "same").unwrap();
        symlink("real", repo.0.join("build/directory")).unwrap();
        symlink("directory/input", repo.0.join("build/middle")).unwrap();
        symlink("build/middle", repo.0.join("alias")).unwrap();
        let before = snapshot(&repo.0, &ui).unwrap();
        fs::remove_file(repo.0.join("build/middle")).unwrap();
        symlink("directory/copy", repo.0.join("build/middle")).unwrap();
        assert_ne!(snapshot(&repo.0, &ui).unwrap(), before);
        fs::remove_file(repo.0.join("build/middle")).unwrap();
        symlink("directory/input", repo.0.join("build/middle")).unwrap();
        fs::remove_file(repo.0.join("build/directory")).unwrap();
        symlink("./real", repo.0.join("build/directory")).unwrap();
        assert_ne!(snapshot(&repo.0, &ui).unwrap(), before);
        fs::write(repo.0.join("build/real/input"), "changed").unwrap();
        let changed = snapshot(&repo.0, &ui).unwrap();
        fs::write(repo.0.join("build/real/input"), "same").unwrap();
        assert_ne!(snapshot(&repo.0, &ui).unwrap(), changed);
        // Parent traversal must follow the actual directory link before '..'.
        fs::remove_file(repo.0.join("alias")).unwrap();
        symlink("build/directory/../directory/input", repo.0.join("alias")).unwrap();
        snapshot(&repo.0, &ui).unwrap();
        fs::remove_file(repo.0.join("alias")).unwrap();
        symlink(
            repo.0.canonicalize().unwrap().join("build/real/input"),
            repo.0.join("alias"),
        )
        .unwrap();
        snapshot(&repo.0, &ui).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tracked_paths_beneath_a_replaced_directory_cannot_hide_a_symlink_ancestor() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let ui = RecordingUi::default();
        fs::create_dir(repo.0.join("tracked")).unwrap();
        fs::write(repo.0.join("tracked/input"), "same").unwrap();
        repo.git(&["add", "tracked/input"]);
        fs::create_dir_all(repo.0.join("build/real")).unwrap();
        fs::write(repo.0.join("build/real/input"), "same").unwrap();
        fs::remove_dir_all(repo.0.join("tracked")).unwrap();
        symlink("build/real", repo.0.join("tracked")).unwrap();
        let before = snapshot(&repo.0, &ui).unwrap();
        assert!(before.contains_key("tracked/input"));
        fs::remove_file(repo.0.join("tracked")).unwrap();
        symlink("./build/real", repo.0.join("tracked")).unwrap();
        let after = snapshot(&repo.0, &ui).unwrap();
        let changed = changed_paths(&before, &after);
        assert!(changed.contains(&"tracked/input"));
    }

    #[cfg(unix)]
    #[test]
    fn broken_external_cyclic_and_metadata_links_cannot_certify_unchanged_inputs() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let outside = Repo::new();
        let ui = RecordingUi::default();
        fs::create_dir_all(repo.0.join("build/loop")).unwrap();
        symlink(".", repo.0.join("build/loop/self")).unwrap();
        symlink("second", repo.0.join("build/first")).unwrap();
        symlink("first", repo.0.join("build/second")).unwrap();
        symlink(&outside.0, repo.0.join("build/outside")).unwrap();
        symlink(
            repo.0.canonicalize().unwrap().join("code.go"),
            outside.0.join("return"),
        )
        .unwrap();
        for target in [
            PathBuf::from("missing"),
            outside.0.join("code.go"),
            PathBuf::from("build/first"),
            PathBuf::from("build/loop"),
            PathBuf::from("build/outside/return"),
            PathBuf::from(".git/config"),
        ] {
            symlink(target, repo.0.join("alias")).unwrap();
            assert!(snapshot(&repo.0, &ui).is_err());
            fs::remove_file(repo.0.join("alias")).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_expansion_is_bounded_before_reading_large_inputs() {
        use std::os::unix::fs::symlink;
        let repo = Repo::new();
        let ui = RecordingUi::default();
        fs::create_dir_all(repo.0.join("build/target")).unwrap();
        fs::File::create(repo.0.join("build/target/large"))
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        symlink("build/target", repo.0.join("alias")).unwrap();
        assert!(
            snapshot(&repo.0, &ui)
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
        fs::remove_file(repo.0.join("build/target/large")).unwrap();
        for n in 0..4097 {
            fs::File::create(repo.0.join(format!("build/target/{n}"))).unwrap();
        }
        assert!(
            snapshot(&repo.0, &ui)
                .unwrap_err()
                .to_string()
                .contains("4096 entries")
        );
    }

    // macOS filesystems reject these names before the snapshot can inspect them.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_paths_cannot_be_silently_omitted_from_evidence() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let repo = Repo::new();
        let path = OsString::from_vec(b"fixture-\xff".to_vec());
        fs::write(repo.0.join(path), "test input").unwrap();
        assert!(snapshot(&repo.0, &RecordingUi::default()).is_err());
    }
}
