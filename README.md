# ospx-build

`ospx-build` is a small synchronous runner for unattended OpenSpec builds. It
runs each Claude/OpenSpec phase, records the next phase in local metadata, and
lets the repository, OpenSpec artifacts, and Claude do their own jobs.

It deliberately does **not** infer file ownership, fingerprint dirty files,
police Git HEAD, reset the repository, or clean up work. Claude creates the two
milestone commits under explicit non-destructive instructions.

## Workflow

1. Run `/explore-unattended` in a new Claude session.
2. Run `/propose-unattended` in that same session.
3. Determine the OpenSpec change name from `openspec list --json`.
4. Ask Claude to commit the proposal as `openspec: propose <change>`.
5. Run the installed OpenSpec Apply workflow in a fresh session.
6. Run Verify in fresh sessions. A `RETRY` result starts a fresh directed
   Repair/Apply session and then verifies again.
7. Run Archive after verification succeeds.
8. Ask Claude to commit the completed change as `openspec: complete <change>`.

Every Claude invocation is a blocking subprocess. The checkpoint records only
workflow facts: request, change name, next stage, planning session, retry count,
pending verifier finding, pending user direction, and milestone HEADs.

## Installation

```sh
cargo install --path /Users/andre/git/ospx-build --force
```

## Basic usage

```sh
ospx-build --repo ~/git/my-project "add function pointer support"
```

Resume after interruption or an ordinary command error:

```sh
ospx-build --repo ~/git/my-project --resume
```

Inspect what resume would do without changing the checkpoint or repository:

```sh
ospx-build --repo ~/git/my-project --resume --dry-run
```

## Directing the next iteration

Supply durable, one-shot implementation guidance while resuming:

```sh
ospx-build --repo ~/git/my-project --resume \
  --direction "Keep the AST representation; fix lowering instead."
```

The direction is written to the checkpoint before Claude starts and remains
pending if the process is interrupted. It is injected into the next Apply or
Repair prompt and removed only after that code-changing stage succeeds.

When a run is waiting at Verify, Archive, or the completion commit, supplying a
direction reopens Repair and then returns through Verify. Direction is
implementation guidance; revise the OpenSpec artifacts separately when the
approved requirements themselves must change.

## Forgetting a checkpoint

Discard only ospx-build's local workflow pointer with:

```sh
ospx-build --repo ~/git/my-project --forget
```

This does not inspect, restore, reset, stash, delete, or otherwise modify
repository files or Git history. A dry run is also available:

```sh
ospx-build --repo ~/git/my-project --forget --dry-run
```

Checkpoints are stored at `.git/ospx-build/last-run.json`. Schema-2 checkpoints
from the original implementation are migrated in memory and written in the new
schema when a real resume proceeds.

## Git behavior

The orchestrator never runs destructive Git operations and never decides which
working-tree paths belong to whom.

At proposal and completion milestones, Claude is instructed to inspect status,
history, and diffs; commit only work belonging to the OpenSpec change; preserve
unrelated work; and never reset, stash, restore, discard, amend, or rewrite
history. If the relevant work is already committed, Claude may report success
without manufacturing an empty commit.

`BLOCKED` has one meaning: Claude explicitly reported that progress requires a
human decision or unavailable external input. Subprocess failures, malformed
terminal markers, missing prerequisites, and retry-limit exhaustion are normal
errors. Their checkpoint remains at the unfinished stage for resume.

## Claude launcher and configuration

The default launcher is `claude`. oMLX Claude mode can be configured with:

```toml
# ~/.config/ospx-build/config.toml
max_verify_retries = 3
permission_mode = "auto"
claude_command = "omlx launch claude"
claude_model = "qwen3.6-35b-a3b"
stream_claude = "activity"
```

Configuration precedence is:

```text
command-line flags
  > OSPX_BUILD_* environment variables
  > config file
  > built-in defaults
```

Use `--config PATH` to select another file or `--no-config` to disable config
loading. Unknown TOML keys are errors.

Workflow command names are discovered from common Claude skill and command
locations. Override them when needed:

```sh
ospx-build \
  --apply-command /opsx:apply \
  --verify-command /opsx:verify \
  --archive-command /opsx:archive \
  "add function pointer support"
```

The corresponding environment variables are:

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

## Live output and diagnostics

Stream Claude's normal assistant text and concise tool activity:

```sh
ospx-build --stream-claude "add function pointer support"
```

Filters are:

- `activity` (default): assistant/subagent text and concise tool calls;
- `full`: activity plus tool results and lifecycle events;
- `raw`: original Claude `stream-json` lines.

Select one with `--stream-claude=full` or `--stream-claude=raw`.

`--debug` displays resolved commands, session IDs, and complete prompts.
`--verbose` displays captured subprocess stdout and stderr. These are transient
flags and are not read from the config file.

## Interactive launcher testing

Open an ordinary interactive Claude session using the configured launcher,
model, and permission mode:

```sh
ospx-build --interactive -- --effort xhigh
```

An optional positional argument becomes the initial prompt. Arguments after
`--` pass directly to Claude. Interactive mode does not read or update the
OpenSpec workflow checkpoint.

## Terminal protocol

Each unattended stage ends with one marker:

```text
OSPX_STATUS: READY
OSPX_STATUS: VERIFIED
OSPX_STATUS: RETRY
OSPX_STATUS: BLOCKED
```

Explore, Propose, Apply, Repair, Archive, and commit stages use `READY` or
`BLOCKED`. Verify uses `VERIFIED`, `RETRY`, or `BLOCKED`.

## Prerequisites and limitations

- The configured Claude launcher, `claude`, `openspec`, and `git` must be on
  `PATH`.
- The repository must contain `openspec/config.yaml` and the unattended Explore
  and Propose skills.
- Apply, Verify, and Archive workflow names must be discoverable or configured.
- Change discovery requires one new change, one uniquely modified change, or
  only one active change.
- The process is synchronous and single-run; there is no daemon or web UI.
- Claude is responsible for the quality and scope of Git commits. The runner
  intentionally does not second-guess them.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build
```
