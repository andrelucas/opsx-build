use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

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
