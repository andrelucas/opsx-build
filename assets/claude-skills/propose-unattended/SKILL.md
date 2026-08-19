---
name: propose-unattended
description: Create a complete OpenSpec proposal autonomously from an already-understood change, without routine user interaction.
---

Create a complete OpenSpec proposal for $ARGUMENTS.

Use conclusions already established by preceding exploration where
available. Preserve OpenSpec's normal artifact workflow rather than
inventing a parallel planning process.

## Interaction policy

Operate autonomously.

Do not ask the user to choose between reasonable technical alternatives.

Resolve ambiguity using, in order:

1. conclusions already established during exploration;
2. existing OpenSpec specifications and active changes;
3. project documentation and CLAUDE.md files;
4. existing architecture, implementation, tests, and conventions;
5. the least surprising reasonable engineering choice.

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

1. Determine an appropriate kebab-case change name.

2. Create the change if it does not already exist:

       openspec new change "<name>"

3. Inspect its current artifact state:

       openspec status --change "<name>" --json

4. Determine which artifacts are ready from the returned dependency graph.

5. For every required artifact, obtain the authoritative OpenSpec
   instructions before writing it:

       openspec instructions <artifact-id> --change "<name>" --json

6. Follow the returned instructions, context, rules, template,
   dependencies, and output path.

7. Before producing a dependent artifact, read its completed dependency
   artifacts from disk.

8. After creating each artifact, run:

       openspec status --change "<name>" --json

9. Continue until all artifacts required before implementation are
   complete.

10. Finish with:

       openspec status --change "<name>"

Do not bypass OpenSpec's dependency structure.

Do not hand-invent proposal/spec/design/task formats when OpenSpec provides
authoritative instructions for them.

## Repository investigation

Do not repeat a full repository exploration if an unattended exploration
has already established the relevant areas.

Read only enough code and documentation to verify or elaborate the
proposal.

Prefer:

- existing exploration conclusions;
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
