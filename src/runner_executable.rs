//! Keep same-version helper commands reachable while the installed path changes.
use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    git::metadata_dir,
    process::{CommandSpec, ProcessRunner},
    ui::Ui,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunnerExecutable {
    pub path: PathBuf,
    version: String,
    hash: String,
}

impl RunnerExecutable {
    pub(crate) fn ensure<U: Ui>(cached: &mut Option<Self>, repo: &Path, ui: &U) -> Result<()> {
        if let Some(executable) = cached
            && executable.version == env!("CARGO_PKG_VERSION")
            && executable.path.is_file()
            && fingerprint(repo, &executable.path, ui)? == executable.hash
        {
            return Ok(());
        }
        let source =
            std::env::current_exe().context("cannot locate the running runner executable")?;
        // Linux keeps an unlinked running image here. Other supported platforms
        // copy and version-check the executable before invoking any agents.
        #[cfg(target_os = "linux")]
        let source = if Path::new("/proc/self/exe").exists() {
            PathBuf::from("/proc/self/exe")
        } else {
            source
        };
        *cached = Some(Self::copy_from(repo, &source, ui)?);
        Ok(())
    }

    fn copy_from<U: Ui>(repo: &Path, source: &Path, ui: &U) -> Result<Self> {
        let directory = metadata_dir(repo, ui)?
            .join("runner")
            .join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("opsx-build{}", std::env::consts::EXE_SUFFIX));
        let result = (|| {
            fs::copy(source, &path).with_context(|| {
                format!("cannot preserve runner executable `{}`", source.display())
            })?;
            let version = ProcessRunner::new(ui).checked(
                &CommandSpec::new(path.to_string_lossy(), repo).args(["--version"]),
                "Checking preserved runner version",
            )?;
            let expected = format!("opsx-build {}", env!("CARGO_PKG_VERSION"));
            if version.stdout.trim() != expected {
                bail!(
                    "preserved runner version differs from this running process; expected `{expected}`, received `{}`",
                    version.stdout.trim()
                );
            }
            Ok(Self {
                hash: fingerprint(repo, &path, ui)?,
                path,
                version: env!("CARGO_PKG_VERSION").to_owned(),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&directory);
        }
        result
    }
}

fn fingerprint<U: Ui>(repo: &Path, path: &Path, ui: &U) -> Result<String> {
    Ok(ProcessRunner::new(ui)
        .checked(
            &CommandSpec::new("git", repo)
                .args(["hash-object", "--no-filters", "--"])
                .args([path.to_string_lossy()]),
            "Fingerprinting preserved runner",
        )?
        .stdout
        .trim()
        .to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::usage::tests::RecordingUi;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pinned_version_survives_replacement_and_cache_detects_changed_bytes() {
        let repo = std::env::temp_dir().join(format!("opsx-pin-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        let source = repo.join("installed-runner");
        fs::write(
            &source,
            format!(
                "#!/bin/sh\nprintf 'opsx-build {}\\n'\n",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755)).unwrap();
        let ui = RecordingUi::default();
        let pinned = RunnerExecutable::copy_from(&repo, &source, &ui).unwrap();
        fs::rename(&source, repo.join("old-install")).unwrap();
        fs::write(&source, "new incompatible version\n").unwrap();
        let output = std::process::Command::new(&pinned.path)
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("opsx-build {}", env!("CARGO_PKG_VERSION"))
        );
        let mut cached = Some(pinned.clone());
        RunnerExecutable::ensure(&mut cached, &repo, &ui).unwrap();
        assert_eq!(cached.as_ref().unwrap().path, pinned.path);
        fs::write(&pinned.path, "changed\n").unwrap();
        assert_ne!(fingerprint(&repo, &pinned.path, &ui).unwrap(), pinned.hash);
        fs::remove_dir_all(repo).unwrap();
    }
}
