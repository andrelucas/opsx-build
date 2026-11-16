//! Bounded fingerprints for link text and the complete in-repository target.
use std::{
    collections::{BTreeSet, VecDeque},
    ffi::OsString,
    fs::{self, File, Metadata},
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(super) struct Budget {
    entries: usize,
    bytes: u64,
}

impl Budget {
    fn entry(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > 4096 {
            bail!("symlink target fingerprint budget exceeded (4096 entries)");
        }
        Ok(())
    }
}

fn field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
}

fn relative<'a>(root: &Path, path: &'a Path) -> Result<&'a Path> {
    let relative = path
        .strip_prefix(root)
        .context("symlink target leaves the repository")?;
    if relative.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
    }) {
        bail!("symlink target includes Git metadata; independent inspection required");
    }
    Ok(relative)
}

enum Pending {
    Part(OsString),
    EndLink(PathBuf),
}

// Resolve components ourselves so an intermediate symlink cannot silently leave
// the repository and return, or change its spelling without invalidating evidence.
fn resolve(root: &Path, path: &Path, digest: &mut Sha256, budget: &mut Budget) -> Result<PathBuf> {
    let mut pending = path
        .components()
        .map(|p| Pending::Part(p.as_os_str().to_owned()))
        .collect::<VecDeque<_>>();
    let mut current = root.to_owned();
    let mut links = BTreeSet::new();
    while let Some(part) = pending.pop_front() {
        let part = match part {
            Pending::Part(part) => part,
            Pending::EndLink(path) => {
                links.remove(&path);
                continue;
            }
        };
        if part == "." {
            continue;
        }
        if part == ".." {
            if current == root {
                bail!("symlink target leaves the repository");
            }
            current.pop();
            continue;
        }
        let next = current.join(part);
        relative(root, &next)?;
        let metadata = fs::symlink_metadata(&next)
            .with_context(|| format!("cannot inspect symlink target `{}`", next.display()))?;
        if metadata.file_type().is_symlink() {
            budget.entry()?;
            if !links.insert(next.clone()) {
                bail!("cyclic symlink resolution requires independent inspection");
            }
            let target = fs::read_link(&next)?;
            field(digest, b"link");
            field(
                digest,
                relative(root, &next)?.as_os_str().as_encoded_bytes(),
            );
            field(digest, target.as_os_str().as_encoded_bytes());
            let target = if target.is_absolute() {
                current = root.to_owned();
                relative(root, &target)?
            } else {
                target.as_path()
            };
            pending.push_front(Pending::EndLink(next));
            for part in target.components().rev() {
                match part {
                    Component::Normal(_) | Component::CurDir | Component::ParentDir => {
                        pending.push_front(Pending::Part(part.as_os_str().to_owned()));
                    }
                    _ => bail!("unsupported symlink target"),
                }
            }
        } else {
            if pending.iter().any(|part| matches!(part, Pending::Part(_))) && !metadata.is_dir() {
                bail!("non-directory symlink target ancestor requires independent inspection");
            }
            current = next;
        }
    }
    Ok(current)
}

fn executable(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

fn collect(
    root: &Path,
    path: &Path,
    digest: &mut Sha256,
    active: &mut BTreeSet<PathBuf>,
    budget: &mut Budget,
) -> Result<()> {
    budget.entry()?;
    let resolved = resolve(root, path, digest, budget)?;
    let metadata = fs::symlink_metadata(&resolved)?;
    field(
        digest,
        relative(root, &resolved)?.as_os_str().as_encoded_bytes(),
    );
    digest.update([u8::from(executable(&metadata))]);
    if metadata.is_file() {
        field(digest, b"file");
        let remaining = (64_u64 * 1024 * 1024).saturating_sub(budget.bytes);
        if metadata.len() > remaining {
            bail!("symlink target fingerprint budget exceeded (64 MiB)");
        }
        let mut bytes = Vec::new();
        File::open(&resolved)?
            .take(remaining + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > remaining {
            bail!("symlink target fingerprint budget exceeded (64 MiB)");
        }
        budget.bytes += bytes.len() as u64;
        field(digest, &bytes);
    } else if metadata.is_dir() {
        field(digest, b"directory");
        if !active.insert(resolved.clone()) {
            bail!("cyclic symlink directory requires independent inspection");
        }
        let remaining = 4096 - budget.entries.min(4096);
        let mut children = fs::read_dir(&resolved)?
            .take(remaining + 1)
            .collect::<std::io::Result<Vec<_>>>()?;
        if children.len() > remaining {
            bail!("symlink target fingerprint budget exceeded (4096 entries)");
        }
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            collect(root, relative(root, &child.path())?, digest, active, budget)?;
        }
        active.remove(&resolved);
    } else {
        bail!("non-file symlink target requires independent inspection");
    }
    Ok(())
}

pub(super) fn fingerprint(root: &Path, path: &Path, budget: &mut Budget) -> Result<String> {
    let mut digest = Sha256::new();
    field(&mut digest, b"opsx-build symlink input v1");
    collect(root, path, &mut digest, &mut BTreeSet::new(), budget)?;
    Ok(format!("{:x}", digest.finalize()))
}
