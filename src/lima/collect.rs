//! Stage and check an inbound export before publishing anything in the host checkout.
mod sync;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
pub(super) use sync::sync;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{checked, limactl};

type Files = BTreeMap<PathBuf, String>;
const METADATA: [&str; 2] = ["opsx-build", "ospx-build"];

#[derive(Serialize, Deserialize)]
struct Baseline {
    files: Files,
    index: String,
    #[serde(default)]
    index_tree: Option<String>,
    #[serde(default)]
    inputs: Option<BTreeSet<PathBuf>>,
}

pub(super) fn baseline(state: &Path, instance: &str) -> PathBuf {
    state.with_extension(format!("{:x}.export-base", Sha256::digest(instance)))
}

fn result(output: Output, action: &str) -> Result<Vec<u8>> {
    ensure!(
        output.status.success(),
        "{action}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn git(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_WORK_TREE", repo)
        .env("GIT_DIR", repo.join(".git"))
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ]);
    cmd
}

fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    result(git(repo).args(args).output()?, &format!("git {}", args[0]))
}

fn index(repo: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(git_bytes(repo, &["ls-files", "--stage", "-z"])?)
    ))
}

fn fingerprint(path: &Path) -> Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    if meta.is_dir() {
        return Ok(Some("dir".into()));
    }
    if meta.is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(Some(format!(
            "link:{:x}",
            Sha256::digest(target.as_os_str().as_encoded_bytes())
        )));
    }
    ensure!(
        meta.is_file(),
        "special file at {}; use an explicit export destination",
        path.display()
    );
    let mut hash = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hash)?;
    Ok(Some(format!(
        "{:o}:{:x}",
        meta.permissions().mode() & 0o777,
        hash.finalize()
    )))
}

// The campaign data includes the working tree and its private checkpoint directories.
// The full scan is used only to compare the two trusted local copies for publication.
fn scan(root: &Path, all_git: bool) -> Result<Files> {
    fn visit(root: &Path, path: &Path, all_git: bool, out: &mut Files) -> Result<()> {
        if path == Path::new(".git") && !all_git {
            ensure!(
                fs::symlink_metadata(root.join(path))?.is_dir(),
                "expected a standalone .git directory"
            );
            for name in METADATA {
                let child = path.join(name);
                match fs::symlink_metadata(root.join(&child)) {
                    Ok(meta) => {
                        ensure!(
                            meta.is_dir(),
                            "expected a checkpoint directory at {}",
                            child.display()
                        );
                        visit(root, &child, all_git, out)?;
                    }
                    Err(e) if e.kind() == ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
            return Ok(());
        }
        let kind = fingerprint(&root.join(path))?.context("file disappeared during export")?;
        let directory = kind == "dir";
        out.insert(path.to_owned(), kind);
        if directory {
            for item in fs::read_dir(root.join(path))? {
                visit(root, &path.join(item?.file_name()), all_git, out)?;
            }
        }
        Ok(())
    }
    let mut out = Files::new();
    for item in fs::read_dir(root)? {
        visit(root, Path::new(&item?.file_name()), all_git, &mut out)?;
    }
    Ok(out)
}

// Keep this byte format in step with checkout_signature.py. PathBuf keys are
// ordered by their components, so children follow their parent immediately.
fn signature_from_files(repo: &Path, files: &Files, full: bool) -> Result<String> {
    let mut hash = Sha256::new();
    for (path, kind) in files {
        if !full
            && let Ok(relative) = path.strip_prefix(".git")
            && !METADATA.iter().any(|name| relative.starts_with(name))
        {
            continue;
        }
        hash.update(path.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update(kind.as_bytes());
        hash.update([0]);
    }
    if !full {
        for args in [
            &["rev-parse", "HEAD"][..],
            &["rev-parse", "--abbrev-ref", "HEAD"],
            &["ls-files", "--stage", "-z"],
            &[
                "for-each-ref",
                "--sort=refname",
                "--format=%(refname) %(objectname)",
            ],
        ] {
            hash.update(git_bytes(repo, args)?);
            hash.update([0]);
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub(super) fn signature(repo: &Path, full: bool) -> Result<String> {
    signature_from_files(repo, &scan(repo, full)?, full)
}

pub(super) fn check_exported_refs(repo: &Path, guest_refs: &[u8]) -> Result<()> {
    let host = git_bytes(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])?;
    let refs: BTreeSet<_> = host.split(|&b| b == b'\n').collect();
    for guest in guest_refs.split(|&b| b == b'\n').filter(|r| !r.is_empty()) {
        ensure!(
            refs.contains(guest),
            "guest ref {} is not in the host export; use 'opsx-build lima export <new-directory> --cleanup' for a full snapshot",
            String::from_utf8_lossy(guest.split(|&b| b == b' ').next().unwrap_or(guest))
        );
    }
    Ok(())
}

fn save_base(data: &Baseline, path: &Path) -> Result<()> {
    let tmp = path.with_extension("base-tmp");
    fs::write(&tmp, serde_json::to_vec(data)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

pub(super) fn remember(repo: &Path, path: &Path) -> Result<()> {
    save_base(
        &Baseline {
            files: scan(repo, false)?,
            index: index(repo)?,
            index_tree: Some(git_text(repo, &["write-tree"])?),
            inputs: Some(sync::inputs(repo)?),
        },
        path,
    )
}

fn copy_checkout(src: &Path, dest: &Path, options: &[&str]) -> Result<()> {
    fs::create_dir(dest)?;
    let mut cmd = Command::new("rsync");
    cmd.arg("-a")
        .args(options)
        .arg("--")
        .arg(src.join("."))
        .arg(dest);
    result(cmd.output()?, "staging the host checkout")?;
    Ok(())
}

fn keys<'a>(a: &'a Files, b: &'a Files) -> BTreeSet<&'a PathBuf> {
    a.keys().chain(b.keys()).collect()
}

fn differences(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .take(12)
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn merged(base: &Files, host: &Files, guest: &Files) -> Result<Files> {
    let mut next = host.clone();
    let mut conflicts = Vec::new();
    for path in keys(base, guest) {
        let old = base.get(path);
        let new = guest.get(path);
        if old == new || host.get(path) == new {
            continue;
        }
        if !path.starts_with(".git")
            && path.components().any(|part| {
                part.as_os_str()
                    .as_encoded_bytes()
                    .eq_ignore_ascii_case(b".git")
            })
        {
            bail!(
                "nested Git metadata at {}; use a separate export directory",
                path.display()
            );
        }
        if host.get(path) != old {
            conflicts.push(path.clone());
            continue;
        }
        match new {
            Some(value) => {
                next.insert(path.clone(), value.clone());
            }
            None => {
                next.remove(path);
            }
        }
    }
    for path in next.keys() {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && parent != Path::new(".git")
            && next.get(parent).map(String::as_str) != Some("dir")
        {
            conflicts.push(path.clone());
        }
    }
    ensure!(
        conflicts.is_empty(),
        "export conflicts at {}. Host and guest were left in place",
        differences(&conflicts)
    );
    Ok(next)
}

fn directory(kind: Option<&String>) -> bool {
    kind.is_some_and(|v| v == "dir")
}

// A symlink is a leaf, including while replacing a link with a directory.
fn existing(root: &Path, path: &Path) -> Result<Option<String>> {
    let mut parent = root.to_owned();
    for part in path.parent().into_iter().flat_map(Path::components) {
        parent.push(part);
        match fs::symlink_metadata(&parent) {
            Ok(meta) if !meta.is_dir() => return Ok(None),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
            _ => (),
        }
    }
    fingerprint(&root.join(path))
}

fn check_parents(root: &Path, path: &Path) -> Result<()> {
    for parent in path
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
    {
        ensure!(
            fs::symlink_metadata(root.join(parent))?.is_dir(),
            "directory changed at {}",
            parent.display()
        );
    }
    Ok(())
}

// Each file replacement is atomic. An interrupted publication can replay the same
// before/after pair. Only paths changed by this export may be written.
fn apply(from: &Files, to: &Files, src: &Path, dst: &Path) -> Result<()> {
    let paths: Vec<_> = keys(from, to)
        .into_iter()
        .filter(|p| from.get(*p) != to.get(*p))
        .collect();
    let mut pending = Vec::new();
    for path in paths {
        let actual = existing(dst, path)?;
        if actual.as_ref() == to.get(path) {
            continue;
        }
        ensure!(
            actual.as_ref() == from.get(path)
                || (actual.is_none()
                    && from.contains_key(path)
                    && to.contains_key(path)
                    && directory(from.get(path)) != directory(to.get(path))),
            "checkout changed at {}; export stopped",
            path.display()
        );
        pending.push(path);
    }
    // Remove children before their directories. Do not recursively delete a
    // directory: any unexpected local file inside it must survive.
    for &path in pending.iter().rev() {
        if to.contains_key(path) && directory(from.get(path)) == directory(to.get(path)) {
            continue;
        }
        if let Some(current) = existing(dst, path)? {
            check_parents(dst, path)?;
            ensure!(
                Some(&current) == from.get(path),
                "checkout changed at {}",
                path.display()
            );
            if directory(from.get(path)) {
                fs::remove_dir(dst.join(path))?;
            } else {
                fs::remove_file(dst.join(path))?;
            }
        }
    }
    for path in pending {
        let Some(kind) = to.get(path) else { continue };
        check_parents(dst, path)?;
        let dest = dst.join(path);
        if kind == "dir" {
            fs::create_dir(&dest)?;
            continue;
        }
        let tmp = dest.with_file_name(format!(".opsx-copy-{}", uuid::Uuid::new_v4()));
        let write = (|| -> Result<()> {
            let source = src.join(path);
            if kind.starts_with("link:") {
                symlink(fs::read_link(source)?, &tmp)?;
            } else {
                fs::copy(source, &tmp)?;
            }
            let expected = from.get(path).filter(|value| value.as_str() != "dir");
            ensure!(
                existing(dst, path)?.as_ref() == expected,
                "checkout changed at {}",
                path.display()
            );
            fs::rename(&tmp, &dest)?;
            Ok(())
        })();
        if write.is_err() {
            let _ = fs::remove_file(tmp);
        }
        write.with_context(|| format!("installing {}", path.display()))?;
    }
    Ok(())
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(repo, args)?)?.trim().to_owned())
}

fn standalone(repo: &Path) -> Result<()> {
    let dir = repo.join(".git");
    ensure!(
        fs::symlink_metadata(&dir)?.is_dir(),
        "expected a standalone checkout at {}",
        repo.display()
    );
    for other in [
        "commondir",
        "objects/info/alternates",
        "objects/info/http-alternates",
    ] {
        ensure!(
            fs::symlink_metadata(dir.join(other)).is_err(),
            "external Git storage at {other}; use a separate export"
        );
    }
    Ok(())
}

// The incoming .git belongs to a VM. Use its object data and refs as a fetch
// source, without importing its config, hooks or filesystem links into host Git.
fn prepare_donor(guest: &Path, host: &Path) -> Result<()> {
    standalone(guest)?;
    fn check(path: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            meta.is_file() || meta.is_dir(),
            "unsupported Git metadata at {}",
            path.display()
        );
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                check(&entry?.path())?;
            }
        }
        Ok(())
    }
    for entry in fs::read_dir(guest.join(".git"))? {
        let entry = entry?;
        if !METADATA
            .iter()
            .any(|name| entry.file_name() == OsStr::new(name))
        {
            check(&entry.path())?;
        }
    }
    let format = git_text(host, &["rev-parse", "--show-object-format"])?;
    let config = match format.as_str() {
        "sha1" => "[core]\nrepositoryformatversion = 0\n",
        "sha256" => "[core]\nrepositoryformatversion = 1\n[extensions]\nobjectFormat = sha256\n",
        _ => bail!("unsupported Git object format {format}"),
    };
    fs::write(guest.join(".git/config"), config)?;
    Ok(())
}

// Before this feature, prepare didn't record a baseline. The host commit is
// the provable common input. Unknown, overlapping untracked inputs conflict.
fn legacy_base(host: &Path, stage: &Path, revision: &str) -> Result<Baseline> {
    let clean = stage.join("clean");
    fs::create_dir(&clean)?;
    let idx = stage.join("clean-index");
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        result(
            git(host)
                .env("GIT_INDEX_FILE", &idx)
                .env("GIT_WORK_TREE", &clean)
                .current_dir(&clean)
                .args(args)
                .output()?,
            "reading the original committed inputs",
        )
    };
    run(&["read-tree", revision])?;
    run(&["checkout-index", "--all"])?;
    Ok(Baseline {
        files: scan(&clean, false)?,
        inputs: Some(sync::paths(&run(&["ls-files", "-z"])?)?),
        index_tree: Some(git_text(
            host,
            &["rev-parse", &format!("{revision}^{{tree}}")],
        )?),
        index: format!("{:x}", Sha256::digest(run(&["ls-files", "--stage", "-z"])?)),
    })
}

fn branch(repo: &Path) -> Result<Vec<u8>> {
    let out = git(repo).args(["symbolic-ref", "-q", "HEAD"]).output()?;
    if out.status.code() == Some(1) {
        Ok(Vec::new())
    } else {
        result(out, "reading the current branch")
    }
}

fn import_git(guest: &Path, candidate: &Path) -> Result<()> {
    ensure!(
        branch(guest)? == branch(candidate)?,
        "host and VM are on different branches; use a separate export"
    );
    ensure!(
        git_bytes(guest, &["ls-files", "-u", "-z"])?.is_empty(),
        "the guest index has unresolved conflicts"
    );
    let head = git_text(guest, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    result(
        git(candidate)
            .args([
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--no-auto-maintenance",
                "--",
            ])
            .arg(guest)
            .args([
                "HEAD",
                "refs/tags/*:refs/tags/*",
                "refs/opsx-build/*:refs/opsx-build/*",
            ])
            .output()?,
        "importing campaign history and checkpoint refs",
    )?;
    let before = git_text(candidate, &["rev-parse", "--verify", "HEAD"])?;
    ensure!(
        git(candidate)
            .args(["merge-base", "--is-ancestor", &before, &head])
            .status()?
            .success(),
        "host and VM history cannot fast-forward; export to a separate directory to review them"
    );
    if before != head {
        git_bytes(
            candidate,
            &[
                "update-ref",
                "-m",
                "opsx-build lima export",
                "HEAD",
                &head,
                &before,
            ],
        )?;
    }
    if index(candidate)? == index(guest)? {
        return Ok(());
    }
    git_bytes(candidate, &["read-tree", "--reset", &head])?;
    let patch = git_bytes(
        guest,
        &[
            "diff",
            "--cached",
            "--binary",
            "--full-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "HEAD",
            "--",
        ],
    )?;
    if !patch.is_empty() {
        let mut child = git(candidate)
            .args(["apply", "--cached", "--whitespace=nowarn", "-"])
            .stdin(Stdio::piped())
            .spawn()?;
        let sent = child.stdin.take().unwrap().write_all(&patch);
        let status = child.wait()?;
        ensure!(status.success(), "could not reconstruct the guest index");
        sent?;
    }
    ensure!(
        index(candidate)? == index(guest)?,
        "could not reproduce the guest's staged changes"
    );
    Ok(())
}

fn check_host_git_idle(repo: &Path) -> Result<()> {
    let gitdir = repo.join(".git");
    for busy in [
        "index.lock",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-apply",
        "rebase-merge",
    ] {
        ensure!(
            !gitdir.join(busy).exists(),
            "host Git operation in progress at .git/{busy}"
        );
    }
    Ok(())
}

fn plan(instance: &str, guest_dir: &str, repo: &Path, stage: &Path, anchor: &Path) -> Result<()> {
    standalone(repo)?;
    check_host_git_idle(repo)?;
    super::ensure_guest_idle(instance, guest_dir)?;
    let guest = stage.join("guest");
    fs::create_dir(&guest)?;
    checked(
        limactl().current_dir(&guest).args([
            "copy",
            "--backend=rsync",
            &format!("{instance}:{guest_dir}/"),
            "./",
        ]),
        "staging guest results",
    )?;
    super::ensure_guest_idle(instance, guest_dir)?;
    prepare_donor(&guest, repo)?;
    let before = stage.join("before");
    copy_checkout(repo, &before, &[])?;
    let mut old: Baseline = match fs::read(anchor) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(e) if e.kind() == ErrorKind::NotFound => legacy_base(&before, stage, "HEAD")?,
        Err(e) => return Err(e.into()),
    };
    let local = scan(&before, false)?;
    let incoming = Baseline {
        files: scan(&guest, false)?,
        index: index(&guest)?,
        index_tree: None,
        inputs: Some(sync::inputs(&guest)?),
    };
    fs::write(
        stage.join("guest-signature"),
        signature_from_files(&guest, &incoming.files, false)?,
    )?;
    // Preflight regenerates this cache in each checkout. When both have a
    // regular report and the guest has a new result, collect the guest's copy.
    let report = Path::new(".git/opsx-build/contract-preflight.json");
    if incoming.files.get(report) != old.files.get(report)
        && let Some(local_report) = local.get(report)
        && fs::symlink_metadata(before.join(report)).is_ok_and(|m| m.is_file())
        && fs::symlink_metadata(guest.join(report)).is_ok_and(|m| m.is_file())
    {
        old.files.insert(report.to_owned(), local_report.clone());
    }
    let wanted = merged(&old.files, &local, &incoming.files)?;
    let local_index = index(&before)?;
    ensure!(
        local_index == old.index || local_index == incoming.index,
        "host staging area has changed; preserve those staged edits before exporting"
    );
    // Only the host's Git administration is carried forward. The staged guest
    // already contains the result files, so only host-only edits need copying.
    let admin = stage.join("admin");
    fs::create_dir(&admin)?;
    let trusted_git = admin.join(".git");
    copy_checkout(
        &before.join(".git"),
        &trusted_git,
        &["--exclude=/opsx-build", "--exclude=/ospx-build"],
    )?;
    import_git(&guest, &admin)?;
    // Keep the exported index tree in the host object database for a later sync.
    let incoming = Baseline {
        index_tree: Some(git_text(&admin, &["write-tree"])?),
        ..incoming
    };
    apply(&incoming.files, &wanted, &before, &guest)?;
    let donor = stage.join("donor-git");
    fs::rename(guest.join(".git"), &donor)?;
    for name in METADATA {
        if donor.join(name).try_exists()? {
            fs::rename(donor.join(name), trusted_git.join(name))?;
        }
    }
    fs::rename(trusted_git, guest.join(".git"))?;
    save_base(&incoming, &stage.join("next-base"))?;
    fs::write(stage.join("ready"), b"")?;
    Ok(())
}

pub(super) fn export(
    instance: &str,
    guest_dir: &str,
    repo: &Path,
    state: &Path,
) -> Result<Option<(String, String)>> {
    let stage = state.with_extension("export-staging");
    ensure!(
        !stage.starts_with(repo),
        "export staging must be outside the campaign checkout"
    );
    let anchor = baseline(state, instance);
    if stage.try_exists()? {
        ensure!(
            fs::symlink_metadata(&stage)?.is_dir()
                && fs::read(stage.join("owner"))? == instance.as_bytes(),
            "unexpected export staging directory at {}",
            stage.display()
        );
    }
    if !stage.join("ready").is_file() {
        if stage.try_exists()? {
            fs::remove_dir_all(&stage)?;
        }
        fs::DirBuilder::new().mode(0o700).create(&stage)?;
        fs::write(stage.join("owner"), instance)?;
        if let Err(error) = plan(instance, guest_dir, repo, &stage, &anchor) {
            fs::remove_dir_all(&stage)?;
            return Err(error);
        }
    }

    let before = stage.join("before");
    let after = stage.join("guest");
    let old = scan(&before, true)?;
    let new = scan(&after, true)?;
    let lock_path = repo.join(".git/index.lock");
    let signature = format!("opsx-build lima export {instance}\n");
    match fs::File::create_new(&lock_path) {
        Ok(mut lock) => lock.write_all(signature.as_bytes())?,
        Err(e)
            if e.kind() == ErrorKind::AlreadyExists
                && fs::read(&lock_path)? == signature.as_bytes() =>
        {
            ()
        }
        Err(e) => return Err(e).context("host Git index is busy"),
    }
    struct IndexLock(PathBuf);
    impl Drop for IndexLock {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let _index_lock = IndexLock(lock_path);
    if !stage.join("publishing").exists() {
        // If the host changed while copying the guest, report it before the first write.
        if let Err(error) = merged(&old, &scan(repo, true)?, &new) {
            fs::remove_dir_all(&stage)?;
            return Err(error);
        }
        fs::write(stage.join("publishing"), b"")?;
    }
    if let Err(error) = apply(&old, &new, &after, repo) {
        if apply(&new, &old, &before, repo).is_ok() {
            fs::remove_dir_all(&stage)?;
            return Err(error).context("export failed; restored the original checkout");
        }
        return Err(error.context(format!("export incomplete; original and result retained in {}. Retry 'opsx-build lima export' after resolving the error", stage.display())));
    }
    let next: Baseline = serde_json::from_slice(&fs::read(stage.join("next-base"))?)?;
    save_base(&next, &anchor)?;
    // An interrupted export staged by an older launcher can still finish.
    let signature = match fs::read_to_string(stage.join("guest-signature")) {
        Ok(value) => Some((value, signature_from_files(&after, &new, false)?)),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    drop(_index_lock);
    fs::remove_dir_all(stage)?;
    println!("Exported campaign into {}", repo.display());
    Ok(signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_publication_replays_files_and_preserves_symlink_targets() {
        let tmp = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let before = tmp.join("before");
        let after = tmp.join("after");
        let live = tmp.join("live");
        let outside = tmp.join("outside");
        for p in [&before, &after, &live, &outside] {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(outside.join("item"), "private").unwrap();
        for p in [&before, &live] {
            symlink(&outside, p.join("folder")).unwrap();
            fs::write(p.join("other"), "old").unwrap();
        }
        fs::create_dir(after.join("folder")).unwrap();
        fs::write(after.join("folder/item"), "new").unwrap();
        fs::write(after.join("other"), "published first").unwrap();
        let old = scan(&before, true).unwrap();
        let new = scan(&after, true).unwrap();
        fs::write(live.join("other"), "published first").unwrap();
        apply(&old, &new, &after, &live).unwrap();
        assert_eq!(scan(&live, true).unwrap(), new);
        assert_eq!(fs::read_to_string(outside.join("item")).unwrap(), "private");
        apply(&new, &old, &before, &live).unwrap();
        assert_eq!(scan(&live, true).unwrap(), old);
        fs::remove_dir_all(tmp).unwrap();
    }
}
