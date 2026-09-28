# Bootstrap implementation agenda from supplied contracts

Change name: `bootstrap-implementation-slices`

This is planning-only work. Create an ordered implementation agenda for the
complete in-scope goal. Do not implement product functionality.

{{OPSX_BUILD_WORKER_CAPACITY}}

{{OPSX_BUILD_PROJECT_AUTHORITY}}

Read the supplied context and declared required contracts within their reading
boundaries. The supplied-contract workflow assigns existing requirements to
work; it does not author replacement behavioural specifications or expected
results. Keep conditions, exceptions and component ownership with the original
source. Internal implementation decisions remain the campaign's responsibility.

During Propose, create only this change's proposal, design and tasks using
schema `opsx-supplied-contracts`. Map supplied requirements to the planning work
with source links. Do not create spec deltas or the agenda files yet.

During Apply, materialize the agenda under `automation/slices/`:

- A README lists slices in order and maps every material in-scope requirement
  to delivery slices and the terminal gate, using links to original inputs.
- At least one delivery slice precedes `9999-project-acceptance.md`.
- Each slice uses `<four-digit ordinal>-<kebab-case slug>.md` and starts with
  a level-one title. Include `## Objective`, `## Prerequisites`,
  `## Supplied requirements`, `## Acceptance Criteria`, and `## Required Tests`.
- `## Supplied requirements` in each slice and the README contains Markdown
  links to declared original contracts, with heading anchors or exact IDs where
  available. Paths are relative to that document or absolute. Beside each link
  identify the work/slices that cover it; do not restate the requirement.
- Acceptance criteria reference supplied expectations and scenarios. Required
  tests reference those scenarios and describe implementation-specific checks
  without replacing their expected outcomes. Link supplied acceptance files.
- Each slice delivers coherent, testable owned work, lists its prerequisites,
  and leaves the repository buildable. Several implementation steps can be
  tasks within one slice; do not subdivide indefinitely.
- Prove uncertain dependencies with a small end-to-end use before building
  later slices on them; record a fallback in the implementation design.
- The terminal `9999` slice also contains `## Project Goal Coverage` and covers
  all supplied in-scope requirements, including integration and required checks.
  It cannot define different acceptance expectations.

Use the existing Verify pass to check agenda structure, ownership, source
references, conditions, exceptions and complete goal coverage. Return RETRY for
correctable derived-plan mistakes. Repair bootstrap artifacts and agenda
together; never change supplied contracts or acceptance expectations. Use
BLOCKED only for authoritative-input gaps requiring an external decision.

Archive proposal, design and tasks without specification synchronization.
