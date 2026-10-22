# Assign supplied obligations to implementation work

Change name: `bootstrap-implementation-slices`

This change is planning-only. Do not implement product functionality or create
OpenSpec changes for the future delivery slices.
This workflow does not author replacement behavioural specifications or spec deltas.

{{OPSX_BUILD_WORKER_CAPACITY}}

{{OPSX_BUILD_PROJECT_AUTHORITY}}

Read the protected obligation inventory, original contracts and acceptance
inputs within their supplied reading and ownership boundaries. Original sources
and their declared precedence govern meaning. The inventory identifies selected
obligations, dependency owners, context-only material, supplied tests and tests
the builder must add. A genuine contradiction or ambiguity in authoritative
inputs requires an external decision; generated planning errors are yours to fix.

During Propose, create proposal, design and tasks with the bundled
`opsx-supplied-contracts` schema, plus `assignments.json` in this bootstrap change.
The runner supplies its JSON format. Assign every non-context obligation and
every acceptance obligation, with one accountable slice and any contributions.
Dependency assignments mean consuming the named owner's contract, not building
that owner's implementation. Make cross-cutting contributions explicit.
Prerequisites must be executable in numeric slice order.
Every link in `## Supplied requirements` must target a declared supplied input
directly; generated records, proposals and coverage views belong under Planning
references. The renderer creates the agenda's source-reference sections.

Use proposal for purpose and direct original-source references, design for
decomposition decisions and risks, and tasks for materializing and checking the
agenda. Reference the assignment record under Planning references; do not copy
its coverage map into those documents. Do not create agenda files during Propose.

During Apply, move the assignment to `automation/slices/assignments.json` and
write the slice briefs there, plus `9999-project-acceptance.md`. Each slice has
a level-one title and Objective, Prerequisites, Supplied requirements,
Acceptance Criteria and Required Tests level-two headings. Delivery filenames
start with a four-digit ordinal and use a kebab-case slug. The terminal also has
Project Goal Coverage. Choose a few coherent delivery slices, keeping internal
steps as tasks. A small component may need one delivery slice and acceptance.

Describe objective, owned scope, completion boundary and acceptance route by
reference. Assign complete source conditions and exceptions without translating
them into another behavioural checklist. Split obligations explicitly where
needed. A source link does not excuse lost scope or unassigned required tests.
Leave detailed algorithms and test mechanisms to implementation, except for
source-mandated mechanisms and decisions necessary to establish decomposition.
Assign an early dependency proof if uncertainty affects later ordering.

Run `opsx-build render-agenda` after authoring or repairing the assignment. It
generates the README coverage view, slice source references and prerequisites,
and terminal checks from the record and the runner's pinned commands. Do not
hand-copy commands or repeat case counts. Keep author-written completion and
test-route prose concise. Remove contradictory optional recipes rather than
replacing them with more prescriptions. Product code and protected inputs stay
unchanged. Leave the change active; the runner owns Archive and milestone commits.

Verify must independently read the sources and review actual assignment
consequences. Reject omitted obligations, incorrect scope splits, ownership
transfers, invented or inverted conditions, unworkable dependencies and weakened
acceptance. Each blocker identifies source, defective assignment or prescription,
and consequence. Complete assignment by reference requires no repeated prose
condition; missing repetition alone is not missing scope. Harmless descriptive
counts are observations when complete scope and acceptance remain assigned.
Structural and source-reference checks remain mandatory. Verify reads and checks;
it never renders or repairs. Real delivery tests and independent final acceptance
remain required; a correct agenda does not establish implementation correctness.

Archive proposal, design and tasks without synchronizing specifications.
