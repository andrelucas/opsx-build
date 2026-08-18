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

If a Claude result reports `stop_reason: max_tokens` or a
`max_output_tokens` API failure, ospx-build treats the turn as interrupted
rather than failed. It best-effort compacts that same Claude session and asks it
to continue the current phase from durable repository state. This recovery is
bounded by `max_output_retries` (default 3); exhausting it leaves the checkpoint
at the current phase for an ordinary `--resume`.

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
max_output_retries = 3
permission_mode = "auto"
claude_command = "omlx launch claude"
claude_model = "qwen3.6-35b-a3b"
auto_compact_window = "128k"
# Alternatively, compact at a percentage of the launcher's effective capacity.
# auto_compact_percent = 50
max_output_tokens = "8k"
# Optional: reveal Claude's normal assistant/tool activity in the dashboard.
# stream_claude = "activity"
```

Set the same limits for one invocation with:

```sh
ospx-build --auto-compact-window 128k --max-output-tokens 8k \
  "add function pointer support"
```

Output-limit recovery can likewise be adjusted for one run:

```sh
ospx-build --max-output-retries 5 "add function pointer support"
```

Token counts may be plain integers (`8192`) or use binary `k`/`m` suffixes;
`8k` therefore means 8,192 tokens. `auto_compact_percent` accepts an integer
from 1 to 100 and applies to the effective capacity selected by Claude or its
launcher, so ospx-build does not need model visibility. The configured values
are exported to every Claude subprocess as
`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, and
`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, including interactive launcher tests, without
changing the parent shell. Leaving a setting absent preserves Claude's or the
launcher's default.

The oMLX launcher supplies the selected model's capacity when no explicit
window is present. Current oMLX versions preserve an inherited absolute window
and percentage override. As an alternative to `auto_compact_window = "128k"`,
`auto_compact_percent = 50` against a 262,144-token capacity targets
approximately 131,072 tokens. A lower output limit bounds slow decode latency
but can cause more continuation turns; output-limit recovery handles those
turns.

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
- `OSPX_BUILD_AUTO_COMPACT_PERCENT`
- `OSPX_BUILD_MAX_OUTPUT_TOKENS`
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

For bidirectional streaming, the launcher must pass newline-delimited messages
from stdin to Claude before EOF, preserve multiple `result` events on stdout,
and leave the pipe open for later messages. Ordinary and slash-command turns use
Claude's `user` message shape. Mid-command compaction additionally uses Claude's
`control_request`/`control_response` interrupt protocol, which is the streaming
equivalent of pressing Escape in the interactive terminal. `/compact` behavior
and `compact_boundary` events follow Claude Code's
[slash-command protocol][claude-slash-commands]. These are Claude transport
requirements, not oMLX APIs.

When token policies are configured, the launcher must preserve
`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, and
`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, unless it explicitly owns one of those policy
choices. The output limit is Claude Code's documented
[maximum-output environment variable][claude-env-vars].

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
`Qwen3.6-35B-A3B-4bit`. The interrupt → slash command → resume cycle was also
tested with both `/compact` and `/context` through this oMLX launcher.

## Terminal dashboard and live diagnostics

Normal workflow runs use the alternate-screen dashboard automatically when
stdin and stderr are terminals. The dashboard itself is the standard terminal
presentation; it is not enabled by `--stream-claude`.

By default the dashboard shows orchestration events, phase progress, elapsed
times, and explicit diagnostics such as `/context`, while suppressing Claude's
ordinary assistant/tool chatter. Show that additional activity with:

```sh
ospx-build --stream-claude "add function pointer support"
```

Each workflow phase has its own disclosure, headed by status, stage position,
phase name, retry/pass number when applicable, live/final elapsed time, and
captured line count. The current phase starts expanded; completed phases
collapse but remain available with their elapsed time frozen:

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

- `c` sends Claude's interrupt control request—the streaming equivalent of
  Escape—then runs `/compact` and reissues the interrupted stage command (once
  per invocation);
- `C` follows the same interrupt/resume cycle but runs `/context`, leaving
  Claude's context report in the phase disclosure for debugging;
- `i` opens a steering prompt; Enter interrupts the active Claude turn and
  delivers the entered instruction as its continuation;
- Escape cancels the injection prompt;
- Tab or Left/Right selects a phase disclosure;
- Enter, Space, `o`, or a click on a heading expands/collapses that phase;
- Up/Down and Page Up/Page Down scroll expanded output;
- End returns to the newest output;
- Escape collapses the selected phase;
- Ctrl-C interrupts the current subprocess while preserving its ospx-build
  checkpoint.

All three injection controls interrupt the active turn first. `i` delivers the
entered text as the continuation, which lets it steer the work already in
progress. `c` and `C` send their slash command, wait for that result, and then
reissue the exact stage input because diagnostic/maintenance slash commands do
not themselves continue the work. None rolls back repository changes made
before interruption; OpenSpec and the repository remain the durable state from
which Claude continues.
The dashboard records each step. A write or interrupt acknowledgement is not a
compaction acknowledgement; successful compaction is reported by Claude's
`compact_boundary` event, which is shown by the `activity` filter. If the
launcher rejects interrupt controls, ospx-build degrades to
queuing the requested command after the current turn. These controls are
available in the TTY dashboard; linear non-TTY streaming remains output-only.

Repeated Verify and Repair phases are retained separately as `pass 2`,
`pass 3`, and so on. The dashboard restores the previous terminal screen when
the workflow completes or stops.
When either stdin or stderr is not a TTY—for example under CI, redirection, or
a pipe—ospx-build uses ordinary linear output and does not write cursor-control
sequences. If `--stream-claude` is enabled there, its selected filtered stream
is emitted as linear text.

`--stream-claude` filters are:

- `activity` (used when the flag has no value): assistant/subagent text and
  concise tool calls;
- `full`: activity plus tool results and lifecycle events;
- `raw`: original Claude `stream-json` lines.

Select one with `--stream-claude=full` or `--stream-claude=raw`.

`--debug` displays resolved commands, session IDs, and complete prompts.
`--verbose` displays captured subprocess stdout and stderr. These are transient
flags and are not read from the config file.

[claude-streaming]: https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode
[claude-slash-commands]: https://code.claude.com/docs/en/agent-sdk/slash-commands
[claude-env-vars]: https://code.claude.com/docs/en/env-vars

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
