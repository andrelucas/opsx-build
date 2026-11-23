//! Package the maintainer's delta. The guest validates it against its live checkout.
use super::*;
use crate::lima::{ensure_guest_idle, output, shell};
use serde_json::json;

pub(super) fn paths(bytes: &[u8]) -> Result<BTreeSet<PathBuf>> {
    std::str::from_utf8(bytes)?
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(|name| {
            let path = Path::new(name.trim_end_matches('/'));
            ensure!(campaign_path(path), "unsafe campaign path {name:?}");
            Ok(path.to_owned())
        })
        .collect()
}

fn campaign_path(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|part| {
        matches!(part, std::path::Component::Normal(s) if !s.as_encoded_bytes().eq_ignore_ascii_case(b".git"))
    })
}

pub(super) fn inputs(repo: &Path) -> Result<BTreeSet<PathBuf>> {
    paths(&git_bytes(
        repo,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?)
}

fn common_commit(repo: &Path, instance: &str, dir: &str, head: &str) -> Result<String> {
    // Normally the guest already contains the exact host commit.
    let guest = output(shell(instance, dir).args(["sh", "-c",
        r#"if git merge-base --is-ancestor "$1" HEAD 2>/dev/null; then echo "$1"; else git rev-list --topo-order HEAD; fi"#,
        "opsx-sync", head]), "finding shared campaign history")?;
    if guest == head {
        return Ok(guest);
    }
    let host = git_text(repo, &["rev-list", "HEAD"])?;
    let commits: BTreeSet<_> = host.lines().collect();
    Ok(guest
        .lines()
        .find(|id| commits.contains(id))
        .context("host and guest do not share a campaign commit")?
        .to_owned())
}

// A pack can carry the staged tree directly, even when it has no commit.
fn pack_history(
    repo: &Path,
    payload: &Path,
    head: &str,
    common: &str,
    tree: &str,
    old_tree: Option<&str>,
) -> Result<()> {
    let mut revisions = format!("{head}\n{tree}\n^{common}\n");
    if let Some(old) = old_tree
        && git(repo)
            .args(["cat-file", "-e", old])
            .output()?
            .status
            .success()
    {
        revisions.push_str(old);
        revisions.push('\n');
    }
    let mut command = git(repo);
    command
        .args(["pack-objects", "--revs", "--stdout"])
        .stdout(fs::File::create(payload.join("history.pack"))?);
    ensure!(
        crate::lima::with_input(&mut command, &revisions)?.success(),
        "could not pack the host commits and staged files"
    );
    Ok(())
}

pub(in crate::lima) fn sync(instance: &str, dir: &str, repo: &Path, state: &Path) -> Result<()> {
    standalone(repo)?;
    check_host_git_idle(repo)?;
    ensure!(
        !state.with_extension("export-staging").exists(),
        "finish the pending Lima export before syncing"
    );
    ensure!(
        git_bytes(repo, &["ls-files", "-u", "-z"])?.is_empty(),
        "resolve the host index conflicts before syncing"
    );
    ensure_guest_idle(instance, dir)?;
    let scratch = state
        .parent()
        .unwrap()
        .join(format!("sync-{}", uuid::Uuid::new_v4()));
    ensure!(
        !scratch.starts_with(repo),
        "sync staging must be outside the checkout"
    );
    fs::DirBuilder::new().mode(0o700).create(&scratch)?;
    let res = send(instance, dir, repo, state, &scratch);
    let _ = fs::remove_dir_all(scratch);
    res
}

fn send(instance: &str, dir: &str, repo: &Path, state: &Path, scratch: &Path) -> Result<()> {
    let head = git_text(repo, &["rev-parse", "HEAD"])?;
    let common = common_commit(repo, instance, dir, &head)?;
    let anchor = baseline(state, instance);
    let mut base: Baseline = match fs::read(&anchor) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == ErrorKind::NotFound => legacy_base(repo, scratch, &common)?,
        Err(e) => return Err(e.into()),
    };
    let selected = inputs(repo)?;
    // Earlier exports saved hashes for ignored and untracked files together.
    let previous = match base.inputs.take() {
        Some(names) => names,
        None => {
            let mut names = paths(&git_bytes(
                repo,
                &["ls-tree", "-rz", "--name-only", &common],
            )?)?;
            for (path, kind) in &base.files {
                if kind != "dir"
                    && campaign_path(path)
                    && !names.contains(path)
                    && !selected.contains(path)
                    && existing(repo, path)?.is_none()
                    && git(repo)
                        .args(["check-ignore", "-q", "--"])
                        .arg(Path::new(".").join(path))
                        .output()?
                        .status
                        .code()
                        == Some(1)
                {
                    names.insert(path.clone());
                }
            }
            names
        }
    };
    let mut candidates = BTreeSet::new();
    for path in selected.union(&previous) {
        ensure!(
            campaign_path(path),
            "invalid sync baseline path {}",
            path.display()
        );
        candidates.extend(
            path.ancestors()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_owned),
        );
    }
    let payload = scratch.join("payload");
    let files = payload.join("files");
    fs::create_dir_all(&files)?;
    let mut changes = BTreeMap::new();
    let mut copies = Files::new();
    for path in candidates {
        let old = base.files.get(&path).cloned();
        let new = existing(repo, &path)?;
        if old == new {
            continue;
        }
        // Directories shared with guest-only build products may stay behind.
        if directory(old.as_ref()) && new.is_none() {
            continue;
        }
        if new.is_some() && !directory(new.as_ref()) && !selected.contains(&path) {
            continue;
        }
        if let Some(value) = &new {
            for parent in path
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                copies.insert(parent.to_owned(), "dir".into());
            }
            copies.insert(path.clone(), value.clone());
            base.files.insert(path.clone(), value.clone());
        } else {
            base.files.remove(&path);
        }
        changes.insert(path, (old, new));
    }
    apply(&Files::new(), &copies, repo, &files)?;
    let new_index = index(repo)?;
    let new_tree = git_text(repo, &["write-tree"])?;
    let history = head != common || base.index != new_index;
    if history {
        pack_history(
            repo,
            &payload,
            &head,
            &common,
            &new_tree,
            base.index_tree.as_deref(),
        )?;
    }
    let manifest = json!({
        "paths": changes, "branch": git_text(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?,
        "head": head, "common": common, "base_index": base.index, "base_tree": base.index_tree,
        "index": new_index, "tree": new_tree,
    });
    fs::write(payload.join("sync.json"), serde_json::to_vec(&manifest)?)?;
    fs::write(payload.join("apply.py"), include_bytes!("sync_guest.py"))?;
    let remote = output(
        shell(instance, dir).args(["mktemp", "-d", "/tmp/opsx-sync.XXXXXXXX"]),
        "creating guest sync staging",
    )?;
    ensure!(
        remote
            .strip_prefix("/tmp/opsx-sync.")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric())),
        "invalid guest staging path"
    );
    let sent = (|| -> Result<()> {
        checked(
            limactl().current_dir(&payload).args([
                "copy",
                "--backend=rsync",
                "./",
                &format!("{instance}:{remote}/"),
            ]),
            "sending maintainer edits",
        )?;
        ensure!(
            git_text(repo, &["rev-parse", "HEAD"])? == head && index(repo)? == new_index,
            "host Git state changed while sending edits; retry sync"
        );
        ensure_guest_idle(instance, dir)?;
        checked(
            shell(instance, dir).args(["python3", &format!("{remote}/apply.py")]),
            "applying maintainer edits in the guest",
        )
    })();
    let _ = shell(instance, "/tmp")
        .args(["rm", "-rf", "--", &remote])
        .status();
    sent?;
    base.index = new_index;
    base.index_tree = Some(new_tree);
    base.inputs = Some(selected);
    save_base(&base, &anchor)?;
    println!("Synced maintainer edits to {instance}");
    Ok(())
}
