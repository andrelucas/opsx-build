# Review continuity and repair convergence

## Evidence and lessons, 22 October 2026

This work follows the Sirocco maintainer's consolidated feedback,
`OPSX-BUILD-FEEDBACK-2026-10-22.md`, and a separate read-only runner investigation.
The source-free reconstruction exercise exposed both real defects and unnecessary
reconstruction of review context. It does not establish that every repair was
unnecessary or that a passing supplied suite establishes complete acceptance.

At 15:34 BST, completed post-bootstrap records across four components contained
18 RETRY verdicts and 16 Repair invocations. Failed Verify plus Repair consumed
207.3 summed stage-minutes and about USD 122.02 in model-reported cost. These
include necessary repairs, sum concurrent campaign activity rather than makespan,
and omit unfinished invocations. Other published snapshots use different cutoffs
and accounting buckets; they are not controlled before/after measurements.

Observed substantive defects included resuming an interrupted non-resumable seal,
losing reserved recovery capacity, incorrect deadline checks, mutable result
aliasing, and incorrect failure decisions. Test defects included a purported
concurrent schedule executed sequentially, an absence assertion that could never
fail, expectations derived from candidate constants instead of fixed contracts,
and an assertion demanding one result where the contract permits two race outcomes.
These remain reasons to reject a candidate.

The runner amplified the work in several ways:

- Apply and Repair summaries were discarded rather than handed to Verify.
- Each Verify started fresh without durable findings or actual review coverage.
  Successive reviews discovered pre-existing defects after earlier broad claims
  of completion. An assigned obligation is not evidence that it was reviewed.
- Repair invoked the full Apply workflow and could repeat planning and audits.
  Conversely, repairing only the examples cited in a finding left sibling paths
  violating the same governing rule.
- Execution results and changed coverage claims were conflated. Generated plans
  demanded full command refreshes after evidence-only corrections.
- Repair exhaustion was described as a capacity failure and triggered rollback
  and subdivision. Subdivision helped one broad slice, but a counter alone cannot
  distinguish broad scope, newly discovered old defects, regressions or ambiguity.
- Live installation replaced the running executable's pathname. Agenda-command
  guidance sometimes named a deleted path, requiring model-discovered workarounds.

Project preparation also matters. Whole inherited briefs left applicability
decisions unresolved; substitute capabilities were sometimes assumed; and vague
terms such as "strict JSON" left decisions implicit. The project maintainer owns
those decisions and promotion of useful counterexamples into future public inputs.
The runner must expose gaps without inventing requirements or changing protected
inputs. Component substitutes and cross-builds do not establish native/system
qualification.

## Boundaries

Original sources and their precedence remain authoritative. Review records are
candidate evidence, never another specification. A complete source reference
inherits its conditions and exceptions; repeated paraphrases are unnecessary.

Standard mode retains its materiality policy and needs no obligation inventory.
Supplied-contract mode retains strict scoped obligations, required builder
observations, ownership and protected inputs. No project-specific identifiers,
protocol rules, scenarios or package layout belong in runner code.

Independent verification remains independent. A closed finding, prior review or
matching fingerprint cannot establish that no undiscovered defect remains. New
evidence can reopen a finding or a previously examined area. Unexamined and
partially examined areas must not be represented as complete. Required delegated
review must finish, or use the existing bounded INCOMPLETE handling.

A repair makes the smallest complete correction to the governing rule. It checks
the counterexample, relevant sibling paths and allowed alternatives, updates
affected evidence and adds meaningful regressions. It does not authorize unrelated
refactoring or transferring a dependency owner's responsibilities.

The exact final commands, order, protected inputs and per-command timeouts remain
pinned and independently executed. Prior failures cannot become passes through
reuse. Existing Verify-to-Archive-to-completion evidence reuse is preserved.

## Implementation order

### First change: durable review continuity

1. Retain bounded Apply/Repair handoffs and successive verification rounds in
   runner-managed checkpoint evidence, with snapshots and limitations.
2. Carry structured findings and actual review areas through supported backends.
   Findings retain source references, consequence, evidence kind, affected scope,
   category, discovery history and verifier-controlled closure/reopening. Defect
   kind and discovery history are separate: an implementation defect can also be
   a previously missed defect or a regression.
3. Give Repair a dedicated rule-complete brief. Give Verify the open findings,
   prior coverage, changes and worker's repair claims as diagnostic evidence.
   Worker claims do not close findings and observations do not become repair tasks.
4. Preserve bounded review and repair limits, accurate exhaustion diagnostics and
   recoverable state. Distinguish exhaustion from an explicit capacity report.
5. Add project-neutral regression fixtures for closure, reopening, incomplete
   coverage, shared changes, observations, ambiguous sources and unchanged final
   gates. Record enough provenance to distinguish new findings from repeated work.
6. Make matching-version agenda commands reachable through supported paths after
   installation replaces the normal executable. Avoid platform-specific advice
   that requires a model to discover a workaround.

### Second change: conservative execution-evidence reuse

Record command, working directory, relevant environment/tool/dependency identities,
protected inputs, code/tests, runtime premises, actual exit and retained logs.
Distinguish changed review claims from changed execution inputs. Review corrected
claims and invalidate affected checks conservatively; a shared helper can affect
many obligations. Unknown impact or changed/unknown native and clock premises
requires rechecking. Source hashes alone cannot freeze live premises.

Do not introduce a blanket documentation exclusion: a check may itself validate
documentation, a delivery record or its hashes. Nor may generated convenience
rules expand the supplied acceptance indefinitely. Keep independent final
execution unchanged before optimizing repeated pre-terminal checks.

The implementation uses a runner helper to capture actual process results rather
than treating a model summary as an execution receipt. Check records and logs live
under private Git metadata at `opsx-build/checks/<run>/<attempt>/`. The checkpoint
retains the run identity and preserved helper executable; each stage supplies its
change, stage and repository context. Resumes retain the same run; new runs do not
inherit cached passes.

The default input boundary includes all tracked and non-ignored files in both
planning and product repositories. A caller can identify individual review-only
files, with a reason why the command does not consume each file; this is an
explicit diagnostic claim, not an automatic documentation exclusion. Protected
inputs cannot be excluded. Ignored or external dependencies must be listed as
additional inputs, and executable tools must be declared. Reuse also requires the
same command, working directory, timeout, environment fingerprint, tool identities,
unchanged logs and an explicitly stable runtime premise. Unknown or live premises
force execution. Dependency completeness and runtime stability remain claims for
independent review; fingerprints do not establish them.

Only the latest attempt for a named check in the current change can be reused.
Failed, interrupted, incomplete or input-mutating checks cannot supply a cached
pass. The helper preserves the actual process exit code and separately records
invalidation reasons. A failed or interrupted independent acceptance attempt also
invalidates helper receipts from before that attempt, even if files are unchanged.
Independent review still establishes coverage and whether
fresh execution is required. The final acceptance gate never reads this cache.

## Evaluation

The 23 October blob-service run exposed an avoidable manual continuation. Its
0007 verifier had four blocking findings and completed delegated audits, but
left clean check receipts partial after temporary diagnostics. The runner
correctly retained `INCOMPLETE` coverage and refused to start Repair, then asked
the operator to resume despite needing no external decision. Retained review
round `24a42aaa-5f46-411e-ae4f-6e1f8f9c89f1` records that partial coverage and
the four findings. The correction is
automatic bounded continuation, with explicit unfinished work in the handoff and
stop diagnostics. The existing status distinction is sufficient: `INCOMPLETE`
means autonomous work remains; `BLOCKED` names a genuine external dependency.
No model-controlled retry recommendation or relaxed review gate is introduced.

Verify now saves partial review evidence after its same-session continuation and
starts a fresh reviewer under the same cumulative operating-policy request
allowance. Findings and completed/unfinished areas survive the handoff. Other
explicitly incomplete stages continue in the same session under their original
deadline. Exhaustion reports the obstacle and preserved evidence; an identical
manual resume does not purchase a fresh allowance. These are runner regression
guarantees, not measured improvements in live review quality. Existing campaigns
and installed runners are outside implementation and testing.

Validation: all 450 deterministic tests pass with
`TMPDIR=/private/tmp cargo test --all-targets`; Clippy with `-D warnings`,
formatting and diff checks pass. New cases exercise productive automatic
continuation in both workflows, cumulative exhaustion and identical resume,
missing review coverage, partial independent closures, recurring findings and
immediate external blockers. Existing final acceptance checks remain required.

The third runner change, [operating policy](operating-policy.md), uses retained
evidence to stop fruitless repetition and explain bounded recovery. It does not
reopen the completed continuity/reuse backlog. It applies in both modes with
normal operating defaults; cost remains diagnostic context. Independent acceptance
continues to define completion.

Deterministic synthetic contracts and fake stage backends establish orchestration,
state, provenance and gate invariants; they do not prove a model's review quality.
Include rejection of real missing observations, wrong conditions/ownership and
incomplete repairs, while retaining valid alternative designs and allowed outcomes.
Test that a later genuine defect can reopen previously examined work.

Measure first-review results, findings by category and discovery round, reopened
and reintroduced findings, invalidation reasons, repeated checks and review work,
cost/time, false blockers and substantive defects caught. Fewer retries alone is
not success. Keep runner/model/connection settings and executable provenance.

Live evaluation needs separately authorized fresh frozen inputs and recorded
settings, initially one demanding component. Do not reset or rewrite existing
campaign evidence, or launch/resume campaigns as part of implementing this work.

## Work log

- Investigation and combined findings documented before implementation.
- First implementation in 0.29.0 retains Apply/Repair handoffs,
  structured independent review rounds, stable findings and actual coverage.
  Only the verifier can close or reclassify a known blocker with stated evidence;
  omission does not close it. New evidence can reopen it.
- JSON and marker report formats are supported through Claude, Codex and
  OpenCode. Incomplete/inconsistent reports use the existing bounded continuation;
  a still-incomplete structured report is retained at Verify without a repair
  cycle or acceptance transition. Missing structured coverage remains unknown.
- Dedicated repair instructions address the affected governing rule and related
  alternatives, rather than invoking the complete Apply skill. Prior review and
  candidate diffs are evidence to reassess, not automatic check exemptions.
- Repair exhaustion preserves work and stops for bounded user-directed recovery;
  it no longer fabricates a capacity report. Explicit capacity recovery retains
  its rollback/subdivision contract and archives superseded review history.
- Assignment helpers use a version-checked, fingerprinted executable copy in
  private metadata. Validation output identifies the actual helper version.
- Deterministic tests cover both workflows, retained/open/closed/reopened and
  partial findings, scope changes, protocol transport, bounded resume and exact
  final acceptance execution. No live campaign or model evaluation is part of
  these tests. Execution-evidence reuse was the separately described follow-up.
- Validation: 410 tests pass with `TMPDIR=/private/tmp cargo test --all-targets`;
  `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` and
  `git diff --check` pass. Fourteen new regression tests supplement the existing
  suite. These results verify orchestration and preservation boundaries, not
  measured improvements in model review quality or live campaign cost.
- The 0.30.0 working copy adds actual execution receipts and conservative reuse.
  `opsx-build check` receives JSON on stdin and writes private per-run receipts,
  exact shell-command outcomes, stdout/stderr and invalidation reasons. Check
  context, run identity and the preserved helper survive checkpoint/resume.
- Reuse starts disabled (`runtime: unknown`). Explicit stable-premise declarations
  remain independently reviewable. All Git-visible files in both roots,
  protected inputs, declared additional dependencies, executable identities,
  inherited environment fingerprint, host/kernel and logs are checked. Individual
  justified review-only exclusions are supported; no blanket exclusion is added.
- The latest named attempt wins, including failures and incomplete receipts.
  Calls for the same key are locked and sequenced independently of wall time.
  Missing/corrupt receipts force fresh execution without fabricating a check
  failure. Timeouts/interruption retain partial logs and do not invent exit codes.
  Failed/interrupted independent acceptance invalidates earlier helper receipts.
- Fifteen new regressions cover evidence-only edits, shared/new/deleted code,
  tests/configuration, tool and dependency changes, environment identities,
  native/live premise limits, log integrity, process interruption/timeout,
  input mutation, protected inputs, sidecars, run isolation and concurrent calls.
  Both workflows retain receipts across resume. A command already passed and
  reused through the helper still executes independently as the exact final gate
  with its pinned timeout. The acceptance executor and selection are unchanged.
- Validation: 425 tests pass with `TMPDIR=/private/tmp cargo test --all-targets`;
  Clippy with `-D warnings`, formatting and diff checks pass. No campaign input,
  live process or installed runner was changed. Live model cost/quality evaluation
  remains outstanding; these deterministic results establish orchestration only.
- The 0.31.0 working copy adds a default operating-policy guard in both modes.
  Independent closure evidence establishes progress; recurring findings,
  fruitless repair/review transitions, repeated reporting gaps, stage dispatches,
  recovery without progress and selected runtime bounds can stop further work.
  No extra diagnostic call, automatic model change or policy-driven rollback is
  introduced. Incomplete acceptance stays incomplete.
- Policy state and actionable reports live in private Git metadata. A normal
  resume retains consumed allowances, review history and unresolved operations;
  explicit policy revisions authorize only the remaining cumulative allowance.
  Owned process groups are cleaned up on cancellation, with unconfirmed work
  reported for reconciliation. Bootstrap and iteration handoffs carry the same
  policy ledger and check-helper scope.
- Twenty new regression tests cover productive versus fruitless repair, genuine
  new and recurring defects, review uncertainty, pinned policies and cumulative
  resume, internal continuations/compaction, concurrency, pre-rollback stops,
  owned-child cleanup and interrupted acceptance. All 445 tests pass with
  `TMPDIR=/private/tmp cargo test --all-targets`; Clippy with `-D warnings`,
  formatting and diff checks pass. Existing campaigns and installed runners are
  untouched. Live evaluation of default thresholds remains outstanding.

## Sirocco handoff for the check-receipt change

No new campaign switch, contract format or receipt file is required from the
maintainer. The runner supplies the scoped helper and diagnostic context in both
modes. Keep original acceptance commands, protected inputs and source obligations
as prepared. Agents declare the execution boundary when invoking a check; those
declarations are not another project specification. If helper use is unsuitable,
normal execution and accurate summaries remain available without a cached claim.
Future public input improvements and promotion of counterexamples still belong
to Sirocco. Evaluate one separately authorized fresh demanding unit with frozen
inputs and recorded model settings before a broad restart; compare actual reruns,
invalidation reasons, cost, false blockers and substantive defects caught. Existing
campaign evidence remains untouched.
