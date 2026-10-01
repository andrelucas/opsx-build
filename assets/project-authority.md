### Project authority and derived plans

The supplied project brief, system contracts, and component ownership/scope
govern the work. Read the supplied context in `openspec/config.yaml` and every
input it declares required, within its stated reading boundaries, before
planning. Do not wait for an apparent ambiguity to read required inputs. During
Apply, Repair and Verify, re-read the source requirements relevant to the
assigned work. Milestone commits package completed work: inspect the relevant
diff and consult sources only to resolve a concrete scope question, without
repeating planning or semantic review.

Supplied requirements govern generated agendas; those agendas govern subordinate
OpenSpec change artifacts and implementation only within those requirements.
Exploration conclusions, generated specs (including synchronized or archived
specs), task lists, code, and tests are derived evidence, not permission to
override their inputs. Follow any precedence explicitly declared by the supplied
inputs. A dependency contract describes how to consume that dependency; it does
not transfer ownership of its implementation to this component.

Keep the assigned slice and change identity, and implement it in coherent,
testable tasks. When a generated objective or acceptance criterion contradicts
supplied behaviour or ownership, correct the derived mistake rather than
implementing it, deferring it as a known discrepancy, or subdividing it into
more wrong work. Preserve the source requirement's conditions and exceptions.
Do not edit supplied contracts, expand component scope, or weaken
maintainer-owned conformance tests to make generated work appear consistent.

Respect stage ownership: exploration and verification diagnose; planning changes
planning artifacts; Apply/Repair correct the affected work; milestone commits
preserve and commit that work. Bootstrap remains
planning-only. Do not repair work during verification. Use BLOCKED only when
authoritative inputs require an external decision or necessary external input
is unavailable; cite the gap and the smallest decision needed.

### Ordinary context workflow: anti-Karen rule

Unless this run explicitly uses the supplied-contract workflow (OpenSpec schema
`opsx-supplied-contracts`), apply a materiality threshold to review. Report RETRY
only for a concrete defect that prevents delivery or verification of the
requested behaviour: a missing requirement, incorrect observable behaviour,
failed required check, or an unworkable plan. State the consequence and the
smallest necessary correction. Wording, citation bookkeeping, preferred designs,
speculative edge cases beyond the brief, and explanatory inaccuracies with no
effect on the required outcome are non-blocking observations. Return VERIFIED
when required outcomes and checks pass despite such observations. Do not turn
observations into mandatory repair tasks or expand the scope on successive
reviews. Actual correctness defects and missing required tests still block.

During ordinary bootstrap, verify coverage, usable slice boundaries, observable
acceptance criteria and the final whole-project gate. Leave implementation
mechanics and detailed test design to implementation and its tests. Do not
require exhaustive requirement labels, verbatim quotations, or duplicated
implementation recipes. A plausible plan need not prove every implementation
detail before work starts. Correct a materially impossible prescription by
leaving the choice to implementation while preserving the required outcome and
test.

### Supplied-contract workflow

Only in the supplied-contract workflow, enforce exact source references,
component ownership, conditions, exceptions and protected acceptance scenarios.
Return RETRY for concrete correctable agenda, change-spec, implementation or
test mistakes against those contracts, citing the governing source and affected
artifacts. Apply/Repair synchronize affected derived plans, specs,
implementation and tests. A contradiction introduced by generated planning is
the campaign's responsibility to correct; do not defer it or weaken the contract.
