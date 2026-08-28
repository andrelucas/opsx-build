# opsx-build

`opsx-build` is a small synchronous runner for unattended OpenSpec builds. It
runs each Claude/OpenSpec phase, records the next phase in local metadata, and
lets the repository, OpenSpec artifacts, and Claude do their own jobs.

During an ordinary run it deliberately does **not** infer file ownership,
fingerprint dirty files, or police Git HEAD. Claude creates the two milestone
commits under explicit non-destructive instructions. The one deliberate
exception is frontier escalation: before Propose, opsx-build records a private
rollback baseline so a failed oversized local attempt can be discarded without
rejecting or losing pre-existing staged, unstaged, or untracked work.

`opsx-build` bundles the canonical `explore-unattended` and
`propose-unattended` Claude skills. Before a workflow starts, it installs
missing copies and replaces stale copies in the repository selected by
`--repo`, under `.claude/skills`. This keeps the target project's local Claude
configuration self-contained. `--dry-run` reports the installation or update
without writing it. The retired monolithic `build-unattended` skill is not
installed because orchestration belongs in this process.

## Bootstrap a new project

Create a Markdown file that states the project goal, technology choices,
constraints, and a concrete definition of done, then run:

```sh
opsx-build --repo ~/git/my-project \
  --frontier-connection openrouter-kimi \
  bootstrap --context project.md
```

Project contexts may contain strict `{{name}}` placeholders. Supply each value
with a repeatable `--define NAME=VALUE` option:

```sh
opsx-build --frontier-connection openrouter-kimi \
  --define language=Go \
  --context proxy-context.md \
  bootstrap
```

Substitution is deliberately textual and non-recursive. Bootstrap rejects
missing definitions, duplicate names, malformed placeholders, and definitions
that are not used by the context. It supports no conditionals, includes, loops,
or executable template expressions.

Bootstrap requires an existing Git repository but no existing OpenSpec setup.
It uses the configured **frontier** connection for the complete one-time
planning workflow; the worker connection is not used. The context path is an
input source and may live inside or outside the target repository. opsx-build
embeds its contents into `openspec/config.yaml` but does not assume the source
file itself should be committed.

The command:

1. initializes OpenSpec non-interactively for Claude;
2. creates `openspec/config.yaml` from the supplied context;
3. installs the reusable planning brief at `automation/bootstrap.md`;
4. adds or refreshes only the marked `opsx-build` fragment in `CLAUDE.md`,
   preserving all user-owned text outside its markers; the managed fragment
   includes workflow safety, revisable dependency selection,
   language-server-first navigation, and scoped canonical source-formatting
   policy;
5. runs the planning-only `bootstrap-implementation-slices` change through the
   existing Propose, milestone commit, Apply, Verify/repair, Archive, and final
   commit stages; and
6. checks that the result is a parseable ordered agenda with a README, bounded
   slice contracts, and the terminal whole-project gate
   `automation/slices/9999-project-acceptance.md`.

The bootstrap planner is explicitly told to use the OpenSpec project context
rather than rediscovering the source tree, to create no product code, and to
cover the complete goal rather than stopping at an attractive early milestone.
Once the final `9999` slice is archived, `advance` has a concrete definition of
DONE.

Bootstrap uses the same durable checkpoint and fresh-session stage engine as a
normal build. If it is interrupted after initialization, continue with:

```sh
opsx-build --repo ~/git/my-project --resume
```

The checkpoint remembers that this is a frontier bootstrap; resume will not
silently move it onto the local worker. Use `--dry-run` to inspect the planned
initialization and generated paths without writing or invoking Claude.

This first version intentionally does not merge into an existing OpenSpec
configuration or replace an existing agenda. It stops before initialization if
either already exists. The selected OpenSpec profile must expose Propose,
Apply, Verify, and Archive actions; if it does not, enable those actions and run
`openspec update` before resuming.

## Workflow

`advance` is the well-known ordered-agenda operation and the default when no
request is supplied. If the repository contains files named
`automation/slices/<number>-slug.md` (for example `001-lexer.md` or
`0001-lexer.md`), opsx-build sorts them by numeric prefix, treats a
slice as complete only when a matching OpenSpec change has been archived, and
assigns the earliest unarchived slice. A date-prefixed archived or active
change still matches when its name ends with the complete slice stem.
Hierarchical ordinals such as `0009.1-short-circuit.md` are sorted numerically
(`0009.2` precedes `0009.10`) and map to kebab-case OpenSpec names such as
`0009-1-short-circuit`.

An advance operation persists the exact slice path, contents, and required
OpenSpec change name before Claude starts. It skips Explore because the
frontier-authored slice is already the exploration result, then gives Propose
the complete slice contents. Propose must create or continue only the assigned
change; opsx-build never infers intent from Claude's prose or silently adopts a
different slice. When every agenda entry is archived, advance returns `DONE`
without invoking Claude.

Free-form requests retain the exploratory workflow:

```sh
opsx-build --change remote-path-prefix \
  "Implement optional remote base-URI path prefixes"
```

`--change NAME` prescribes the exact OpenSpec change name for a free-form
request. Propose must create only that change; opsx-build validates it directly
rather than inferring the name from the set of active changes. Omit `--change`
to retain automatic naming. A prescribed name cannot be combined with a
multi-change campaign.

1. Run `/explore-unattended` in a new Claude planning session.
2. Explicitly `/compact` that session.
3. Run `/propose-unattended` in the compacted planning session. If the
   requested objective is already satisfied and no meaningful change remains,
   Propose returns `DONE` without creating artifacts. If it instead reports
   `READY` while OpenSpec still has no new or modified active change, or while
   `openspec status --json` reports `isPlanningComplete: false`, retry a
   corrective Propose turn once in that same session before reporting a failed
   postcondition. The bundled skills forbid background commands and detached
   subagents so READY cannot race unfinished proposal work.
4. Determine the OpenSpec change name from `openspec list --json`, then retire
   the planning session.
5. Ask Claude in a fresh session to commit the proposal as
   `openspec: propose <change>`.
6. Run the installed OpenSpec Apply workflow in a fresh session. Worker stages
   may return `TOO_LARGE` when a frontier-assigned slice cannot reliably fit
   one bounded local-model change.
7. Run Verify in fresh sessions. A `RETRY` result starts a fresh directed
   Repair/Apply session and then verifies again.
8. Run Archive in a fresh session after verification succeeds.
9. Ask Claude in another fresh session to commit the completed change as
   `openspec: complete <change>`.

If Archive finishes without a usable terminal result, opsx-build first checks
OpenSpec's durable state. An absent active change proves that archival completed.
If the change remains active, Archive is retried once in a fresh Claude session;
explicit `BLOCKED` results and substantive process errors are not retried.

Provider API failures emitted as Claude assistant activity are preserved and
reported directly even when the stream omits the normal stage result. They are
treated as substantive errors rather than missing-protocol retries, and the
workflow checkpoint remains available for a later `--resume`.

Before each Claude milestone-commit session, opsx-build records the current
Git commit. It proceeds only if that commit remains an ancestor of the result.
This catches rewritten or displaced committed history without imposing rules
about paths, dirty files, commit count, merges, or other working-tree shape.

Every Claude invocation is a blocking subprocess. The checkpoint records only
workflow facts: request, assigned agenda slice when advancing, change name,
next stage, planning session, retry count, pending verifier finding, pending
user direction, milestone HEADs, and any `TOO_LARGE` decomposition report.

`TOO_LARGE` is a normal worker-routing outcome, distinct from `BLOCKED` and
ordinary failure. It is accepted from Explore, Propose, Apply, and Repair.
Agenda runs also escalate when a bounded worker stage exceeds
`local_worker_timeout_minutes`, exhausts output-limit recovery, exhausts the
Verify/Repair allowance, or receives the dashboard's `f` command.

On escalation, opsx-build retains a diagnostic and any failed commits under
private Git metadata, then restores the exact pre-Propose Git-visible state. A
fresh frontier Claude process narrows the original agenda file, adds one or
more hierarchical child slices, and commits only `automation/slices/`.
Deterministic postconditions require the original first slice to remain next,
at least one child to exist, OpenSpec state to be unchanged, all frontier
commits to be agenda-only, and the pre-existing dirty state to be identical.
The local workflow then restarts at Propose for the smaller first slice.
Recursive fallback is capped at three successful frontier replans for one
in-progress slice; reaching the cap restores the baseline and stops as an
ordinary error rather than looping forever.

With live streaming enabled, opsx-build uses Claude Code's documented
[bidirectional `stream-json` transport][claude-streaming]. It keeps the current
Claude process's stdin open until the phase and any interactively queued turns
finish. This allows terminal input to enqueue a slash command or ordinary
follow-up without turning the workflow into an asynchronous marker-file
protocol.

For free-form requests, only Explore and Propose reuse the planning session;
the explicit compact between them is best-effort. Agenda-driven `advance`
starts directly at Propose. Proposal commit, Apply, Verify, Repair, Archive,
and completion work all use disposable fresh sessions, so there is no carried
context to compact at those boundaries.

If a Claude result reports `stop_reason: max_tokens` or a
`max_output_tokens` API failure, opsx-build treats the turn as interrupted
rather than failed. It best-effort compacts that same Claude session and asks it
to continue the current phase from durable repository state. This recovery is
bounded by `max_output_retries` (default 3). Exhausting it in a bounded worker
phase requests the same frontier replan; narrow milestone and archive phases
still fail normally rather than treating protocol trouble as a slice-sizing
decision.

If an Explore, Apply, or Repair turn instead ends normally but omits every
terminal result, opsx-build treats it as an incomplete worker turn. It
best-effort compacts and resumes that same session once, preserving partial
repository work and requiring actual tool use rather than another description
of intended work. Provider/API errors, explicit terminal outcomes, and narrow
milestone stages do not use this recovery.

## Campaign loop

Advance repeatedly through an ordered agenda:

```sh
opsx-build --repo ~/git/my-project --loop advance
```

Because `advance` is the default, this is equivalent to
`opsx-build --repo ~/git/my-project --loop`. The former phrase `next slice` is
accepted as an alias and normalized to `advance`.

Campaigns with an explicit free-form objective still use Explore and let
Claude identify one bounded change per iteration.

Each iteration gets a fresh planning session. Agenda-driven iterations use the
six stages from Propose through completion; free-form campaigns use the normal
seven-stage workflow beginning with Explore. An agenda campaign ends when
every slice is archived. A free-form campaign ends when `propose-unattended`
returns `DONE`. Agenda `TOO_LARGE` outcomes are subdivided and retried; either
kind of campaign stops on `BLOCKED`, interruption, or an unrecoverable ordinary
error. There is no additional whole-workflow retry policy around the existing
stages.

An optional cumulative circuit breaker pauses the campaign cleanly after a
fixed total number of completed changes:

```sh
opsx-build --loop --max-iterations 20 "finish the compiler"
```

The completed checkpoint and campaign history are preserved. If work remains,
opsx-build explains that the ceiling is cumulative and prints a concrete
resume command with a higher total, for example:

```sh
opsx-build --repo ~/git/my-project --resume --loop --max-iterations 40
```

Campaign metadata is stored with the normal checkpoint: current iteration,
completed change names and milestone commits, available elapsed iteration
times, and the optional limit. `--resume` continues the interrupted inner
workflow and then continues its campaign. A crash after a completion commit but
before the next Explore is reconciled without repeating the completed change.

In the TTY dashboard, press `q` to finish the current change and then pause the
campaign cleanly. Ctrl-C still interrupts the active phase immediately and
leaves it resumable.

Press `p` to pause immediately during any streamed Claude phase. opsx-build
interrupts the complete Claude process group, restores the terminal, preserves
the current phase checkpoint and partial repository work, and exits
successfully with the exact `--resume` command. This is the preferred way to
stop work before suspending or repurposing the machine; unlike `q`, it does not
wait for the current OpenSpec change to finish.

If `loop = true` is configured as a default, `--no-loop` runs one change while
preserving the rest of that configuration. On `--resume`, it finishes the
current campaign change and pauses before the following iteration.

## Installation

```sh
cargo install --path . --force
```

## Basic usage

```sh
opsx-build --repo ~/git/my-project "add function pointer support"
```

Test one named Claude connection without starting an OpenSpec build:

```sh
opsx-build --test-connection openrouter-kimi
opsx-build --test-connection openrouter-gemini --verbose
```

This sends a minimal no-tools prompt through the profile's configured command,
environment, and model, verifies Claude's machine-readable result, and exits.
It requires neither Git nor OpenSpec; `--repo` merely selects an existing
working directory for the subprocess. `--dry-run` resolves the profile and
prints its redacted command without contacting the model.

The probe distinguishes transport from usable response compatibility. A model
turn may reach the provider and complete successfully yet emit no visible text,
for example after producing only hidden thinking tokens. That confirms the
route but still fails the command because the model did not return the required
marker. A non-marker response likewise fails with a compatibility diagnostic.

Advance one item from an ordered slice agenda (both forms are equivalent):

```sh
opsx-build --repo ~/git/my-project
opsx-build --repo ~/git/my-project advance
```

Install or refresh the bundled unattended skills without starting Claude or a
workflow:

```sh
opsx-build --repo ~/git/my-project --update-skills
```

This is useful when the skill files are committed: review and commit the
result as an explicit workflow-policy update. Preview it without writing with
`--update-skills --dry-run`. Normal workflows continue to synchronize the
bundled copies at startup for compatibility.

Resume after interruption or an ordinary command error:

```sh
opsx-build --repo ~/git/my-project --resume
```

Inspect what resume would do without changing the checkpoint or repository:

```sh
opsx-build --repo ~/git/my-project --resume --dry-run
```

## Continuing an existing OpenSpec change

When Explore and Propose were performed outside opsx-build, continue the sole
active OpenSpec change with:

```sh
opsx-build --repo ~/git/my-project --continue-existing
```

If several changes are active, select one explicitly:

```sh
opsx-build --repo ~/git/my-project --continue-existing \
  --change fix-test-infrastructure
```

This does not rerun Explore or Propose. It deliberately enters at the proposal
commit milestone: Claude commits uncommitted planning artifacts if necessary,
or treats that step as complete when they are already committed. Apply then
continues any remaining OpenSpec tasks, followed by Verify, Archive, and the
completion commit. Each stage inspects durable repository/OpenSpec state, so a
separate model call does not have to guess which stages are safe to skip.

`--continue-existing --dry-run` shows the selected change and entry stage
without writing a checkpoint. An unfinished opsx-build checkpoint must still be
resumed or forgotten before adopting another change; a completed checkpoint is
replaced by the adopted run.

## Directing the next iteration

Supply durable, one-shot implementation guidance while resuming:

```sh
opsx-build --repo ~/git/my-project --resume \
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

Discard only opsx-build's local workflow pointer with:

```sh
opsx-build --repo ~/git/my-project --forget
```

This does not inspect, restore, reset, stash, delete, or otherwise modify
repository files or Git history. A dry run is also available:

```sh
opsx-build --repo ~/git/my-project --forget --dry-run
```

Checkpoints are stored at `.git/opsx-build/last-run.json`. Schema-2 checkpoints
from the original implementation are migrated in memory and written in the new
schema when a real resume proceeds. A checkpoint at the old
`.git/ospx-build/last-run.json` location remains readable and migrates to the
canonical location on the next write.

## Git behavior

Normal workflow stages never reset, clean, stash, or infer ownership of the
working tree. Frontier escalation is an explicit transactional exception: at
the start of a slice, opsx-build records HEAD, index/worktree state, and
Git-visible untracked files without changing them. If the local worker
escalates, its commits are anchored under `refs/opsx-build/recovery/`, its
status and binary diff are retained under the repository's private Git
metadata, and the recorded state is restored before frontier planning.

This rollback is intentionally not a clean-tree requirement. A slice may begin
with user edits, and the rollback test requires those staged, unstaged, and
untracked edits to return exactly. Ignored files are outside Git's visible
baseline and are not copied or cleaned.

At proposal and completion milestones, Claude is instructed to inspect status,
history, and diffs; commit only work belonging to the OpenSpec change; preserve
unrelated work; and never reset, stash, restore, discard, amend, or rewrite
history. If the relevant work is already committed, Claude may report success
without manufacturing an empty commit.

`BLOCKED` has one meaning: Claude explicitly reported that progress requires a
human decision or unavailable external input. Subprocess failures, malformed
terminal results, missing prerequisites, and rejected frontier postconditions
are normal errors. Their checkpoint remains at the unfinished stage for
resume.

## Claude launcher and configuration

The default launcher is `claude`. oMLX Claude mode can be configured with:

```toml
# ~/.config/opsx-build/config.toml
max_verify_retries = 3
max_output_retries = 3
local_worker_timeout_minutes = 60
# Optional campaign defaults:
# loop = true
# max_iterations = 20
permission_mode = "auto"
claude_command = "omlx launch claude"
claude_model = "qwen3.6-35b-a3b"
# Frontier planning defaults to plain `claude` and that harness's default model.
frontier_command = "claude"
# frontier_model = "opus"
auto_compact_window = "128k"
# Alternatively, compact at a percentage of the launcher's effective capacity.
# auto_compact_percent = 50
max_output_tokens = "8k"
# Optional: reveal Claude's normal assistant/tool activity in the dashboard.
# stream_claude = "activity"
```

The flat launcher fields remain supported for existing configurations. Named
Claude connection profiles are recommended when worker and frontier use
different models, endpoints, or credentials:

```toml
worker_connection = "local"
frontier_connection = "openrouter-kimi"

[connections.local]
command = "omlx launch claude"
model = "qwen3.6-35b-a3b"
context_window = "256k"
auto_compact_percent = 75
max_output_tokens = "8k"

[environments.openrouter.env]
ANTHROPIC_BASE_URL = "https://openrouter.ai/api"
ANTHROPIC_AUTH_TOKEN = { from_env = "OPENROUTER_API_KEY" }
ANTHROPIC_API_KEY = ""

[connections.openrouter-kimi]
command = "claude"
model = "moonshotai/kimi-k3"
environment = "openrouter"

[connections.openrouter-gemini]
command = "claude"
model = "provider-specific-gemini-model-id"
environment = "openrouter"
```

Select a configured connection for one invocation with:

```sh
opsx-build --worker-connection local --frontier-connection openrouter-kimi \
  --loop advance
```

`--claude-command`, `--claude-model`, and the worker token-policy flags override
the selected worker profile. `--frontier-command` and `--frontier-model`
override the selected frontier profile. A profile may also set its own
`context_window`, `auto_compact_window`, `auto_compact_percent`, and
`max_output_tokens`; frontier profiles therefore do not accidentally inherit
worker policy. `context_window` is intentionally profile-only because it
describes the selected model rather than a run-wide policy.

Reusable `[environments.NAME]` profiles hold endpoint, authentication, and
other provider environment. A connection selects one with
`environment = "NAME"`; any inline `[connections.NAME.env]` values override the
shared environment for that model only. Both shared and inline `env` values may
be literal strings or `{ from_env = "NAME" }` references. References keep
credentials out of the TOML file, are resolved only when a connection using
that environment is selected, and are redacted from displayed commands and
debug output. Literal variables with names containing `TOKEN`, `KEY`, `SECRET`,
or `PASSWORD` are also redacted, but storing credentials literally is not
recommended.

Named connections are isolated by default: inherited Claude endpoint,
authentication, and provider-selection variables are removed before the
shared and inline environment is applied. `isolate` and `unset_env` may be set
on the shared environment and overridden or extended by a connection. Add
`isolate = false` only when a profile intentionally depends on ambient Claude
connection variables. `unset_env` removes additional named variables:

```toml
[environments.local-network]
unset_env = ["HTTP_PROXY"]

[connections.slow-local]
command = "omlx launch claude"
model = "larger-model"
environment = "local-network"
```

A hosted connection must still be usable by Claude Code. Direct endpoints or
gateways should implement the Anthropic Messages interface expected by
[Claude Code's LLM gateway support][claude-gateway]; opsx-build does not adapt
an OpenAI-only chat endpoint into Claude's agent protocol.

Set the same limits for one invocation with:

```sh
opsx-build --auto-compact-window 128k --max-output-tokens 8k \
  "add function pointer support"
```

Output-limit recovery can likewise be adjusted for one run:

```sh
opsx-build --max-output-retries 5 "add function pointer support"
```

Select another frontier harness/model or worker-stage timeout with:

```sh
opsx-build --frontier-command claude --frontier-model opus \
  --local-worker-timeout-minutes 45 --loop advance
```

The frontier launcher deliberately does not inherit the worker launcher's
model or token policy. With no frontier profile or model override, plain
`claude` uses the harness default. Frontier replanning provides the
configured command with repository planning context and permission to edit and
commit `automation/slices/`; configure it only to a destination permitted to
receive that repository content.

Token counts may be plain integers (`8192`) or use binary `k`/`m` suffixes;
`8k` therefore means 8,192 tokens. Set `context_window = "256k"` when Claude
Code cannot identify a local, gateway, or provider-specific model's real
capacity. opsx-build exports it as `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, which tells
Claude Code what model window to assume and makes `/context` and proactive
compaction use that capacity where Claude Code supports custom model IDs.

`auto_compact_window` is an optional effective capacity used only for
auto-compaction calculations. `auto_compact_percent` accepts an integer from 1
to 100 and applies to that effective auto-compact window, or to the model's
declared/inferred context window when no separate auto-compact window is set.
For example, `context_window = "256k"` with `auto_compact_percent = 75` targets
approximately 192k tokens. The configured values are exported to every Claude
subprocess selected by that connection, including interactive worker-launcher
tests, through `CLAUDE_CODE_MAX_CONTEXT_TOKENS`,
`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`, and
`CLAUDE_CODE_MAX_OUTPUT_TOKENS`, without changing the parent shell. Leaving a
setting absent preserves Claude's or the launcher's default. See Claude Code's
[environment-variable contract][claude-env-vars] for custom-model caveats.

A lower output limit bounds slow decode latency but can cause more continuation
turns; output-limit recovery handles those turns.

Configuration precedence is:

```text
command-line or OPSX_BUILD_* field override
  > selected named connection profile
  > legacy flat config field
  > built-in defaults
```

Use `--config PATH` to select another file or `--no-config` to disable config
loading. Unknown TOML keys are errors.

For rename compatibility, if the canonical config is absent,
`~/.config/ospx-build/config.toml` is still loaded. Legacy `OSPX_BUILD_*`
environment variables are also accepted below their `OPSX_BUILD_*`
counterparts in precedence. New configuration should use the corrected
spelling.

Explore and Propose default to the two bundled skills. Apply, Verify, and
Archive command names are discovered from common Claude skill and command
locations. Override any command when needed:

```sh
opsx-build \
  --apply-command /opsx:apply \
  --verify-command /opsx:verify \
  --archive-command /opsx:archive \
  "add function pointer support"
```

The corresponding environment variables are:

- `OPSX_BUILD_MAX_VERIFY_RETRIES`
- `OPSX_BUILD_MAX_OUTPUT_RETRIES`
- `OPSX_BUILD_LOCAL_WORKER_TIMEOUT_MINUTES`
- `OPSX_BUILD_LOOP`
- `OPSX_BUILD_MAX_ITERATIONS`
- `OPSX_BUILD_PERMISSION_MODE`
- `OPSX_BUILD_CLAUDE_COMMAND`
- `OPSX_BUILD_CLAUDE_MODEL`
- `OPSX_BUILD_WORKER_CONNECTION`
- `OPSX_BUILD_FRONTIER_COMMAND`
- `OPSX_BUILD_FRONTIER_MODEL`
- `OPSX_BUILD_FRONTIER_CONNECTION`
- `OPSX_BUILD_AUTO_COMPACT_WINDOW`
- `OPSX_BUILD_AUTO_COMPACT_PERCENT`
- `OPSX_BUILD_MAX_OUTPUT_TOKENS`
- `OPSX_BUILD_EXPLORE_COMMAND`
- `OPSX_BUILD_PROPOSE_COMMAND`
- `OPSX_BUILD_APPLY_COMMAND`
- `OPSX_BUILD_VERIFY_COMMAND`
- `OPSX_BUILD_ARCHIVE_COMMAND`
- `OPSX_BUILD_STREAM_CLAUDE`

## Claude launcher compatibility

`opsx-build` does not call an oMLX API. It starts the configured launcher as a
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
- `--model` when the selected connection configures a model;
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

For JSON output, opsx-build reads `is_error`, `result`, `session_id`, and
`structured_output`. For streaming output it finds the most recent JSONL result
carrying a valid OpenSpec stage status; later slash-command results such as
`/compact` do not replace the stage result. A launcher may write diagnostics to
stderr, but should not mix banners or unrelated prose into non-streaming JSON
stdout.

`--json-schema` is optional for compatibility: if the launcher clearly rejects
that option before execution, opsx-build retries once using the documented
`OPSX_STATUS` marker protocol. It similarly degrades from streaming JSON to
ordinary JSON or text when an older Claude CLI clearly rejects `--input-format`
or `--output-format`. It never retries merely because a successful, potentially
mutating invocation returned malformed terminal data.

### oMLX-specific behavior

The only oMLX-specific configuration is the launcher prefix and model name.
The oMLX server must already be listening, normally on its default local port;
opsx-build does not start, stop, configure, or query that server. `omlx launch
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
opsx-build --stream-claude "add function pointer support"
```

Each workflow phase has its own disclosure, headed by status, stage position,
phase name, retry/pass number when applicable, live/final elapsed time, and
captured line count. The current phase starts expanded; completed phases
collapse but remain available with their elapsed time frozen:

```text
opsx-build  change: add-function-pointers  /path/to/repository
[4/7] Apply · 14m 07s  ⠴ Claude is applying the OpenSpec change
──────────────────────────────────────────────────────
  ▶ ✓ [1/7] Explore · 3m 12s · 48 lines
  ▶ ✓ [2/7] Propose · 8m 41s · 31 lines
  ▶ ✓ [3/7] Proposal commit · 22s · 12 lines
› ▼ ⠴ [4/7] Apply · 14m 07s · 137 lines
```

The change name appears as soon as proposal discovery records it in the
opsx-build checkpoint. Resumed and `--continue-existing` runs show it from the
start.

When content exceeds the terminal height, a scrollbar appears on the right.
Its arrow buttons move one line, its track is clickable, and its thumb can be
dragged. Mouse-wheel and keyboard scrolling continue to work; reaching the
bottom resumes tail-following.

An individual Claude message is limited to 200 displayed lines in the
dashboard. Longer messages retain their first 120 and final 80 lines with an
omission marker showing the number of hidden lines. This affects presentation
only; subprocess capture and workflow result parsing retain the complete
message.

Campaign mode adds the iteration number to the header and retains a compact
summary row for every completed change. The active iteration continues to use
the existing phase disclosures; starting the next iteration clears the old
phase chatter while preserving its summary.

Controls:

- `p`, followed by `y`, immediately stops the active Claude subprocess and
  exits successfully, preserving the current phase checkpoint for `--resume`;
- `f` during Explore, Propose, Apply, Verify, or Repair stops the local worker
  and requests frontier subdivision at the next orchestration opportunity
  after confirmation;
- `c` sends Claude's interrupt control request—the streaming equivalent of
  Escape—then runs `/compact` and reissues the interrupted stage command (once
  per invocation) after confirmation;
- `C` follows the same interrupt/resume cycle but runs `/context`, leaving
  Claude's context report in the phase disclosure for debugging, after
  confirmation;
- `i` opens a steering prompt; Enter interrupts the active Claude turn and
  delivers the entered instruction as its continuation;
- `q` in campaign mode pauses cleanly after the current change completes after
  confirmation;
- Escape cancels the injection prompt;
- Tab or Left/Right selects a phase disclosure;
- Enter, Space, `o`, or a click on a heading expands/collapses that phase;
- Up/Down and Page Up/Page Down scroll expanded output;
- End returns to the newest output;
- Escape collapses the selected phase;
- Ctrl-C interrupts the current subprocess while preserving its opsx-build
  checkpoint, but reports an interruption rather than a deliberate pause.

The single-key actions `p`, `q`, `f`, `c`, and `C` require a following `y` to
confirm; any other key cancels the pending action and is not interpreted as a
new shortcut. Steering is already a deliberate two-step action: Escape cancels
its input prompt and Enter sends the entered text. Ctrl-C remains an immediate
emergency stop.

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
launcher rejects interrupt controls, opsx-build degrades to
queuing the requested command after the current turn. These controls are
available in the TTY dashboard; linear non-TTY streaming remains output-only.

Repeated Verify and Repair phases are retained separately as `pass 2`,
`pass 3`, and so on. The dashboard restores the previous terminal screen when
the workflow completes or stops.
When either stdin or stderr is not a TTY—for example under CI, redirection, or
a pipe—opsx-build uses ordinary linear output and does not write cursor-control
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
[claude-gateway]: https://code.claude.com/docs/en/llm-gateway

## Interactive launcher testing

Open an ordinary interactive Claude session using the configured launcher,
model, and permission mode:

```sh
opsx-build --interactive -- --effort xhigh
```

An optional positional argument becomes the initial prompt. Arguments after
`--` pass directly to Claude. Interactive mode does not read or update the
OpenSpec workflow checkpoint.

## Terminal protocol

Each unattended stage asks Claude Code for schema-validated structured output.
The result contains an `opsx_status` and a concise `summary`; Claude Code can
re-prompt the model when its first result does not satisfy the schema.

For older or compatibility-layer Claude CLIs that reject `--json-schema`,
opsx-build falls back to final-line markers:

```text
OPSX_STATUS: READY
OPSX_STATUS: DONE
OPSX_STATUS: TOO_LARGE
OPSX_STATUS: VERIFIED
OPSX_STATUS: RETRY
OPSX_STATUS: BLOCKED
OPSX_STATUS: REPLANNED
```

Explore, Apply, and Repair use `READY`, `TOO_LARGE`, or `BLOCKED`. Propose also
accepts `DONE`. Archive and commit stages use `READY` or `BLOCKED`; Verify uses
`VERIFIED`, `RETRY`, or `BLOCKED`. `DONE` means no OpenSpec change was created
or modified because the requested objective is already satisfied; in campaign
mode it terminates the outer loop successfully. `TOO_LARGE` means the assigned
worker slice needs decomposition before another local attempt. The frontier
planner uses `REPLANNED` only after committing a valid agenda subdivision.

The corrected structured field and fallback marker are `opsx_status` and
`OPSX_STATUS`. Results using the old `ospx_status` or `OSPX_STATUS` spellings
remain readable for compatibility.

If the Claude process exits successfully but returns neither structured output
nor a fallback marker, opsx-build does not rerun that potentially mutating
stage. Archive additionally checks durable OpenSpec state so a completed
archive is not repeated merely because its acknowledgement was malformed.

## Prerequisites and limitations

- The configured Claude launcher, `claude`, `openspec`, and `git` must be on
  `PATH`.
- The repository must contain `openspec/config.yaml`. The unattended Explore
  and Propose skills are installed into the target repository automatically.
- Apply, Verify, and Archive workflow names must be discoverable or configured.
- Change discovery requires one new change, one uniquely modified change, or
  only one active change.
- The process is synchronous; campaign mode repeats complete runs in the same
  foreground process. There is no daemon or web UI.
- Automatic frontier subdivision currently requires an ordered
  `automation/slices/` assignment. Free-form work can report `TOO_LARGE`, but
  cannot yet be subdivided automatically.
- Claude remains responsible for normal proposal/completion commit scope. The
  runner applies deterministic path and state checks only to the special
  frontier agenda commit.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build
```
