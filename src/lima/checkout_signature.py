"""Fingerprint the guest checkout without transferring its files back a second time."""
import hashlib
import os
import stat
import subprocess
import sys

full = sys.argv[1] == "full"
digest = hashlib.sha256()


def visit(path):
    mode = os.lstat(path).st_mode
    if path == b".git" and not full:
        for name in (b"opsx-build", b"ospx-build"):
            child = os.path.join(path, name)
            if os.path.lexists(child):
                visit(child)
        return
    if stat.S_ISDIR(mode):
        kind = "dir"
    elif stat.S_ISLNK(mode):
        kind = "link:" + hashlib.sha256(os.readlink(path)).hexdigest()
    elif stat.S_ISREG(mode):
        contents = hashlib.sha256()
        with open(path, "rb") as stream:
            for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                contents.update(chunk)
        kind = f"{mode & 0o777:o}:{contents.hexdigest()}"
    else:
        sys.exit(f"cannot verify special file {os.fsdecode(path)}")
    digest.update(path + b"\0" + kind.encode() + b"\0")
    if stat.S_ISDIR(mode):
        for name in sorted(os.listdir(path)):
            visit(os.path.join(path, name))


for name in sorted(os.listdir(b".")):
    visit(name)
if not full:
    env = dict(os.environ, GIT_DIR=".git", GIT_WORK_TREE=".", GIT_OPTIONAL_LOCKS="0", GIT_NO_REPLACE_OBJECTS="1")
    env.pop("GIT_INDEX_FILE", None)
    for args in (["rev-parse", "HEAD"], ["rev-parse", "--abbrev-ref", "HEAD"],
                 ["ls-files", "--stage", "-z"], ["for-each-ref", "--sort=refname", "--format=%(refname) %(objectname)"]):
        digest.update(subprocess.check_output(["git", "-c", "core.fsmonitor=false", *args], env=env) + b"\0")
print(digest.hexdigest())
