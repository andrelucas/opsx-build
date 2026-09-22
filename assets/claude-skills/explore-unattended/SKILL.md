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

Run every command and delegated task synchronously. Never background or detach
a command or subagent. Do not return a terminal outcome while any command or
delegated task is still running.

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

Worker describes your workflow role, not a smaller or less capable model.
Assess the assignment against your actual capabilities and available tools.
The default is to carry the assigned slice through one OpenSpec workflow.

Use targeted repository inspection to establish a practical implementation and
verification plan. Multiple files, subsystems, test cases, or independently
testable intermediate steps do not by themselves make a slice TOO_LARGE. They
can be ordered tasks within the same change. Difficulty, unfamiliarity, and
the mere availability of a cleaner decomposition are not capacity evidence.

Return TOO_LARGE only when that inspection establishes a concrete capacity or
scope constraint that prevents completing and verifying the assigned change,
and sequencing its tasks within the change would not resolve the constraint.
Keep this stage investigative; do not implement code to prove feasibility.

When returning TOO_LARGE, stop before creating or modifying OpenSpec change
artifacts. Report:

- the specific constraint and repository evidence establishing it;
- what you inspected and why ordered tasks within this change are insufficient;
- the minimum necessary decomposition into testable delivery slices;
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
Repository evidence establishes a concrete constraint that prevents completing
and verifying the assigned OpenSpec change within the current workflow.

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
