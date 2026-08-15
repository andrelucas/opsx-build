# ospx-build

`ospx-build` is a synchronous CLI orchestrator for unattended OpenSpec builds.
It gives each implementation stage a bounded Claude Code session while treating
OpenSpec artifacts, the repository, and Git history as durable workflow state.

The initial release is terminal-only. Terminal rendering is isolated behind a
small `Ui` trait, and the workflow state machine, command construction, JSON
parsing, and repository safeguards are independent of the terminal
implementation so a later web frontend does not need to replace the core.

## Workflow

1. Persist the Git/OpenSpec planning baseline in local run metadata.
2. Invoke `/explore-unattended` in a new Claude session.
3. Resume that session for `/propose-unattended`.
4. Compare `openspec list --json` snapshots to identify one new or uniquely
   modified change.
5. Best-effort rename the planning session and save its ID/name mapping under
   `.git/ospx-build/last-run.json`.
6. Commit the selected change artifacts and any canonical
   `openspec/specs/**` updates as `openspec: propose <change>`.
7. Run Apply in a fresh Claude session.
8. Run Verify in fresh sessions. Correctable failures trigger bounded, fresh
   Repair/Apply sessions followed by another Verify.
9. Run Archive in a fresh session after verification succeeds.
10. Commit the implementation, tests, synchronized specs, and archive as
   `openspec: complete <change>`.

All Claude subprocesses are blocking. Claude output is requested as JSON and
captured with stdout, stderr, and exit status; ospx-build falls back to text
mode if an older Claude CLI explicitly rejects `--output-format`.

## Prerequisites

- the configured Claude launcher (`claude` by default), `openspec`, and `git`
  on `PATH`
- a Git repository with `openspec/config.yaml`
- project-local Claude skills `explore-unattended` and `propose-unattended`
- installed OpenSpec Apply, Verify, and Archive Claude workflows
- a Claude permission configuration suitable for unattended work (the default
  passed by ospx-build is `--permission-mode auto`)

The runner discovers these common OpenSpec forms:

- `.claude/skills/openspec-apply-change/SKILL.md`
- `.claude/skills/openspec-verify-change/SKILL.md`
- `.claude/skills/openspec-archive-change/SKILL.md`
- `.claude/commands/opsx/{apply,verify,archive}.md`

Every invocation is also configurable, which covers other delivery profiles
and future OpenSpec naming changes.

The Claude launcher is configurable too. Direct Claude Code is the default,
but a local oMLX model can be selected non-interactively with:

```sh
ospx-build \
  --claude-command "omlx launch claude" \
  --claude-model qwen3.6-35b-a3b \
  "add function pointer support"
```

This produces an argv beginning with:

```text
omlx launch claude --model qwen3.6-35b-a3b --print ...
```

`--claude-command` is parsed as a quoted command prefix without invoking a
shell. Single quotes, double quotes, and backslash escapes are supported;
shell variables, substitutions, redirects, and pipelines are intentionally
not evaluated. `--claude-model` is omitted when no explicit model is needed.

## Configuration

`ospx-build` automatically loads durable defaults from:

```text
$XDG_CONFIG_HOME/ospx-build/config.toml
```

or, when `XDG_CONFIG_HOME` is unset:

```text
~/.config/ospx-build/config.toml
```

Copy [`config.example.toml`](config.example.toml) to that location as a
starting point:

```toml
max_verify_retries = 3
permission_mode = "auto"

claude_command = "omlx launch claude"
claude_model = "qwen3.6-35b-a3b"

# Optional: retain the fail-closed policy for unexpected Claude commits.
# strict = true

# Optional workflow overrides:
# apply_command = "/opsx:apply"
# verify_command = "/opsx:verify"
# archive_command = "/opsx:archive"
```

Configuration precedence is:

```text
command-line flags
  > OSPX_BUILD_* environment variables
  > config file
  > built-in defaults
```

Use `--config PATH` or `OSPX_BUILD_CONFIG` to select another file. An explicit
file must exist and parse successfully. `--no-config` disables automatic and
environment-selected config loading for one invocation.

Supported environment overrides are:

- `OSPX_BUILD_MAX_VERIFY_RETRIES`
- `OSPX_BUILD_PERMISSION_MODE`
- `OSPX_BUILD_CLAUDE_COMMAND`
- `OSPX_BUILD_CLAUDE_MODEL`
- `OSPX_BUILD_EXPLORE_COMMAND`
- `OSPX_BUILD_PROPOSE_COMMAND`
- `OSPX_BUILD_APPLY_COMMAND`
- `OSPX_BUILD_VERIFY_COMMAND`
- `OSPX_BUILD_ARCHIVE_COMMAND`
- `OSPX_BUILD_STREAM_CLAUDE`
- `OSPX_BUILD_STRICT`

Unknown TOML keys are errors so misspelled unattended settings cannot be
silently ignored. Repository-local config is not loaded automatically because
`claude_command` controls executable invocation; use an explicit `--config`
only for repositories you trust.

## Interactive Claude testing

Use `--interactive` to open a normal Claude terminal session with the
configured launcher, model, and permission mode, without starting an OpenSpec
workflow:

```sh
ospx-build --interactive
```

Arguments after `--` are passed directly to Claude. This makes it convenient
to experiment with Claude Code invocation switches while retaining the oMLX
launcher and model from your config:

```sh
ospx-build --interactive -- \
  --effort xhigh \
  --autocompact 192k
```

An optional positional value becomes Claude's initial prompt:

```sh
ospx-build --interactive "inspect the repository architecture" -- \
  --effort high
```

Use ospx-build options before `--` and raw Claude options after it. For example,
to test another model or permission mode:

```sh
ospx-build \
  --interactive \
  --claude-model another-model \
  --permission-mode manual \
  -- --effort xhigh
```

Interactive mode inherits the terminal's stdin, stdout, and stderr. It does not
add `--print`, request JSON output, inspect Git/OpenSpec state, or capture the
conversation. Use `--interactive --dry-run` to print the exact command without
launching Claude.

## Diagnostics

Use `--debug` to show the resolved launcher configuration, discovered workflow
commands, Claude session modes and IDs, every subprocess command, and each
complete prompt sent to Claude:

```sh
ospx-build --debug "add function pointer support"
```

Debug output is written to stderr alongside the normal terminal UI. Complete
prompts can contain the original request, verification findings, and other
repository-derived content, so avoid copying debug logs into an untrusted
location without reviewing them.

`--verbose` has a different purpose: it prints captured subprocess stdout and
stderr. Use both switches when diagnosing the full request/response exchange:

```sh
ospx-build --debug --verbose "add function pointer support"
```

Debug and verbose are transient command-line switches and are deliberately not
config-file settings, which avoids accidentally leaving prompt disclosure on
for every unattended run. They also work with interactive dry runs, making it
easy to inspect the launcher and initial prompt without starting Claude:

```sh
ospx-build --interactive --dry-run --debug \
  "test this prompt" -- --effort xhigh
```

## Live Claude output

By default, unattended Claude stages remain quiet until they finish. During
testing, stream Claude's assistant text and concise tool/subagent activity as
it happens with:

```sh
ospx-build --stream-claude "add function pointer support"
```

The stream filter is selected independently of `--debug` and `--verbose`:

- `activity` (the default) shows assistant text, forwarded subagent text, and
  concise tool calls while suppressing potentially large tool results.
- `full` additionally shows tool results and Claude lifecycle events.
- `raw` prints each original Claude `stream-json` line without interpretation.

Select a non-default profile using an equals sign, which prevents the optional
filter argument from consuming the positional build request:

```sh
ospx-build --stream-claude=full "add function pointer support"
ospx-build --stream-claude=raw "add function pointer support"
```

The setting can be made a default in `config.toml`:

```toml
stream_claude = "activity"
```

or supplied as `OSPX_BUILD_STREAM_CLAUDE=activity`. Stream parsing and display
selection live in a dedicated module, [src/stream.rs](src/stream.rs), so adding
or changing filters does not affect process orchestration or terminal-status
handling. The orchestrator still waits synchronously for each Claude process,
captures its final result event, and parses `OSPX_STATUS` before advancing.

If the configured Claude launcher rejects `stream-json`, ospx-build warns and
retries that stage using its ordinary single-result JSON mode.

## Resuming an interrupted run

Resume the most recent durable run in a repository with:

```sh
ospx-build --repo ~/git/my-compiler --resume
```

No request is required: the original request and, once known, OpenSpec change
name come from `.git/ospx-build/last-run.json`. Supplying a request is allowed
only when it exactly matches the stored request, preventing an accidental
change of scope.

Resume starts at the recorded incomplete phase. Explore, Propose, and the
proposal milestone now have durable boundaries too. For example, interruption
during Apply starts a fresh Apply Claude session against the existing partial
implementation; it does not rerun planning. An interrupted Propose retains its
original OpenSpec change-list baseline and planning session, preventing a new
run from creating a second slice. Verify, Repair, Archive, and the final commit
have equivalent durable boundaries. An interrupted Repair retains the
verification finding that triggered it.

Before continuing, ospx-build verifies:

- the metadata schema and kebab-case change name;
- that current Git HEAD is the recorded commit or a linear descendant of it;
- the state of dirty files which predated the original run;
- that the OpenSpec change remains active when the recorded phase requires it;
- the stored verification retry count against the current retry limit.

Partial files created by the interrupted phase remain available to its fresh
Claude session. Pre-existing dirty paths remain excluded from orchestrator
milestone commits, even if a compiler or test updates them. Resume never resets,
restores, or stashes the working tree.

In the default pragmatic policy, linear commits made by Claude or a project
hook during a stage are adopted into the run and recorded in the checkpoint.
This makes an interrupted Apply or a project workflow that commits its own
work resumable without manual history surgery. Use `--strict` to restore the
older fail-closed behaviour.

Inspect the selected restart point without executing it using:

```sh
ospx-build --repo ~/git/my-compiler --resume --dry-run
```

If Archive completed immediately before interruption and the active change is
already absent, resume advances to the final milestone. If the completion
commit itself succeeded immediately before interruption, ospx-build recognizes
it by its parent and exact milestone subject rather than creating a duplicate.

## Aborting a planning transaction

Abort an unfinished run before its proposal milestone commit with:

```sh
ospx-build --repo ~/git/my-compiler --abort
```

Abort is intentionally explicit rather than automatic. A failed proposal can
contain useful work, so ordinary failures retain both the files and checkpoint
for inspection or `--resume`.

Before cleanup, ospx-build verifies the original dirty-file fingerprints and
Git HEAD. It then restores tracked paths and removes newly created paths that
were absent from the checkpoint baseline. The checkpoint is deleted only after
the resulting HEAD, working tree, and index exactly reproduce that baseline.
Use a dry run to inspect every affected path first:

```sh
ospx-build --repo ~/git/my-compiler --abort --dry-run
```

While a checkpoint is unfinished, starting another build is refused with a
diagnostic directing you to `--resume` or `--abort`. This prevents one failed
proposal from being mistaken for pre-existing state by a second proposal.

Abort is limited to Explore, Propose, and Proposal Commit before a milestone
commit exists. Once the proposal is committed, the run has durable Git history
and aborting it requires an explicit history-level policy rather than working-
tree cleanup.

As with any ownership scheme based on a checkpoint, a file created manually
after the run began is indistinguishable from a file created by that run. Review
`--abort --dry-run` output before cleanup if the repository was edited
concurrently or after interruption.

## Installation

From this repository:

```sh
cargo install --path .
```

Or run it directly with Cargo:

```sh
cargo run -- "add function pointer support" --repo /path/to/project
```

## Usage

```text
ospx-build [OPTIONS] <REQUEST>
```

Examples:

```sh
ospx-build "add function pointer support"

ospx-build \
  --repo ~/git/my-compiler \
  --max-verify-retries 2 \
  --verbose \
  "add function pointer support"

ospx-build \
  --repo ~/git/my-compiler \
  --dry-run \
  "add function pointer support"

ospx-build \
  --repo ~/git/my-compiler \
  --debug \
  "add function pointer support"

ospx-build \
  --repo ~/git/my-compiler \
  --resume

ospx-build \
  --repo ~/git/my-compiler \
  --abort --dry-run

ospx-build \
  --apply-command /opsx:apply \
  --verify-command /opsx:verify \
  --archive-command /opsx:archive \
  "add function pointer support"

ospx-build \
  --config ~/.config/ospx-build/workstation.toml \
  "add function pointer support"

ospx-build --interactive -- --effort xhigh
```

Run `ospx-build --help` for all command overrides and options.

## Git safety model

The runner never resets, stashes, discards, force-checks-out, or amends user
work.

At startup it fingerprints every pre-existing dirty path in both the working
tree and index. The proposal commit is restricted to the detected OpenSpec
change directory plus canonical spec updates under `openspec/specs/`; exact
changed paths are committed rather than either directory wholesale. In the
default policy, changes elsewhere during Propose are reported and left out of
the proposal commit. Strict mode blocks on them. The final milestone excludes
pre-existing dirty paths.

After the proposal milestone, the default policy is pragmatic. Git HEAD and
pre-existing dirty-file fingerprints are checked immediately before and after
every Apply, Verify, Repair, and Archive invocation. If a compiler or test
updates a path which was already dirty when the run began (for example
`a.out`), ospx-build warns, records the path, excludes it from orchestrator
milestone commits, and continues. If a stage creates one or more commits on top
of the recorded HEAD, ospx-build adopts that linear history, records the commit
IDs in `.git/ospx-build/last-run.json`, and continues. If those commits leave no
uncommitted completion paths, the latest adopted commit serves as the
completion milestone instead of manufacturing an empty commit.

The runner still stops when history diverges, the selected OpenSpec change has
no proposal artifacts to commit, or its own proposal directory was already
dirty at the initial checkpoint. Those cases would require rewriting history or
guessing at ownership. Use `--strict` to retain the former fail-closed policy in
which any unexpected commit, unrelated proposal-stage path, or changed pre-run
dirty path stops the run:

```sh
ospx-build --strict --resume
```

`strict = true` in the config file or `OSPX_BUILD_STRICT=true` makes that policy
the default for a workstation.

Unrelated staged changes are left staged. Git commits use explicit pathspecs so
those changes are not swept into either milestone.

## Terminal protocol

Each skill prompt requires one final machine marker:

```text
OSPX_STATUS: READY
OSPX_STATUS: VERIFIED
OSPX_STATUS: RETRY
OSPX_STATUS: BLOCKED
```

Explore, Propose, Apply/Repair, and Archive use `READY` or `BLOCKED`. Verify
uses `VERIFIED`, `RETRY`, or `BLOCKED`. A missing or invalid marker is treated
as an orchestration error rather than inferred from prose.

## Local metadata

The latest run metadata is written to:

```text
.git/ospx-build/last-run.json
```

Because it lives inside Git's metadata directory, it is local and inherently
excluded from commits. It is created before Explore and records the original
OpenSpec change list, eventual change name, workflow stage, verification
retry/finding state, original dirty-tree fingerprints, milestone commits, and
Claude session mappings. It also records the expected Git HEAD, linear
descendant commits adopted during the run, and changed pre-run dirty paths that
were tolerated and excluded from milestone commits. Writes use a temporary file
plus atomic rename. It is not an asynchronous marker and the runner never
watches it.

## Current limitations

- The process is synchronous and single-run; there is no daemon, web UI, or
  concurrent job management yet.
- Repository paths reported by Git must be valid UTF-8.
- Change selection requires exactly one new change, one uniquely modified
  existing change, or a repository with only one active change.
- Session rename is best-effort. Failure does not stop the build because the
  UUID/name mapping is retained in local metadata.
- Verification quality depends on the installed Verify skill and project test
  commands. `ospx-build` enforces the stage protocol but does not replace
  OpenSpec's semantic verification.
- Explicit abort is available only before the proposal milestone commit. It
  does not rewrite durable Git history for later phases.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build
```
