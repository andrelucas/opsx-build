# Bootstrap implementation agenda

Change name: `bootstrap-implementation-slices`

Create the complete implementation agenda that opsx-build will subsequently
execute with a smaller worker model. This is a planning-only change. It MUST
NOT implement product functionality.

Treat `openspec/config.yaml` as the authoritative project goal and planning
context. Do not survey or attempt to understand the product source tree. Read
only the configuration and project documentation needed to resolve a genuine
planning ambiguity.

The deliverable is an ordered set of independently executable slices under
`automation/slices/`, plus `automation/slices/README.md` as its index and goal
coverage map. Do not create OpenSpec changes for the implementation slices now.

Each implementation slice must:

- use the exact filename form `<four-digit ordinal>-<kebab-case slug>.md`, such
  as `0001-project-scaffold.md`;
- begin with a level-one Markdown title (`# ...`) describing the slice;
- contain `## Objective`, `## Prerequisites`, `## Acceptance Criteria`, and
  `## Required Tests` sections;
- introduce one new observable capability or one bounded extension;
- be implementable in one OpenSpec propose/apply/verify/archive cycle;
- state its dependencies on earlier slices;
- define observable acceptance criteria and the tests that prove them;
- include enough durable context for a worker without the planner's
  conversation history;
- leave the repository buildable and its tests passing;
- be small enough for a substantially less capable local model to reason about
  and verify reliably.

The agenda must contain at least one delivery slice in addition to
`9999-project-acceptance.md`.

A slice is too large when it requires several independently testable
behaviours, coordinated novelty across several subsystems, substantial
architectural discovery during implementation, or has an obvious independently
testable intermediate state. When in doubt, split it. Prefer several small
sequential slices over one ambitious slice.

Treat important third-party dependency choices as hypotheses rather than
requirements unless `openspec/config.yaml` explicitly mandates them. When later
work depends on a library, framework, protocol implementation, or external API
whose suitability is not already established, make the earliest relevant slice
prove the dependency with the smallest end-to-end vertical use. Give that slice
an observable compatibility gate and record a fallback. Do not build several
slices on an unproven dependency or distort the project requirements around a
library selected during planning.

The agenda must cover the complete concrete project goal in
`openspec/config.yaml`, not merely an initial milestone. Do not leave essential
capabilities to unspecified "future work". If the supplied goal is genuinely
open-ended or lacks a testable completion boundary, report BLOCKED rather than
silently inventing a partial endpoint.

The final slice MUST be named `9999-project-acceptance.md`. It is the terminal
whole-project gate and must additionally contain `## Project Goal Coverage`.
Its acceptance criteria and required tests must prove the project goal as a
whole, including integration across earlier slices. When that slice is archived,
every stated project requirement must be delivered and the agenda is DONE.

The README must list the slices in execution order, explain how their sequence
reaches the complete goal, and map every material goal requirement to one or
more slices, including the final acceptance gate.

## Stage ownership

During OpenSpec Propose, create or update only the normal OpenSpec artifacts for
the `bootstrap-implementation-slices` change. Do not create or modify
`automation/slices/README.md` or any implementation slice under
`automation/slices/` during Propose. Describe the intended agenda structure,
slice files, acceptance criteria, and required tests in the OpenSpec design and
tasks so a fresh Apply session can carry them out without conversation history.

During OpenSpec Apply, materialize that approved plan as
`automation/slices/README.md` and the ordered implementation slice files. Apply
exclusively owns creation of those agenda deliverables.

Before reporting Apply complete, inspect the finished agenda and correct every
structural omission: the README must exist; at least one delivery slice and the
`9999` final gate must exist; every slice filename, level-one title, and required
section must match this contract; and the final gate must contain
`## Project Goal Coverage`.

Mark this planning change with `skip_specs: true`. Build its normal OpenSpec
proposal, design, and tasks artifacts, apply those tasks by writing the agenda,
verify the agenda against this brief and `openspec/config.yaml`, and do not
modify product code.
