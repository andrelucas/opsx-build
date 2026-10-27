# Supplied-contract assignment workflow (0.37)

New supplied-contract `bootstrap` and `execute` runs require
`--obligation-inventory PATH`. Standard mode still develops requirements, designs
and tests from a brief and does not use this inventory or assignment format.
Standalone supplied-contract changes can still implement contracts without an
agenda inventory.

The maintainer supplies the selected obligations, ownership and acceptance
expectations. Bootstrap assigns this work; it does not author another description
of the required behaviour. The original contracts and their declared precedence
remain authoritative. Conflicting authoritative inputs require a maintainer
decision. Mistakes introduced in the generated assignment are runner work to
repair.

## Proportional planning

Fresh supplied-contract runs write assignments and concise briefs directly,
without a bootstrap OpenSpec package. Mechanical checks establish declared
coverage and ordering; multi-slice decomposition also gets independent planning
review. A narrow repair normally needs one delivery slice. Where the supplied
policy permits, put dependency checks and baseline startup first in its tasks.
Check that intermediate tests do not compile APIs assigned to later slices.

Delivery changes require a short proposal and tasks, with an optional supporting
design. Preserve the introduction in context.md. Explain each brief's work and
purpose before linking to the inventory. Keep every obligation assigned without
repeated authored inventories. Required delivery reports are delivery tasks.

Terminal review and exact final commands run directly. A single delivery's
complete independent review may be reused after its own archive move; changes to
other files require review. Failed final acceptance returns to repair and
independent review before another execution. Older checkpoints retain their
original schema and bootstrap/terminal OpenSpec stages.

Maintainers can edit files and registered input/gate settings in the stopped
campaign and run `opsx-build accept-changes`, then `opsx-build --resume`. This
adopts the correction without approving the implementation or clearing unrelated
findings. The planner arranges work affected by the correction, including
completed slices. Project-specific manifests must be corrected by their owner.
See the README for invocation details.

## Current delivery and later qualification

Planning and delivery prompts distinguish owned implementation and checks needed
now from qualification that the original inputs explicitly assign after assembly
or outside the component's delivery. Derived tasks must preserve that boundary.
When later qualification is allowed, finish and check the owned harness and its
assertions, correct mistaken derived tasks, and proceed through independent
Verify and the unchanged component gate. Record each unexecuted observation,
missing input, responsible owner and remaining acceptance route as pending.
Missing peers never excuse unfinished owned work or failing required checks.

A `BLOCKED` summary must cite the original source making the input necessary for
the current stage, identify the affected work/check and missing input or decision,
give evidence of its absence, and state the action needed to continue. A generated
task alone cannot justify that dependency. A narrower passing gate does not waive
execution required by the original inputs. Genuine missing current prerequisites
and authoritative ambiguities still stop the run; `BLOCKED` is not automatically
retried. These instructions apply to both backends in supplied-contract mode;
they guide model judgment rather than mechanically prove a correct scope split.

## Changes needed in Sirocco campaign preparation

For each fresh component workspace, the Sirocco maintainer should:

1. Produce a maintainer-owned `obligations.json` from the selected component
   contracts, ownership boundaries and acceptance inventories. Include inherited
   and cross-cutting obligations, dependency consumption, supplied checks and
   builder-required tests/observations. Do not select only existing executable
   fixtures: they may cover only part of the assigned behaviour.
2. Resolve ambiguous ownership during preparation. Mark material from shared
   documents as owned, a consumed dependency (with its provider owner), or context.
   Split a shared section into suitably scoped entries where components own
   different responsibilities. An explicit `scope` records the maintainer's
   boundary; it cannot override higher-precedence source requirements.
3. Keep all original files registered with `--contract` and all supplied tests,
   drivers and fixtures registered with `--acceptance-file`. Inventory source
   references must resolve directly to those declared originals. Do not register
   generated plans as authoritative inputs.
4. Add `--obligation-inventory obligations.json` to the configuration/launch
   arguments. The runner protects the inventory automatically; it need not also
   be listed with `--contract`. Keep the existing exact `--acceptance-command`
   arguments and timeout. The inventory neither selects nor waives commands.
5. Start a fresh campaign with the new runner installed on the execution host.
   Restart the demanding campaigns with these prepared inputs; this change does
   not update any existing workspace or install a binary on another machine.

For example, using real paths and commands chosen by the project:

```sh
opsx-build --supplied-contracts \
  --contract context.md \
  --contract spec/component.md \
  --contract acceptance/observations.md \
  --obligation-inventory obligations.json \
  --acceptance-file acceptance/check.sh \
  --acceptance-command 'sh acceptance/check.sh' \
  --acceptance-timeout-seconds 600 \
  execute --context context.md
```

`configure`/`autoconfigure` can record an existing inventory. They must not create
an authoritative inventory or invent ownership. A missing inventory is a
preparation issue, not permission for bootstrap to infer a closed obligation set.
No Sirocco-specific IDs, semantics or acceptance scenarios are built into the runner.

## Maintainer inventory, version 1

Source paths are relative to the campaign root (absolute paths are also accepted),
with optional heading/requirement/line fragments as in normal supplied references.
IDs are unique across both arrays and use letters, digits, hyphens, underscores
or dots. All fields shown are required except `owner` and `scope`.

```json
{
  "version": 1,
  "component": "example",
  "obligations": [
    {"id": "OWN-01", "source": "spec/component.md#REQ-01", "role": "owned"},
    {"id": "DEP-01", "source": "spec/component.md#DEP-01", "role": "dependency",
     "owner": "clock-provider", "scope": "Consume qualification; the provider owns its policy."},
    {"id": "CONTEXT-01", "source": "spec/component.md#context", "role": "context"}
  ],
  "acceptance": [
    {"id": "SUPPLIED-01", "source": "acceptance/check.sh", "kind": "supplied-test",
     "obligations": ["OWN-01"]},
    {"id": "BUILDER-01", "source": "acceptance/observations.md#required-observations",
     "kind": "builder-test", "obligations": ["OWN-01", "DEP-01"]}
  ]
}
```

`owned` entries belong to `component`. A `dependency` entry must name a different
owner; assigning it means implementing consumption of its public contract. Context
entries are reading material, not assigned implementation. Every owned obligation
must have at least one acceptance obligation. Acceptance may govern several
obligations and obligations may have several acceptance routes.

Evidence kinds are `supplied-test`, `builder-test`, and `review`. A supplied test
must reference a protected acceptance file. Builder tests and required reviews
reference their supplied expectations. Use `review` only where the original
inputs actually call for review evidence; it is not a substitute for required
executable checks. The generated record cannot change these evidence kinds.
The runner validates declarations and references, not whether the maintainer
selected the right obligations or whether a review route is semantically adequate.

The inventory is pinned with the other inputs. Resume cannot select a different
inventory, change its contents or executable bit, or recapture it. Revise inputs
with `accept-changes` as a deliberate maintainer correction, or prepare a new
run. New checkpoints use format 7. Older supported checkpoints keep their
recorded planning workflow.

## Generated assignment, version 1

New bootstrap writes `automation/slices/assignments.json` directly and renders
its briefs. The following draft/move sequence applies to older checkpoints:
during bootstrap Propose the agent writes
`openspec/changes/bootstrap-implementation-slices/assignments.json`. Proposal,
design and tasks reference that draft under Planning references; they retain
direct original links under Supplied requirements. No agenda files are created
yet. During Apply the agent moves the record to
`automation/slices/assignments.json`, authors the concise briefs and renders the
views. The draft should no longer remain in the bootstrap change.

```json
{
  "version": 1,
  "slices": [{"path": "0001-delivery.md", "requires": []}],
  "obligations": [
    {"id": "OWN-01", "accountable": "0001-delivery.md", "contributors": []},
    {"id": "DEP-01", "accountable": "0001-delivery.md", "contributors": []}
  ],
  "acceptance": [
    {"id": "SUPPLIED-01", "slices": ["9999-project-acceptance.md"],
     "route": "Run the pinned protected check on the complete component."},
    {"id": "BUILDER-01", "slices": ["0001-delivery.md"],
     "route": "Implement and run real assertions for all supplied observations."}
  ]
}
```

Every owned/dependency obligation has exactly one accountable delivery slice and
may have contributors. This allows coherent slices and cross-cutting work without
requiring a whole shared requirement to fit inside one slice. Semantic review
checks the allocation's completeness. All acceptance obligations have nonempty
routes; builder tests must be delivered before terminal acceptance.

The terminal filename `9999-project-acceptance.md` is implicit, excluded from the
`slices` array, and required on disk. All source obligations and acceptance work
are included in its generated coverage view. Each delivery filename begins with
a four-digit ordinal; recovery may append numeric children such as
`0001.1-follow-up.md`. Prerequisites name earlier delivery filenames. Unknown IDs,
duplicate assignments, lost work, cycles/forward prerequisites, unassigned routes
and mismatches with actual agenda files are rejected before semantic Verify.

## Rendering, repair and recovery

From the campaign root, using the running runner version:

```sh
opsx-build render-agenda
opsx-build check-agenda
```

These commands read the checkpoint's pinned inventory and acceptance settings;
they do not run a campaign, invoke a model, execute acceptance, or change the
checkpoint. `render-agenda` is an authoring command. `check-agenda` is read-only.
The agent is given the exact current executable path to avoid finding an older
installed binary. Both commands can be run with `--no-config --no-campaign-config`.

The runner owns the entire generated README, each slice's Supplied requirements
and Prerequisites sections, and the terminal Required Tests and Project Goal
Coverage sections. It copies terminal commands exactly from the pinned gate in
order with its timeout, without asking the model to repeat them. Repeated
rendering is stable. Stale views are rejected, not silently repaired by Verify.

The agent owns slice titles, objectives, completion boundaries and delivery test
routes. Those reference full source expectations; they should not rewrite all
conditions into a new refusal list or invent an implementation harness. Source
mechanisms and decisions needed for decomposition still matter. Leave ordinary
implementation and test design to delivery. Referencing a requirement alone is
not evidence that a split preserves scope or a test has real assertions.

Apply/Repair must render changed assignments before returning. Subdivision must
update the record and views before its agenda-only commit. Recovery can
redistribute work in the failed slice, while preserving other accountability,
participants and existing prerequisites. Later, unstarted slices may additionally
depend on newly introduced hierarchical children of the failed slice. Recovery
cannot add unrelated or pre-existing prerequisites to those later slices, remove
their existing prerequisites, or change earlier slices' prerequisites. Dependency
ordering and cycle checks still apply to the complete assignment. Terminal
remediation adds contributions/routes
to new slices while retaining completed accountability and existing acceptance
routes. The terminal document stays byte-for-byte unchanged: its generated whole
coverage and prerequisites do not depend on mutable delivery slice lists.
Existing rollback and recovery limits still apply.

Archive and completion commits reuse valid verification evidence. They do not
need to regenerate unchanged views or repeat semantic coverage review. Final
acceptance still runs independently at the terminal gate, or for a standalone
change, not during bootstrap or each delivery slice.

## Validation and evaluation limits

Mechanical checks prove declared coverage, valid references, consistent views,
ordering and retained commands. Semantic Verify independently rejects changed
conditions, omitted scope within a split, ownership violations, impossible plans
and weaker acceptance. Each blocker identifies source, defective assignment or
prescription, and consequence. Missing repetition alone does not erase a fully
assigned source condition; harmless descriptive counts are observations.

The test suite uses synthetic frozen inputs under
`tests/fixtures/assignment-workflow` and deterministic agent fixtures. It covers
missing obligations/routes, ownership declarations, ordering, stale/altered
terminal commands, preservation through remediation, protected inputs and actual
stage boundaries. These are regression checks, not evidence of improved live
model first-pass quality.

Before claiming that improvement, evaluate new generation on frozen copies of the
five component inputs with recorded model settings, recording the first independent
review and its concrete defects. Include valid concise assignments and semantic
mutations that invert Finalize, require a profile for wrong outcomes, transfer
provider policy, or replace real assertions with weaker evidence. These mutations
can retain valid IDs and links; the semantic reviewer must still reject them.
Preserve original campaign evidence; do not evaluate by resuming or editing those
campaigns. Measure false blockers as well as false acceptance, review rounds and
cost. An eventual pass is not a first-pass quality result.
