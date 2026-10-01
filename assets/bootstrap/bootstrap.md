# Bootstrap implementation agenda

Change name: `bootstrap-implementation-slices`

Create the complete implementation agenda that opsx-build will subsequently
execute with the configured worker model. This is a planning-only change. It MUST
NOT implement product functionality.

{{OPSX_BUILD_WORKER_CAPACITY}}

{{OPSX_BUILD_PROJECT_AUTHORITY}}

Read the declared required inputs before decomposing the goal. Use targeted
inspection within the supplied reading boundaries; do not survey or attempt to
understand the entire product source tree.

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
- be bounded enough for the configured worker to implement and verify reliably.

The agenda must contain at least one delivery slice in addition to
`9999-project-acceptance.md`.

Choose coherent delivery boundaries with practical implementation and
verification plans. Several files, subsystems, test cases, or independently
testable intermediate steps can be tasks within one slice. Split when a
concrete capacity, scope, or prerequisite constraint prevents completing and
verifying the work together; the mere ability to identify smaller steps is
not a reason to give each its own OpenSpec cycle.

Treat important third-party dependency choices as hypotheses rather than
requirements unless the supplied context or contracts explicitly mandate them. When later
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
more slices, including the final acceptance gate. Use a concise coverage map;
do not invent a requirement-label system or duplicate the brief as verbatim quotes.

Keep this ordinary context bootstrap proportionate to the project. Describe
observable outcomes, dependencies and meaningful checks. Leave implementation
mechanics and detailed test design to the implementing worker. Provide focused
guidance where it helps, without prescribing and duplicating a complete algorithm
across the bootstrap design, README and slices.

## Stage ownership

Ordinary mode starts from the project brief; it does not assume that a maintainer
has supplied finished requirements, acceptance scenarios or a technical design.
During Propose, derive the required observable outcomes and completion boundary,
resolve reasonable engineering choices, and record material assumptions. Keep
all normal OpenSpec artifacts and schema-required acceptance scenarios. Use
BLOCKED only for a material product decision that cannot be inferred safely.

During OpenSpec Propose, create or update only the normal OpenSpec artifacts for
the `bootstrap-implementation-slices` change. Do not create or modify
`automation/slices/README.md` or any implementation slice under
`automation/slices/` during Propose. Decide the slice identities, observable
outcomes, ordering and prerequisites. Put one compact goal-to-slice coverage map
in the proposal, including the terminal gate. Use design for decomposition
rationale and unresolved risks, and tasks for authoring and checking the agenda.
Reference that map from dependent artifacts instead of reproducing it. Do not
draft complete future slice documents or duplicate per-slice acceptance and test
catalogues inside these artifacts. Retain the decisions and observable success
conditions needed for Apply to develop the agenda without conversation history.

During OpenSpec Apply, use the chosen decomposition and original inputs to write
`automation/slices/README.md` and the ordered implementation slice files. Develop
their acceptance criteria and required tests here.
Apply exclusively owns creation of those agenda deliverables. Preserve sound
decomposition decisions;
reopen them only when a concrete contradiction, coverage gap or unworkable
dependency requires correction. Keep the proposal map and design at decision
level rather than copying the finished agenda back into them.

This boundary applies only to bootstrap. Each later implementation slice still
uses normal OpenSpec Propose to develop its specifications, design and tasks,
then normal Apply to implement and test them.

Before reporting Apply complete, inspect the finished agenda and correct every
structural omission: the README must exist; at least one delivery slice and the
`9999` final gate must exist; every slice filename, level-one title, and required
section must match this contract; and the final gate must contain
`## Project Goal Coverage`.

Check that the agenda covers the complete in-scope goal and contains no material
contradiction of the brief. Verification applies the ordinary anti-Karen rule:
missing outcomes, unworkable dependencies and unusable acceptance criteria block;
wording, preferred designs and implementation details that the worker can resolve
do not. If an optional implementation recipe is flawed, remove the prescription
while preserving the required outcome and check. Do not spend bootstrap repair
cycles refining it into a complete implementation.

Mark this planning change with `skip_specs: true`. Build its normal OpenSpec
proposal, design, and tasks artifacts, apply those tasks by writing the agenda,
verify the agenda against this brief and `openspec/config.yaml`, and do not
modify product code.
