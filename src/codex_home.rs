use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

/// Only unattended runs use this home. Explicit CODEX_HOME values remain user-managed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IsolatedCodexHome {
    source: PathBuf,
    pub(crate) path: PathBuf,
}

impl IsolatedCodexHome {
    pub(crate) fn new(source: PathBuf) -> Self {
        Self {
            path: source.join("opsx-build"),
            source,
        }
    }

    pub(crate) fn prepare(&self) -> Result<()> {
        create_private_directory(&self.path)?;
        // Keep live references, especially for refreshed credentials. Never share
        // sessions, databases or other Codex runtime state.
        for name in [
            "config.toml",
            "auth.json",
            "AGENTS.md",
            "AGENTS.override.md",
            "requirements.toml",
            "skills",
            "plugins",
            "rules",
            "agents",
            "hooks.json",
        ] {
            let source = self.source.join(name);
            if source.exists() || matches!(name, "config.toml" | "auth.json") {
                link_shared(&source, &self.path.join(name))?;
            }
        }
        // Named configuration profiles live alongside config.toml.
        for entry in fs::read_dir(&self.source)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .ends_with(".config.toml")
            {
                link_shared(&entry.path(), &self.path.join(entry.file_name()))?;
            }
        }
        Ok(())
    }

    /// Import only the requested legacy rollout, leaving the original untouched.
    /// Publish atomically so simultaneous worker/frontier resumes cannot expose a
    /// partial file or overwrite history already advanced in the isolated home.
    pub(crate) fn import_session(&self, thread_id: &str) -> Result<Option<PathBuf>> {
        if uuid::Uuid::parse_str(thread_id).is_err() {
            return Ok(None);
        }
        for tree in ["sessions", "archived_sessions"] {
            let root = self.source.join(tree);
            let Some(source) = find_rollout(&root, thread_id)? else {
                continue;
            };
            let target = self.path.join("sessions").join(source.strip_prefix(&root)?);
            create_private_directory(target.parent().context("rollout has no parent")?)?;
            if target.try_exists()? {
                return Ok(Some(target));
            }
            let temporary = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| -> Result<()> {
                fs::copy(&source, &temporary)?;
                match fs::hard_link(&temporary, &target) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
                    Err(error) => Err(error.into()),
                }
            })();
            let _ = fs::remove_file(&temporary);
            result.with_context(|| format!("could not import Codex session `{thread_id}`"))?;
            return Ok(Some(target));
        }
        Ok(None)
    }
}

fn find_rollout(directory: &Path, thread_id: &str) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if let Some(path) = find_rollout(&entry.path(), thread_id)? {
                return Ok(Some(path));
            }
        } else if kind.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-")
                && (name.ends_with(&format!("-{thread_id}.jsonl"))
                    || name.ends_with(&format!("-{thread_id}.jsonl.zst")))
            {
                return Ok(Some(entry.path()));
            }
        }
    }
    Ok(None)
}

fn create_private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).with_context(|| {
        format!(
            "could not create Codex state directory `{}`",
            path.display()
        )
    })
}

fn link_shared(source: &Path, target: &Path) -> Result<()> {
    if fs::read_link(target).is_ok_and(|existing| existing == source) {
        return Ok(());
    }
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(source, target);
    #[cfg(not(unix))]
    let result: std::io::Result<()> = Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "automatic Codex home sharing requires Unix symlinks; configure CODEX_HOME explicitly",
    ));
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            // Another backend may have prepared the same home concurrently.
            if fs::read_link(target).is_ok_and(|existing| existing == source) {
                return Ok(());
            }
            bail!(
                "refusing to replace `{}` while preparing isolated Codex state; configure an explicit CODEX_HOME to manage your own state directory",
                target.display()
            )
        }
        Err(error) => Err(error)
            .with_context(|| format!("could not share Codex configuration `{}`", source.display())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_home() -> IsolatedCodexHome {
        IsolatedCodexHome::new(
            std::env::temp_dir().join(format!("opsx-codex-home-{}", uuid::Uuid::new_v4())),
        )
    }

    #[test]
    fn shares_configuration_and_live_credentials_but_not_session_state() {
        let home = test_home();
        fs::create_dir_all(home.source.join("skills")).unwrap();
        for name in [
            "config.toml",
            "auth.json",
            "worker.config.toml",
            "AGENTS.md",
            "state_5.sqlite",
            "history.jsonl",
        ] {
            fs::write(home.source.join(name), "original").unwrap();
        }
        fs::create_dir(home.source.join("sessions")).unwrap();
        home.prepare().unwrap();
        home.prepare().unwrap();
        for name in [
            "config.toml",
            "auth.json",
            "worker.config.toml",
            "AGENTS.md",
            "skills",
        ] {
            assert_eq!(
                fs::read_link(home.path.join(name)).unwrap(),
                home.source.join(name)
            );
        }
        for name in ["sessions", "state_5.sqlite", "history.jsonl"] {
            assert!(!home.path.join(name).exists());
        }
        assert_eq!(
            fs::metadata(&home.path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::write(home.source.join("auth.json"), "refreshed").unwrap();
        assert_eq!(
            fs::read_to_string(home.path.join("auth.json")).unwrap(),
            "refreshed"
        );
        fs::write(home.path.join("auth.json"), "refreshed again").unwrap();
        assert_eq!(
            fs::read_to_string(home.source.join("auth.json")).unwrap(),
            "refreshed again"
        );
        fs::remove_dir_all(home.source).unwrap();
    }

    #[test]
    fn does_not_overwrite_existing_private_configuration() {
        let home = test_home();
        fs::create_dir_all(&home.path).unwrap();
        fs::write(home.path.join("config.toml"), "custom").unwrap();
        assert!(
            home.prepare()
                .unwrap_err()
                .to_string()
                .contains("refusing to replace")
        );
        assert_eq!(
            fs::read_to_string(home.path.join("config.toml")).unwrap(),
            "custom"
        );
        fs::remove_dir_all(home.source).unwrap();
    }

    #[test]
    fn imports_only_requested_rollout_and_never_overwrites_resumed_history() {
        for tree in ["sessions/2026/09/22", "archived_sessions"] {
            let home = test_home();
            let source_dir = home.source.join(tree);
            fs::create_dir_all(&source_dir).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let filename = format!("rollout-2026-09-22T12-00-00-{id}.jsonl");
            let original = source_dir.join(&filename);
            fs::write(&original, "legacy history").unwrap();
            fs::write(source_dir.join("unrelated.jsonl"), "unrelated").unwrap();
            home.prepare().unwrap();
            assert!(
                home.import_session(&uuid::Uuid::new_v4().to_string())
                    .unwrap()
                    .is_none()
            );
            assert!(home.import_session("../bad").unwrap().is_none());
            let imported = home.import_session(&id).unwrap().unwrap();
            assert!(imported.starts_with(home.path.join("sessions")));
            assert!(!imported.is_symlink());
            assert_eq!(fs::read_to_string(&imported).unwrap(), "legacy history");
            assert!(!imported.parent().unwrap().join("unrelated.jsonl").exists());
            fs::write(&imported, "resumed history").unwrap();
            assert_eq!(home.import_session(&id).unwrap().unwrap(), imported);
            assert_eq!(fs::read_to_string(&imported).unwrap(), "resumed history");
            assert_eq!(fs::read_to_string(&original).unwrap(), "legacy history");
            fs::remove_dir_all(home.source).unwrap();
        }
    }
}
