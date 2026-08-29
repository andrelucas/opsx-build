<!-- BEGIN OPSX-BUILD MANAGED -->
## opsx-build workflow

- Treat OpenSpec artifacts, Git history, and `automation/slices/` as durable
  workflow state; do not rely on conversation history alone.
- For an assigned agenda slice, use its objective, prerequisites, acceptance
  criteria, and tests as the planning authority. Do not substitute a different
  slice or broaden it into adjacent agenda work.
- Run OpenSpec and project commands synchronously. Do not background, detach, or
  return while a command or delegated task is still running.
- Return `TOO_LARGE` when an assigned slice cannot reliably fit one bounded
  worker-model change. Include concrete evidence and an ordered decomposition;
  do not struggle onward by broadening the implementation.
- Reserve `BLOCKED` for a genuine external decision or unavailable dependency,
  not ordinary engineering failures, uncertainty between reasonable technical
  choices, or correctable test failures.
- Preserve unrelated working-tree changes. Never reset, stash, restore,
  discard, amend, or rewrite user work merely to simplify a workflow stage.

{{OPSX_BUILD_MODEL_CONFUSIONS}}

### Dependency selection

- Treat third-party library and framework choices as revisable design decisions
  unless the project context or specification explicitly requires them. The
  specified observable behaviour is authoritative.
- Validate a material dependency with the smallest end-to-end use before
  building substantial work around it. If its public API does not naturally
  support the required behaviour, reassess and replace it when that is simpler.
  Do not distort specified behaviour or accumulate workarounds merely to
  preserve an earlier dependency choice.

### Language-server use

- At the start of code-oriented work, verify that a language server for the
  project's primary implementation language is actually working by making a
  real symbol, definition, reference, hover, or diagnostic request. Plugin
  presence alone is not proof that the integration works.
- Strongly prefer language-server facilities for understanding code structure,
  navigating symbols and references, and obtaining diagnostics. Use targeted
  text search when it is intrinsically more appropriate, or as a fallback when
  the language server is unavailable.
- If the language server is unavailable, report that once and continue with
  targeted repository search when the task remains safe. Do not repeatedly
  retry it or claim that it worked when it did not.

### Source formatting

- Use each source language's canonical formatter as a normal part of completing
  code changes. Format every source file changed by the current task before
  final validation, preferring the project's documented formatting command.
- Keep formatting scoped to task-owned files when unrelated work is present.
  Inspect the resulting diff and do not reformat generated, vendored,
  third-party, or unrelated source merely for consistency.
- Report a missing formatter or unexpected formatter result clearly; do not
  silently skip formatting or claim it succeeded without running it.

### Test execution and Claude's sandbox

- When Claude Code's sandbox prevents a required project build or test from
  running, rerun only the affected command outside the sandbox with the Bash
  tool's `dangerouslyDisableSandbox: true` option. Treat sandbox denial as an
  execution-environment limitation, not a product defect.
- Do not rewrite production code or tests to accommodate Claude's sandbox, add
  sandbox-specific behavior, substitute synthetic or weaker coverage, skip the
  gate, or mark it complete without a real execution. Verification succeeds
  only when the required real checks have run and passed.
- Use the sandbox escape for the minimum required project build or test command,
  not unrelated activity.
<!-- END OPSX-BUILD MANAGED -->
