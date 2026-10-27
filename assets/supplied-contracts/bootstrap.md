# Assign supplied work to implementation slices

This is planning-only. Write `automation/slices/assignments.json` and concise
slice briefs directly. Do not create a bootstrap OpenSpec change or product code.

{{OPSX_BUILD_WORKER_CAPACITY}}

{{OPSX_BUILD_PROJECT_AUTHORITY}}

Start with `context.md` (or the selected project introduction), the protected
inventory and original contracts within their reading and ownership boundaries.
Original sources govern behaviour. Report an authoritative contradiction as
BLOCKED with its sources and the maintainer correction needed. Fix mistakes in
your own assignment rather than passing them to the maintainer.

For a narrow repair to an already runnable seed, normally use one delivery slice.
Dependency checks and baseline startup can be its first ordered tasks where the
supplied instructions allow. Separate slices need useful implementation,
integration or recovery boundaries; several files or test cases are not enough.
A rebuild may need several slices and an early runnable milestone.

Assign every owned and dependency obligation and every acceptance obligation.
Dependencies mean consuming their owner's contract, not implementing that owner.
Preserve conditions, exceptions, required builder tests and all final acceptance.
Use one accountable delivery slice per obligation with explicit contributions
where needed. Prerequisites must run in numeric order. Check that intermediate
tests compile and execute with the APIs and fixtures available at that milestone:
selecting individual tests does not necessarily avoid compiling their package.
Move a full-package gate later or use a separately runnable intermediate check
when necessary. Never hide protected cases or claim pending cases passed.

Create `9999-project-acceptance.md` as the terminal brief. Every brief has a title
and Objective, Prerequisites, Supplied requirements, Acceptance Criteria and
Required Tests headings; the terminal also has Project Goal Coverage. Explain
what the work does and why it matters before linking to detailed coverage.
Preserved behaviour stays assigned without repeated prose enumeration. Required
delivery reports belong to delivery tasks, not a later terminal Apply cycle.

Run the supplied `opsx-build render-agenda` command after writing or repairing the
record. It generates coverage, source references, prerequisites and exact terminal
commands. Run `check-agenda` to check them. Do not hand-copy inventories or commands.
Leave algorithms and test mechanisms to implementation unless needed to decide
decomposition or required by the sources. Do not create a competing behaviour spec.

The runner checks the record mechanically. A meaningful multi-slice split also
gets independent planning review. Missing repeated prose or harmless wording is
not a defect. A one-slice assignment is checked with its implementation.

Leave the agenda uncommitted. The runner owns its completion commit. Final
acceptance is independent review followed by the configured commands, without
another OpenSpec proposal, design, tasks or Apply cycle.
