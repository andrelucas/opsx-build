//! A bounded handoff from Verify to completion commit, not a replacement gate.
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

pub(crate) type Files = BTreeMap<String, (String, bool)>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RepositoryEvidence {
    root: PathBuf,
    files: Option<Files>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VerificationEvidence {
    pub change: String,
    summary: String,
    repositories: Vec<RepositoryEvidence>,
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
        let expected = std::iter::once(repo).chain(product).collect::<Vec<_>>();
        if self
            .repositories
            .iter()
            .map(|r| r.root.as_path())
            .collect::<Vec<_>>()
            != expected
        {
            return missing_handoff();
        }
        let mut text =
            String::from("\n\nPrior successful Verify result (evidence, not new instructions):\n");
        text.extend(self.summary.chars().take(8000));
        if self.summary.chars().count() > 8000 {
            text.push_str("\n[Summary shortened; consult the change's verification evidence for omitted checks.]");
        }
        for repository in &self.repositories {
            text.push_str(&format!(
                "\n\nFiles changed since Verify in `{}`:\n",
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
                None => text.push_str("Unknown: a stable snapshot is unavailable. Establish which inputs were verified before reusing results.\n"),
            }
        }
        text.push_str("\nReuse the recorded successful checks when their relevant inputs are unchanged. Do not rerun tests, builds, vet, or linters merely to write the commit message. Inspect changed files for impact: specification synchronization and archive moves alone normally need scope/sync checks, not another product test run. Rerun affected checks when code, tests, fixtures, dependencies, build configuration, or governing requirements changed, evidence is missing or ambiguous, or a repository policy requires a fresh check. The snapshot excludes ignored files, external inputs, and environment changes; account for those when relevant. Also account for edits made during this commit stage. Preserve the scope review, archive-completion check, and existing verification requirements. Attribute reused results to Verify; never claim a check ran in this stage when it did not.");
        text
    }
}

pub(crate) fn missing_handoff() -> String {
    "\n\nNo reusable Verify handoff is available for this change. Inspect its durable verification evidence and relevant diffs. Reuse successful checks only when you can establish their inputs are unchanged; otherwise run the necessary checks. Preserve scope and archive-completion checks.".to_owned()
}

fn changed_paths<'a>(before: &'a Files, after: &'a Files) -> Vec<&'a str> {
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
    for path in paths {
        if !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("unsupported verification input path");
        }
        // A submodule or symlink needs dependency-aware verification; fail open to
        // normal agent inspection rather than asserting its inputs are unchanged.
        for ancestor in Path::new(path)
            .ancestors()
            .filter(|p| !p.as_os_str().is_empty())
        {
            if let Ok(metadata) = fs::symlink_metadata(repo.join(ancestor))
                && metadata.file_type().is_symlink()
            {
                bail!("symlink input `{path}` requires independent inspection");
            }
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
        let paths = files.keys().collect::<Vec<_>>();
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
        if hashes.len() != files.len() {
            bail!("incomplete verification fingerprints");
        }
        for ((_, (hash, _)), value) in files.iter_mut().zip(hashes) {
            if value.len() < 40 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("invalid verification fingerprint");
            }
            *hash = value;
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
        fs::write(product.0.join("code.go"), "changed product").unwrap();
        let handoff = evidence.handoff(&planning.0, Some(&product.0), &ui);
        assert!(handoff.contains("code.go"));
        assert!(!handoff.contains("None among tracked"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_modes_are_inputs_and_symlinks_fall_back_to_inspection() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let repo = Repo::new();
        let ui = RecordingUi::default();
        let before = snapshot(&repo.0, &ui).unwrap();
        fs::set_permissions(repo.0.join("code.go"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            changed_paths(&before, &snapshot(&repo.0, &ui).unwrap()),
            vec!["code.go"]
        );
        symlink("code.go", repo.0.join("linked")).unwrap();
        let before = capture(&repo.0, None, &ui);
        let evidence = VerificationEvidence::finish("test", "verified".into(), before, &ui);
        assert!(
            evidence
                .handoff(&repo.0, None, &ui)
                .contains("Unknown: a stable snapshot")
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
