#!/usr/bin/env python3
"""Check campaign prerequisites under nono wrap without advancing a campaign."""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile


def run(command, cwd, timeout):
    print('+ ' + ' '.join(map(str, command)), flush=True)
    process = subprocess.Popen(command, cwd=cwd, text=True, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, start_new_session=True)
    try:
        output, _ = process.communicate(timeout=timeout)
    except (subprocess.TimeoutExpired, KeyboardInterrupt):
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
        raise
    print(output, end='', flush=True)
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, command)
    return output.strip()


def inside(args):
    campaign = args.campaign.resolve()
    failures = []
    def check(name, action):
        try:
            action()
            print('PASS ' + name, flush=True)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            failures.append(name)
            print(f'FAIL {name}: {error}', flush=True)
    def executable_check(name, version_args):
        executable = shutil.which(name)
        if not executable:
            raise ValueError(name + ' is missing or not executable on PATH')
        print(f'Selected {name}: {executable} -> {Path(executable).resolve()}', flush=True)
        run([executable, *version_args], campaign, args.timeout)
    for name, version_args in [
        ('go', ['version']),
        ('protoc', ['--version']),
        ('protoc-gen-go', ['--version']),
        ('protoc-gen-go-grpc', ['--version']),
    ]:
        check('PATH executable: ' + name,
              lambda name=name, version_args=version_args: executable_check(name, version_args))
    check('Git configuration and metadata', lambda: run(
        ['git', 'rev-parse', '--git-path', 'opsx-build'], campaign, args.timeout))
    with tempfile.TemporaryDirectory(prefix='opsx-preflight-') as temporary:
        scratch = Path(temporary)
        def git_commit():
            run(['git', 'init', '-q', str(scratch)], campaign, args.timeout)
            (scratch / 'proof.txt').write_text('preflight\n')
            run(['git', 'add', 'proof.txt'], scratch, args.timeout)
            run(['git', '-c', 'core.hooksPath=/dev/null', '-c', 'commit.gpgsign=false',
                 'commit', '-qm', 'Preflight proof'], scratch, args.timeout)
        check('Git commit in disposable repository (hooks/signing excluded)', git_commit)
        check('OpenSpec startup', lambda: run(['openspec', '--version'], campaign, args.timeout))
        if (campaign / 'openspec/config.yaml').exists():
            check('OpenSpec campaign discovery', lambda: run(
                ['openspec', 'list', '--json'], campaign, args.timeout))
        else:
            print('INFO OpenSpec is not initialized; bootstrap initializes it.', flush=True)
        check('Resolved campaign configuration', lambda: run(
            ['opsx-build', 'autoconfigure', '--dry-run'], campaign, args.timeout))
        modules = list(campaign.rglob('go.mod'))
        modules = [p for p in modules if not any(x in p.parts for x in
                   ['.git', '.tools', 'vendor', 'node_modules'])]
        if modules:
            def go_check():
                executable = shutil.which('go')
                if not executable:
                    raise ValueError('go is missing from PATH')
                for module in modules:
                    settings = json.loads(run([executable, 'env', '-json', 'GOVERSION',
                        'GOROOT', 'GOCACHE', 'GOMODCACHE'], module.parent, args.timeout))
                    print(f'Module: {module}; toolchain: {settings["GOVERSION"]}', flush=True)
                    for key in ['GOCACHE', 'GOMODCACHE']:
                        cache = Path(settings[key]); cache.mkdir(parents=True, exist_ok=True)
                        with tempfile.TemporaryFile(dir=cache) as probe:
                            probe.write(b'preflight\n')
                    # Build a standalone fixture with the module-selected toolchain.
                    go = Path(settings['GOROOT']) / 'bin/go'
                    (scratch / 'main.go').write_text('package main\nfunc main() {}\n')
                    run([str(go), 'build', '-o', str(scratch / 'go-proof'),
                         str(scratch / 'main.go')], module.parent, args.timeout)
            check('Module-selected Go toolchains, cache writes and compiler', go_check)
        else:
            print('INFO No Go modules found.', flush=True)
        for connection in dict.fromkeys(args.connection):
            check('Agent tool round trip: ' + connection, lambda connection=connection: run(
                ['opsx-build', '--no-harness-sandbox', '--test-connection', connection],
                campaign, args.timeout))
        if not args.connection:
            print('SKIP Agent round trips: provide --connection for each campaign connection.', flush=True)
    print('FAILED: ' + ', '.join(failures) if failures else 'Preflight passed.', flush=True)
    return bool(failures)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--campaign', type=Path, default=Path.cwd())
    parser.add_argument('--profile', type=Path, default=Path(__file__).with_name('opsx-build.json'))
    parser.add_argument('--connection', action='append', default=[],
                        help='Named campaign connection to test; repeat for worker/frontier (uses model allowance)')
    parser.add_argument('--timeout', type=int, default=120, help='Seconds per command')
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    if args.inside:
        return inside(args)
    campaign = args.campaign.resolve(strict=True)
    profile = args.profile.resolve(strict=True)
    run(['nono', 'profile', 'validate', str(profile)], campaign, args.timeout)
    # /tmp is granted by the profile; the copy avoids needing repository read grants.
    with tempfile.TemporaryDirectory(prefix='opsx-preflight-launch-') as temporary:
        script = Path(temporary) / 'preflight.py'
        shutil.copyfile(__file__, script)
        command = ['nono', 'wrap', '--profile', str(profile), '--allow-cwd', '--',
                   sys.executable, str(script), '--inside', '--campaign', str(campaign),
                   '--timeout', str(args.timeout)]
        for connection in args.connection:
            command.extend(['--connection', connection])
        return subprocess.call(command, cwd=campaign)


if __name__ == '__main__':
    sys.exit(main())
