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

- have a four-digit ordinal and kebab-case filename, such as
  `0001-project-scaffold.md`;
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

A slice is too large when it requires several independently testable
behaviours, coordinated novelty across several subsystems, substantial
architectural discovery during implementation, or has an obvious independently
testable intermediate state. When in doubt, split it. Prefer several small
sequential slices over one ambitious slice.

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

Mark this planning change with `skip_specs: true`. Build its normal OpenSpec
proposal, design, and tasks artifacts, apply those tasks by writing the agenda,
verify the agenda against this brief and `openspec/config.yaml`, and do not
modify product code.
