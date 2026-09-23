---
name: propose-unattended
description: Autonomously create a complete OpenSpec proposal or report that no meaningful change remains.
---

Create a complete OpenSpec proposal for $ARGUMENTS.

Use conclusions already established by preceding exploration where
available. Preserve OpenSpec's normal artifact workflow rather than
inventing a parallel planning process.

## Interaction policy

Operate autonomously.

Run every command and delegated task synchronously. Never background or detach
an OpenSpec command or subagent. Do not return a terminal outcome while any
command or delegated task is still running.

Do not ask the user to choose between reasonable technical alternatives.

{{OPSX_BUILD_PROJECT_AUTHORITY}}

Resolve remaining engineering choices using exploration conclusions, existing
OpenSpec artifacts, project guidance, implementation, tests, and conventions
only where consistent with the supplied requirements. Choose the least
surprising reasonable implementation when those sources leave a choice open.

Ask only when a material product requirement or externally observable
behaviour is genuinely ambiguous, cannot be inferred from available
evidence, and choosing incorrectly would materially alter the requested
change.

Do not ask merely for:

- confirmation;
- naming preferences;
- implementation details;
- architectural choices that can reasonably be inferred;
- permission to proceed to the next OpenSpec artifact.

## OpenSpec workflow

Follow OpenSpec's artifact workflow.

Before creating or modifying a change, decide:

1. Whether the requested objective is already satisfied and no coherent
   implementation work remains. If so, do not create or modify OpenSpec
   artifacts; finish with DONE.
2. Whether targeted repository inspection establishes a concrete capacity or
   scope constraint that prevents implementing and verifying the assigned
   change. Return TOO_LARGE only when ordered tasks within this change would
   not resolve that constraint; do not create or modify OpenSpec artifacts in
   that case.

Worker describes a workflow role, not a smaller or less capable model. Assess
the assignment against your actual capabilities and available tools. The
default is to propose the assigned change and organise its work into tasks.
Multiple files, subsystems, test cases, or independently testable intermediate
steps do not by themselves justify TOO_LARGE. Neither do difficulty,
unfamiliarity, or the mere availability of a cleaner decomposition. Establish
feasibility through targeted inspection without implementing product code.

For TOO_LARGE, report:

- the specific constraint and repository evidence establishing it;
- what you inspected and why ordered tasks within this change are insufficient;
- the minimum necessary decomposition into testable delivery slices;
- prerequisites and acceptance criteria for each suggested slice.

Otherwise:

1. Determine an appropriate kebab-case change name.

2. Create the change if it does not already exist:

       openspec new change "<name>"

3. Inspect its current artifact state:

       openspec status --change "<name>" --json

4. Determine which artifacts are ready from the returned dependency graph.

5. For every required artifact, obtain the OpenSpec schema
   instructions before writing it:

       openspec instructions <artifact-id> --change "<name>" --json

6. Follow the returned instructions, context, rules, template,
   dependencies, and output path within the supplied contracts and component
   scope. Artifact-generation instructions do not override those inputs.

7. Before producing a dependent artifact, read its completed dependency
   artifacts from disk.

8. After creating each artifact, run:

       openspec status --change "<name>" --json

9. Continue until all artifacts required before implementation are
   complete.

10. Finish with:

       openspec status --change "<name>"

    Do not return READY unless the JSON status reports
    `isPlanningComplete: true`.

Do not bypass OpenSpec's dependency structure.

Do not hand-invent proposal/spec/design/task formats when OpenSpec provides
schema instructions for them.

## Repository investigation

Do not repeat a full repository exploration if an unattended exploration
has already established the relevant areas.

Read the declared required inputs first, then only enough code and documentation
to verify or elaborate the proposal.

Prefer:

- supplied contracts and component scope;
- existing exploration conclusions consistent with those inputs;
- OpenSpec artifacts;
- CLAUDE.md/project documentation;
- targeted symbol and text search;
- relevant tests and implementation files.

Expand investigation only when evidence requires it.

## Scope of work

Create all planning artifacts OpenSpec requires before implementation.

This may include, depending on the configured OpenSpec schema:

- proposal;
- spec deltas;
- design;
- tasks;
- other schema-defined artifacts.

Do not implement production code during this skill.

## Terminal outcomes

Continue until exactly one of these outcomes applies.

TOO_LARGE:
Repository evidence establishes a concrete constraint that prevents completing
and verifying the assigned OpenSpec change within the current workflow.

Return TOO_LARGE only before creating or modifying any OpenSpec change
artifacts. Report the evidence and suggested decomposition described above.

DONE:
The requested objective is already satisfied and no meaningful OpenSpec change
remains to propose.

Return DONE only before creating or modifying any OpenSpec change artifacts.
Report the repository and specification evidence showing why no change is
warranted.

READY:
The OpenSpec change exists and every artifact required before implementation
is complete.

Report:

- the change name;
- a concise summary of scope;
- the principal design decision;
- the generated artifact set;
- that the change is ready for apply.

BLOCKED:
A material requirement cannot safely be inferred.

Report:

- the exact unresolved decision;
- the viable alternatives;
- why existing exploration, OpenSpec material, repository evidence, and
  reasonable engineering judgement cannot resolve it;
- the consequence of each choice.

Do not finish by asking for confirmation or what the user wants to do next.
