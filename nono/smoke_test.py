#!/usr/bin/env python3
"""Exercise package provisioning and agent harnesses under nono; requires Python 3.9+."""

import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import uuid


def run(*args, env=None):
    print("+ " + shlex.join(map(str, args)), flush=True)
    result = subprocess.run(list(map(str, args)), env=env, text=True,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    print(result.stdout, end="", flush=True)
    result.check_returncode()
    return result.stdout.strip()


def write(path, text):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def expect(actual, expected):
    if actual != expected:
        raise RuntimeError(f"Expected {expected!r}, got {actual!r}")


def writable_cache(path):
    """A warm cache must not disguise missing write access."""
    path = Path(path)
    print(f"+ verify cache read/write: {path}", flush=True)
    path.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(prefix="opsx-nono-probe-", dir=path) as probe:
        probe.write(b"cache read/write works\n")
        probe.flush()
        probe.seek(0)
        expect(probe.read(), b"cache read/write works\n")


def go_check():
    settings = json.loads(run("go", "env", "-json", "GOPATH", "GOMODCACHE", "GOCACHE", "GOBIN"))
    gopath = Path(settings["GOPATH"].split(os.pathsep)[0])
    for path in [gopath / "pkg/sumdb", settings["GOMODCACHE"], settings["GOCACHE"]]:
        writable_cache(path)
    name = "opsx-nono-probe-" + uuid.uuid4().hex
    write("go.mod", f"module example.invalid/{name}\n\ngo 1.21\n\nrequire github.com/google/uuid v1.6.0\n")
    write("main.go", '''package main
import ("fmt"; "github.com/google/uuid")
func main() { fmt.Println("go-ok", uuid.MustParse("00000000-0000-0000-0000-000000000001")) }
''')
    fresh = dict(os.environ, GOMODCACHE=str(Path.cwd() / "fresh-go-mod"))
    print(f"Fresh Go module cache: {fresh['GOMODCACHE']}", flush=True)
    run("go", "mod", "download", "all", env=fresh)  # Network + shared GOPATH sumdb.
    run("go", "mod", "download", "all")  # Normal shared module cache too.
    binary = Path(settings["GOBIN"] or gopath / "bin") / name
    if binary.exists():
        raise RuntimeError(f"Refusing to replace {binary}")
    try:
        run("go", "install", ".")
        expect(run(binary), "go-ok 00000000-0000-0000-0000-000000000001")
    finally:
        binary.unlink(missing_ok=True)


def rust_check():
    write("Cargo.toml", '''[package]
name = "opsx-nono-rust-probe"
version = "0.0.0"
edition = "2021"
[dependencies]
itoa = "=1.0.15"
''')
    write("src/main.rs", 'fn main() { println!("rust-ok {}", itoa::Buffer::new().format(42)); }\n')
    fresh = dict(os.environ, CARGO_HOME=str(Path.cwd() / "fresh-cargo-home"))
    print(f"Fresh Cargo home: {fresh['CARGO_HOME']}", flush=True)
    run("cargo", "generate-lockfile", env=fresh)
    run("cargo", "fetch", "--locked", env=fresh)  # Cannot succeed from a warm cache.
    install = Path.cwd() / "installed"
    run("cargo", "install", "--path", ".", "--locked", "--root", install)
    expect(run(install / "bin/opsx-nono-rust-probe"), "rust-ok 42")


def python_check():
    venv = Path.cwd() / "venv"
    run(sys.executable, "-m", "venv", venv)
    python = venv / "bin/python"
    # pip can silently disable an unwritable cache and still report success.
    writable_cache(run(python, "-m", "pip", "cache", "dir"))
    run(python, "-m", "pip", "install", "--disable-pip-version-check", "--no-cache-dir",
        "--no-deps", "packaging==24.2")
    expect(run(python, "-c", 'import packaging; from packaging.version import Version; '
               'print("python-ok", packaging.__version__, Version("1.2.3"))'),
           "python-ok 24.2 1.2.3")


HARNESS_PROMPT = (
    "This is a sandbox smoke test. Run exactly this shell command once:\n"
    "```sh\nprintf '%s\\n' 'NONO_HARNESS_TOOL_OK' > harness-proof.txt\n```\n"
    "Then reply with exactly NONO_HARNESS_OK. Do not inspect files or do other work."
)


def json_events(text):
    for line in text.splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(event, dict):
            yield event


def harness_result(reply):
    expect(reply.strip(), "NONO_HARNESS_OK")
    expect(Path("harness-proof.txt").read_text(), "NONO_HARNESS_TOOL_OK\n")


def claude_check():
    run("claude", "--version")
    output = run("claude", "--print", "--output-format", "json", "--no-session-persistence",
                 "--tools", "Bash", "--allowedTools", "Bash(printf *)", "--max-turns", "3",
                 "--settings", '{"sandbox":{"enabled":false}}', HARNESS_PROMPT)
    results = [event for event in json_events(output) if event.get("type") == "result"]
    expect(len(results), 1)
    expect(results[0].get("is_error"), False)
    harness_result(results[0].get("result", ""))


def codex_check():
    run("codex", "--version")
    reply = Path("codex-reply.txt")
    run("codex", "--no-daemon", "--ask-for-approval", "never", "--sandbox", "danger-full-access",
        "exec", "--ephemeral", "--skip-git-repo-check", "--json",
        "--output-last-message", reply, HARNESS_PROMPT)
    harness_result(reply.read_text())


def opencode_check():
    run("opencode", "--version")
    output = run("opencode", "run", "--format", "json", "--auto",
                 "--title", "nono harness smoke test", HARNESS_PROMPT)
    parts = []
    for event in json_events(output):
        if event.get("type") == "error":
            raise RuntimeError(f"OpenCode reported an error: {event.get('error')}")
        if event.get("type") == "text":
            parts.append(event.get("part", {}).get("text", ""))
    harness_result("\n".join(parts))


CHECKS = {"go": go_check, "rust": rust_check, "python": python_check,
          "claude": claude_check, "codex": codex_check, "opencode": opencode_check}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, default=Path(__file__).with_name("opsx-build.json"))
    parser.add_argument("--only", choices=CHECKS, action="append", help="Run only selected checks (repeatable)")
    parser.add_argument("--timeout", type=int, default=300, help="Seconds per check (default: 300)")
    parser.add_argument("--inside", choices=CHECKS, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inside:
        signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
        try:
            CHECKS[args.inside]()
        except (OSError, subprocess.CalledProcessError, RuntimeError) as error:
            print(f"FAIL {args.inside}: {error}", flush=True)
            return 1
        print(f"NONO_SMOKE_OK:{args.inside}", flush=True)
        return 0
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if not shutil.which("nono"):
        parser.error("nono must be installed and on PATH")
    profile = args.profile.resolve(strict=True)
    run("nono", "profile", "validate", profile)
    root = Path(tempfile.mkdtemp(prefix="opsx-nono-smoke-")).resolve()
    print(f"Fixtures and logs: {root}", flush=True)
    failures = []
    for name in dict.fromkeys(args.only or CHECKS):
        workspace = root / name
        workspace.mkdir()
        script = workspace / "smoke_test.py"
        shutil.copyfile(__file__, script)
        command = ["nono", "run", "--profile", str(profile), "--allow-cwd", "--no-rollback",
                   "--", sys.executable, str(script), "--inside", name]
        log = root / f"{name}.log"
        print(f"RUN  {name}", flush=True)
        with log.open("w") as output:
            process = subprocess.Popen(command, cwd=workspace, stdout=output,
                                       stderr=subprocess.STDOUT, start_new_session=True,
                                       env=dict(os.environ, PWD=str(workspace)))
            try:
                process.wait(timeout=args.timeout)
            except (subprocess.TimeoutExpired, KeyboardInterrupt) as error:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                output.write("\nSmoke check interrupted or timed out.\n")
                if isinstance(error, KeyboardInterrupt):
                    return 130
        text = log.read_text(errors="replace")
        if process.returncode == 0 and f"NONO_SMOKE_OK:{name}" in text:
            print(f"PASS {name}", flush=True)
        else:
            failures.append(name)
            print(f"FAIL {name} — {log}\n" + "\n".join(text.splitlines()[-25:]), flush=True)
    print(f"{'FAILED: ' + ', '.join(failures) if failures else 'All checks passed'}. Logs: {root}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
