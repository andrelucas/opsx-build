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
<!-- END OPSX-BUILD MANAGED -->
