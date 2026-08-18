# ospx-build

`ospx-build` is a small synchronous runner for unattended OpenSpec builds. It
runs each Claude/OpenSpec phase, records the next phase in local metadata, and
lets the repository, OpenSpec artifacts, and Claude do their own jobs.

It deliberately does **not** infer file ownership, fingerprint dirty files,
police Git HEAD, reset the repository, or clean up work. Claude creates the two
milestone commits under explicit non-destructive instructions.

## Workflow

1. Run `/explore-unattended` in a new Claude planning session.
2. Explicitly `/compact` that session.
3. Run `/propose-unattended` in the compacted planning session.
4. Determine the OpenSpec change name from `openspec list --json`, then
   explicitly `/compact` the planning session again.
5. Ask Claude to commit the proposal as `openspec: propose <change>`.
6. Run the installed OpenSpec Apply workflow in a fresh session.
7. Run Verify in fresh sessions. A `RETRY` result starts a fresh directed
   Repair/Apply session and then verifies again.
8. Run Archive in a fresh session after verification succeeds.
9. Ask Claude in another fresh session to commit the completed change as
   `openspec: complete <change>`.

Every Claude invocation is a blocking subprocess. The checkpoint records only
workflow facts: request, change name, next stage, planning session, retry count,
pending verifier finding, pending user direction, and milestone HEADs.

With live streaming enabled, ospx-build uses Claude Code's documented
[bidirectional `stream-json` transport][claude-streaming]. It keeps the current
Claude process's stdin open until the phase and any interactively queued turns
finish. This allows terminal input to enqueue a slash command or ordinary
follow-up without turning the workflow into an asynchronous marker-file
protocol.

Only Explore, Propose, and the proposal commit reuse the planning session.
Apply, Verify, Repair, Archive, and completion work already use disposable
fresh sessions, so there is no carried context to compact at those boundaries.
Explicit planning compaction is best-effort: a launcher incompatibility is
reported as a warning and does not block otherwise valid repository work.

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

## Continuing an existing OpenSpec change

When Explore and Propose were performed outside ospx-build, continue the sole
active OpenSpec change with:

```sh
ospx-build --repo ~/git/my-project --continue-existing
```

If several changes are active, select one explicitly:

```sh
ospx-build --repo ~/git/my-project --continue-existing \
  --change fix-test-infrastructure
```

This does not rerun Explore or Propose. It deliberately enters at the proposal
commit milestone: Claude commits uncommitted planning artifacts if necessary,
or treats that step as complete when they are already committed. Apply then
continues any remaining OpenSpec tasks, followed by Verify, Archive, and the
completion commit. Each stage inspects durable repository/OpenSpec state, so a
separate model call does not have to guess which stages are safe to skip.

`--continue-existing --dry-run` shows the selected change and entry stage
without writing a checkpoint. An unfinished ospx-build checkpoint must still be
resumed or forgotten before adopting another change; a completed checkpoint is
replaced by the adopted run.

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
terminal results, missing prerequisites, and retry-limit exhaustion are normal
errors. Their checkpoint remains at the unfinished stage for resume.

## Claude launcher and configuration

The default launcher is `claude`. oMLX Claude mode can be configured with:

```toml
# ~/.config/ospx-build/config.toml
max_verify_retries = 3
permission_mode = "auto"
claude_command = "omlx launch claude"
claude_model = "qwen3.6-35b-a3b"
auto_compact_window = "192k"
stream_claude = "activity"
```

Set the same threshold for one invocation with:

```sh
ospx-build --auto-compact-window 192k "add function pointer support"
```

Token counts may be plain integers (`196608`) or use binary `k`/`m` suffixes;
`192k` therefore means 196,608 tokens. The configured value is exported to
every Claude subprocess as `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, including
interactive launcher tests, without changing the parent shell. Percentage
thresholds are not currently accepted because ospx-build has no portable way
to discover a launcher's effective model context window.

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
- `OSPX_BUILD_AUTO_COMPACT_WINDOW`
- `OSPX_BUILD_EXPLORE_COMMAND`
- `OSPX_BUILD_PROPOSE_COMMAND`
- `OSPX_BUILD_APPLY_COMMAND`
- `OSPX_BUILD_VERIFY_COMMAND`
- `OSPX_BUILD_ARCHIVE_COMMAND`
- `OSPX_BUILD_STREAM_CLAUDE`

## Claude launcher compatibility

`ospx-build` does not call an oMLX API. It starts the configured launcher as a
synchronous subprocess and appends ordinary Claude Code CLI arguments. The
launcher command is split into an executable and literal prefix arguments; it
is not evaluated by a shell.

For example, this configuration:

```toml
claude_command = "omlx launch claude"
claude_model = "Qwen3.6-35B-A3B-4bit"
```

produces commands beginning with:

```text
omlx launch claude --model Qwen3.6-35B-A3B-4bit ...
```

A compatible launcher must run Claude Code synchronously, preserve its exit
status, keep machine-readable results on stdout, and forward these Claude Code
options:

- `--print` and `--permission-mode`;
- `--session-id`, `--resume`, and `--name`;
- `--model` when `claude_model` is configured;
- `--output-format json` for normal unattended operation;
- `--input-format stream-json --output-format stream-json --verbose
  --forward-subagent-text` when live streaming is enabled;
- preferably `--json-schema`, returning `structured_output` in the final JSON
  result event.

For bidirectional streaming, the launcher must pass newline-delimited `user`
messages from stdin to Claude before EOF, preserve multiple `result` events on
stdout, and leave the pipe open for later messages. Slash commands use this same
transport; `/compact` behavior and `compact_boundary` events follow Claude
Code's [slash-command protocol][claude-slash-commands]. These are Claude
transport requirements, not oMLX APIs.

When `auto_compact_window` is configured, the launcher must also preserve the
`CLAUDE_CODE_AUTO_COMPACT_WINDOW` environment variable for Claude Code.

For JSON output, ospx-build reads `is_error`, `result`, `session_id`, and
`structured_output`. For streaming output it finds the most recent JSONL result
carrying a valid OpenSpec stage status; later slash-command results such as
`/compact` do not replace the stage result. A launcher may write diagnostics to
stderr, but should not mix banners or unrelated prose into non-streaming JSON
stdout.

`--json-schema` is optional for compatibility: if the launcher clearly rejects
that option before execution, ospx-build retries once using the documented
`OSPX_STATUS` marker protocol. It similarly degrades from streaming JSON to
ordinary JSON or text when an older Claude CLI clearly rejects `--input-format`
or `--output-format`. It never retries merely because a successful, potentially
mutating invocation returned malformed terminal data.

### oMLX-specific behavior

The only oMLX-specific configuration is the launcher prefix and model name.
The oMLX server must already be listening, normally on its default local port;
ospx-build does not start, stop, configure, or query that server. `omlx launch
claude` must forward the Claude Code options above and preserve the streaming
stdin pipe. Structured and streaming output were tested through this launcher
with Claude Code 2.1.224 and
`Qwen3.6-35B-A3B-4bit`.

## Live output and diagnostics

Stream Claude's normal assistant text and concise tool activity:

```sh
ospx-build --stream-claude "add function pointer support"
```

When stdin and stderr are terminals, streaming automatically uses an
alternate-screen dashboard for the whole run. Each workflow phase has its own
disclosure, headed by status, stage position, phase name, retry/pass number
when applicable, live/final elapsed time, and captured line count. The current
phase starts expanded; completed phases collapse but remain available with
their elapsed time frozen:

```text
ospx-build  change: add-function-pointers  /path/to/repository
[4/7] Apply · 14m 07s  ⠴ Claude is applying the OpenSpec change
──────────────────────────────────────────────────────
  ▶ ✓ [1/7] Explore · 3m 12s · 48 lines
  ▶ ✓ [2/7] Propose · 8m 41s · 31 lines
  ▶ ✓ [3/7] Proposal commit · 22s · 12 lines
› ▼ ⠴ [4/7] Apply · 14m 07s · 137 lines
```

The change name appears as soon as proposal discovery records it in the
ospx-build checkpoint. Resumed and `--continue-existing` runs show it from the
start.

Controls:

- `c` queues `/compact` as Claude's next turn (once per Claude invocation);
- `i` opens an injection prompt; type any Claude slash command or ordinary
  follow-up instruction, then press Enter to queue it as the next turn;
- Escape cancels the injection prompt;
- Tab or Left/Right selects a phase disclosure;
- Enter, Space, `o`, or a click on a heading expands/collapses that phase;
- Up/Down and Page Up/Page Down scroll expanded output;
- End returns to the newest output;
- Escape collapses the selected phase;
- Ctrl-C interrupts the current subprocess while preserving its ospx-build
  checkpoint.

Injected messages do not interrupt an agentic turn already in progress. Claude
finishes that turn and processes queued input afterward. The dashboard records
the queued text, and a `compact_boundary` event is shown even with the default
`activity` filter. These controls are available in the TTY dashboard; linear
non-TTY streaming remains output-only.

Repeated Verify and Repair phases are retained separately as `pass 2`,
`pass 3`, and so on. The dashboard restores the previous terminal screen when
the workflow completes or stops.
When either stdin or stderr is not a TTY—for example under CI, redirection, or
a pipe—ospx-build emits the same filtered stream as ordinary linear text and
does not write cursor-control sequences.

Filters are:

- `activity` (default): assistant/subagent text and concise tool calls;
- `full`: activity plus tool results and lifecycle events;
- `raw`: original Claude `stream-json` lines.

Select one with `--stream-claude=full` or `--stream-claude=raw`.

`--debug` displays resolved commands, session IDs, and complete prompts.
`--verbose` displays captured subprocess stdout and stderr. These are transient
flags and are not read from the config file.

[claude-streaming]: https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode
[claude-slash-commands]: https://code.claude.com/docs/en/agent-sdk/slash-commands

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

Each unattended stage asks Claude Code for schema-validated structured output.
The result contains an `ospx_status` and a concise `summary`; Claude Code can
re-prompt the model when its first result does not satisfy the schema.

For older or compatibility-layer Claude CLIs that reject `--json-schema`,
ospx-build falls back to final-line markers:

```text
OSPX_STATUS: READY
OSPX_STATUS: VERIFIED
OSPX_STATUS: RETRY
OSPX_STATUS: BLOCKED
```

Explore, Propose, Apply, Repair, Archive, and commit stages use `READY` or
`BLOCKED`. Verify uses `VERIFIED`, `RETRY`, or `BLOCKED`.

If the Claude process exits successfully but returns neither structured output
nor a fallback marker, ospx-build does not rerun that potentially mutating
stage. Archive additionally checks durable OpenSpec state so a completed
archive is not repeated merely because its acknowledgement was malformed.

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
