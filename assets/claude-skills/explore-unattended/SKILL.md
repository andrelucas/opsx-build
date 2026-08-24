---
name: explore-unattended
description: Autonomously investigate a requested change using the repository and OpenSpec context, without routine user interaction.
---

Explore $ARGUMENTS autonomously.

The purpose of this skill is to perform the investigation that would
normally happen during OpenSpec exploration, but without conversational
question-and-answer unless progress is genuinely blocked.

## Interaction policy

Do not ask the user routine questions.

Resolve questions using, in order:

1. information already established in the conversation;
2. existing OpenSpec specifications and active changes;
3. project documentation and CLAUDE.md files;
4. existing architecture, implementation, tests, and conventions;
5. reasonable engineering judgement.

Do not ask the user to choose between reasonable technical alternatives.

Ask only when a material product requirement or externally observable
behaviour is genuinely ambiguous, cannot be inferred from available
evidence, and choosing incorrectly would materially change what is built.

## Worker capacity and decomposition

This skill may be running on a substantially smaller local model after a
frontier model assigned the requested slice.

Before broad investigation, assess whether the slice can reliably be proposed,
implemented, tested, and verified as one bounded OpenSpec change.

Return TOO_LARGE when the slice requires decomposition because it:

- coordinates substantial changes across several subsystems or compiler stages;
- contains multiple independently testable behaviours;
- requires major architectural discovery before implementation;
- has an obvious intermediate state that can be verified independently; or
- cannot reasonably be completed within one worker-model workflow.

Do not use TOO_LARGE merely because the work is difficult, unfamiliar, or
requires ordinary targeted investigation. Prefer reasonable engineering
judgement for a bounded slice.

When returning TOO_LARGE, stop before creating or modifying OpenSpec change
artifacts. Report:

- why the assigned slice exceeds a reliable worker-sized change;
- the independently verifiable boundaries causing the problem;
- a suggested ordered decomposition into smaller slices;
- prerequisites and acceptance criteria for each suggested slice.

## Exploration strategy

Do not attempt to understand the repository exhaustively.

Start from the requested change and identify the smallest likely relevant
part of the codebase.

Prefer this order:

1. Read relevant OpenSpec specs and active change artifacts.
2. Read root and relevant nested CLAUDE.md/project documentation.
3. Use repository structure to identify likely subsystems.
4. Use targeted search, symbol lookup, references, tests, and call paths.
5. Read only files needed to understand the relevant behaviour.
6. Expand the search only when evidence shows additional code is relevant.

Do not recursively scan the entire repository merely to establish general
understanding.

Prefer evidence from the implementation and tests over speculation.

## Scope of work

Investigate:

- current behaviour;
- relevant architecture and data/control flow;
- constraints imposed by existing specs and tests;
- likely implementation surface;
- important edge cases;
- compatibility implications;
- reasonable design alternatives where they materially matter.

Do not implement production code.

Do not create OpenSpec proposal/design/task artifacts unless explicitly
asked to do so separately.

## Terminal outcomes

Continue investigating until exactly one of these outcomes applies.

TOO_LARGE:
The assigned slice cannot reliably be completed as one bounded worker-model
OpenSpec change and should be decomposed by the planning model.

Report the evidence and suggested decomposition required by the worker-capacity
policy above.

READY:
There is enough evidence to formulate a coherent OpenSpec proposal without
further routine clarification.

Report:

- the current behaviour;
- the relevant implementation areas;
- the recommended approach;
- material alternatives considered;
- important constraints and risks;
- any assumptions that the subsequent proposal should preserve.

BLOCKED:
A material requirement cannot safely be inferred.

Report:

- the exact unresolved decision;
- the viable alternatives;
- why existing specs, repository evidence, and reasonable engineering
  judgement cannot resolve it;
- the consequence of each choice.

Do not finish with an open-ended question or ask what the user wants to do
next.
