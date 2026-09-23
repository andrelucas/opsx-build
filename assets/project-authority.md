### Project authority and derived plans

The supplied project brief, system contracts, and component ownership/scope
govern the work. Read the supplied context in `openspec/config.yaml` and every
input it declares required, within its stated reading boundaries, before
planning. Do not wait for an apparent ambiguity to read required inputs. For
later stages, re-read the source requirements relevant to the assigned work.

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
planning artifacts; Apply/Repair synchronize affected derived plans, specs,
implementation, and tests. Bootstrap remains planning-only. During verification,
return RETRY for concrete correctable agenda, change-spec, implementation, or
test mistakes, citing the governing source and affected artifacts. Do not repair
them during verification. Use BLOCKED only when authoritative inputs themselves
require an external decision or necessary external input is unavailable; cite
the exact conflict or gap and the smallest decision needed. A contradiction
introduced by generated planning is the campaign's responsibility to correct.
