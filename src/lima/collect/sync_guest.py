"""Apply one verified host delta in the paused guest, using its existing Git and Python."""
import contextlib
import ctypes
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile


def require(condition, message):
    if not condition:
        raise ValueError(message)


def git(*args, index=None, check=True):
    env = dict(os.environ, GIT_OPTIONAL_LOCKS="0", GIT_NO_REPLACE_OBJECTS="1")
    if index is not None:
        env["GIT_INDEX_FILE"] = str(index)
    return subprocess.run(
        ["git", "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false", *args],
        env=env, stdout=subprocess.PIPE, check=check,
    )


def oid(*args, index=None):
    return git(*args, index=index).stdout.decode().strip()


def fingerprint(path):
    # A symlink is always a leaf, even when it points to a directory.
    if any(parent.is_symlink() or not parent.is_dir() for parent in path.parents):
        return None
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError:
        return None
    if stat.S_ISDIR(mode):
        return "dir"
    if stat.S_ISLNK(mode):
        return "link:" + hashlib.sha256(os.fsencode(os.readlink(path))).hexdigest()
    require(stat.S_ISREG(mode), f"special file at {path}")
    with path.open("rb") as stream:
        digest = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"{mode & 0o777:o}:{digest.hexdigest()}"


def unexpected_files(directory, original, changes, version):
    for child in directory.rglob("*"):
        relative = original / child.relative_to(directory)
        kind = fingerprint(child)
        if kind != "dir" and changes.get(relative, (None, None))[version] != kind:
            yield relative


def work_plan(payload, changes):
    pending = set()
    conflicts = []
    for path, (old, new) in changes.items():
        require(path.parts and not path.is_absolute() and
                all(p not in (".", "..") and p.lower() != ".git" for p in path.parts),
                f"unsafe sync path {path}")
        if new is not None:
            require(fingerprint(payload / path) == new, f"host file changed while syncing {path}")
            for parent in path.parents:
                if fingerprint(parent) != "dir" and changes.get(parent, (None, None))[1] != "dir":
                    conflicts.append(parent)
        actual = fingerprint(path)
        if actual == new:
            continue
        if actual != old:
            conflicts.append(path)
            continue
        if actual == "dir":
            # Replacing a directory must never consume guest-only files.
            conflicts.extend(unexpected_files(path, path, changes, 0))
        pending.add(path)
    require(not conflicts, "sync conflicts at " + ", ".join(map(str, conflicts[:12])))
    return sorted(path for path in pending if pending.isdisjoint(path.parents))


def prepare_history(manifest, packet, index):
    original = oid("rev-parse", "HEAD")
    require(oid("rev-parse", "--abbrev-ref", "HEAD") == manifest["branch"],
            "host and guest are on different branches")
    require(not git("ls-files", "-uz", index=index).stdout, "resolve guest index conflicts first")
    pack = packet / "history.pack"
    if pack.exists():
        with pack.open("rb") as stream:
            subprocess.run(["git", "index-pack", "--stdin"], stdin=stream, stdout=subprocess.DEVNULL, check=True)
    host = manifest["head"]
    head = original
    if git("merge-base", "--is-ancestor", host, original, check=False).returncode:
        if not git("merge-base", "--is-ancestor", original, host, check=False).returncode:
            head = host
        else:
            merged = git("merge-tree", "--write-tree", original, host, check=False)
            require(merged.returncode == 0, "host and guest commits conflict: " +
                    merged.stdout.decode(errors="replace").strip())
            head = oid("-c", "user.name=opsx-build", "-c", "user.email=opsx-build@localhost",
                       "commit-tree", merged.stdout.decode().strip(), "-p", original, "-p", host,
                       "-m", "Merge maintainer corrections into campaign")
    if manifest["base_index"] != manifest["index"]:
        current = hashlib.sha256(git("ls-files", "--stage", "-z", index=index).stdout).hexdigest()
        if current != manifest["index"]:
            tree = manifest["base_tree"]
            if current == manifest["base_index"]:
                tree = oid("write-tree", index=index)
            if tree is None:
                # Older launchers only recorded the index checksum.
                pristine = index.with_name("common-index")
                git("read-tree", manifest["common"], index=pristine)
                digest = hashlib.sha256(git("ls-files", "--stage", "-z", index=pristine).stdout)
                require(digest.hexdigest() == manifest["base_index"],
                        "cannot identify the previous staging area; export the paused campaign first")
                tree = oid("write-tree", index=pristine)
            git("read-tree", "-i", "-m", tree, manifest["tree"], index=index)
    return original, head


def move(source, dest, exchange=False):
    # The Linux guest can swap a file and directory in one operation. This keeps
    # every path at either its old or its new value even if the VM loses power.
    libc = ctypes.CDLL(None, use_errno=True)
    src, dst = os.fsencode(source), os.fsencode(dest)
    if sys.platform == "darwin":  # Native integration tests on a macOS host.
        result = libc.renamex_np(src, dst, 2 if exchange else 4)
    else:
        result = libc.renameat2(-100, src, -100, dst, 2 if exchange else 1)
    if result != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(dest))


class RecoveryError(RuntimeError):
    pass


def apply(packet, scratch):
    manifest = json.loads((packet / "sync.json").read_bytes())
    changes = {Path(name): pair for name, pair in manifest["paths"].items()}
    payload = packet / "files"
    roots = work_plan(payload, changes)
    gitdir = Path(".git")
    for busy in ("MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD", "rebase-apply", "rebase-merge"):
        require(not (gitdir / busy).exists(), f"guest Git operation in progress at .git/{busy}")
    index = scratch / "index"
    shutil.copy2(gitdir / "index", index)
    old_head, new_head = prepare_history(manifest, packet, index)
    replacements = []
    for n, path in enumerate(roots):
        old, new = changes[path]
        staged = scratch / f"new-{n}"
        if new == "dir":
            shutil.copytree(payload / path, staged, symlinks=True)
        elif new is not None:
            shutil.copy2(payload / path, staged, follow_symlinks=False)
        replacements.append((path, old, new, staged))

    installed = []
    moved_head = False
    try:
        for path, old, new, staged in replacements:
            require(fingerprint(path) == old, f"guest changed during sync at {path}")
            if new is None:
                move(path, staged)
            else:
                move(staged, path, exchange=old is not None)
            installed.append((path, old, new, staged))
            if old is not None:
                require(fingerprint(staged) == old, f"guest changed during sync at {path}")
            if old == "dir":
                require(not any(unexpected_files(staged, path, changes, 0)),
                        f"guest directory changed during sync at {path}")
        if old_head != new_head:
            git("update-ref", "-m", "opsx-build lima sync", "HEAD", new_head, old_head)
            moved_head = True
        if manifest["base_index"] != manifest["index"]:
            os.replace(index, gitdir / "index")
    except BaseException:
        try:
            if moved_head:
                git("update-ref", "HEAD", old_head, new_head)
            for path, old, new, staged in reversed(installed):
                require(fingerprint(path) == new, f"guest changed during rollback at {path}")
                if new == "dir":
                    require(not any(unexpected_files(path, path, changes, 1)),
                            f"guest directory changed during rollback at {path}")
                if old is None:
                    move(path, staged)
                else:
                    move(staged, path, exchange=new is not None)
        except Exception as error:
            raise RecoveryError(f"sync interrupted; original files retained in {scratch}: {error}") from error
        raise


@contextlib.contextmanager
def index_guard(gitdir, scratch):
    # Publish an identifiable index.lock atomically. A retry can reclaim our
    # lock after a VM restart, while ordinary Git processes keep their locks.
    path = gitdir / "index.lock"
    marker = scratch / "owner"
    signature = b"opsx-build lima sync\n"
    marker.write_bytes(signature)
    try:
        os.link(marker, path)
    except FileExistsError:
        pass
    with path.open("rb") as lock:
        require(lock.read(len(signature) + 1) == signature, "guest Git index is busy")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            path.unlink()


def main():
    packet = Path(__file__).resolve().parent
    gitdir = Path(".git")
    require(not gitdir.is_symlink() and gitdir.is_dir(), "guest needs a standalone checkout")
    with contextlib.ExitStack() as stack:
        for owner in gitdir.glob("opsx-build/policy/*/owner.lock"):
            lock = stack.enter_context(owner.open("rb"))
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        scratch = Path(tempfile.mkdtemp(prefix="opsx-sync-", dir=gitdir)).resolve()
        keep_originals = False
        try:
            with index_guard(gitdir, scratch):
                apply(packet, scratch)
        except RecoveryError:
            keep_originals = True
            raise
        finally:
            if not keep_originals:
                shutil.rmtree(scratch)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError, RecoveryError) as error:
        sys.exit(f"lima sync: {error}")
