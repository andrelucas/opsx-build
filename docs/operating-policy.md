# Operating policy and recoverable non-convergence stops

The objective is an independently accepted result within the operating bounds,
or a recoverable stop that explains what prevents completion. The guard addresses
fruitless repetition. Cost is diagnostic context, not a dollar-budget system.
This follows the Sirocco maintainer agent's 22 October 2026 addendum; its older
campaign examples motivated the change and do not measure this version's results.

## Normal behaviour and defaults

The guard applies to normal unattended builds, in standard and supplied-contract
modes, without a new campaign option or obligation inventory. Interactive sessions,
connection tests and configure are outside the build-run ledger. Existing repair,
output/provider retry, remediation and capacity-recovery limits still apply.

| Limit | Default | Meaning |
| --- | --- | --- |
| `max_no_progress_reviews` | 2 | Consecutive repair/review transitions with retained findings and no independently closed finding, after an initial structured review baseline. Also bounds consecutive transitions with unassessable review evidence, under a separate stop reason. |
| `max_recurring_finding_reviews` | 2 | Reopening rounds for the same independently closed finding within a change. Other closures cannot disguise repeated regression of that finding. |
| `max_requests_per_stage` | 12 | Runner-dispatched backend requests for a stage/change, including continuations, compatibility retries and billable compaction/naming operations. |
| `max_replans_without_verified_progress` | 2 | Capacity-recovery attempts without an intervening independent Verify/acceptance milestone. Checked before restoring a candidate's baseline. |
| `max_request_seconds` | 3600 | Model-request runtime, followed by supported cancellation and owned-process cleanup. |
| `max_active_seconds` | unset | Optional cumulative active time across normal resumes, including independent acceptance. Paused/offline time is excluded. |

These are operating defaults, not claims that a particular round count diagnoses
every project. A newly discovered real defect remains valid and blocking.
Independent closures with evidence count as progress even when another genuine
defect appears. Repair claims, smaller documents, commits, checked tasks and
assigned-ID percentages do not reset the no-progress trend. Missing structured
review history makes that trend unknown; it breaks the consecutive comparison.
Repeated unassessable outcomes reach the same allowance with a distinct
`review_evidence_gap` stop: consolidate independent reporting before another
product repair. Missing evidence is not proof of waste or a product defect.
Request and existing recovery bounds still apply.

An explicit `INCOMPLETE` result means authorized work remains without an external
decision or input. The runner continues automatically under the existing stage
request allowance instead of asking the operator to run an identical resume.
Other stages keep their session and original timeout. Verify uses one same-session
continuation, then retains its partial review and starts a fresh verifier with
the exact outstanding areas and prior evidence. Neither partial coverage nor
known blockers alone authorize Repair before review is complete.

Incomplete review handoffs do not consume repair cycles or count as unsuccessful
repair/review transitions. Independent closures recorded during partial review
still count when the completed review arrives; genuine reopening remains subject
to the recurrence guard. Missing structured coverage stays unknown. All backend
dispatches consume the same cumulative request allowance across fresh sessions
and manual resumes. An exhausted allowance stops before another model request;
the report includes the latest unfinished stage work and partial review's
findings, area limitations and evidence path. Diagnose that obstacle or explicitly
authorize a justified cumulative allowance change before resuming. `BLOCKED`
continues to require external intervention, and substantive process errors and
missing-status protocol recovery are not converted to autonomous continuation.

Repeated commands or unchanged file hashes alone never establish waste, nor that
a defect pre-existed. Dependency/caller changes can affect unchanged code. Review
snapshots and original review records remain available for diagnosis, with that
uncertainty stated. Native/live premise changes can require repeated execution;
the existing conservative check-receipt rules still govern reuse. The guard
neither suppresses new findings nor substitutes a cheaper acceptance route.

## Durable state and reports

The checkpoint references a private Git-metadata directory, normally:

```text
.git/opsx-build/policy/<run>/
  ledger.json
  owner.lock
  stop.json
  stop.txt
```

The ledger retains selected limits and policy revisions, request identities,
backend/session/stage, outcomes, reported cost or unknown, elapsed active time,
review/finding references, no-progress/recurrence trends, recovery counts and
unresolved operations. It is written before dispatch and after outcomes. Exclusive
ownership prevents concurrent resumes from consuming one allowance twice.
Normal resume, bootstrap-to-delivery handoff and campaign iteration changes keep
the same ledger. Review histories superseded by supported decomposition remain
recorded; new scope does not reset cumulative request/usage evidence. Check-helper
scope is also carried across those iteration changes.

A stop occurs after retaining a completed stage result and before the next model
request. An in-flight timeout uses cancellation instead; that stage is incomplete.
The report is refreshed after backend cleanup with the actual checkpoint stage
and request outcome. It includes:

- The trigger and counters against selected limits.
- The next stage, last independent milestone and remaining findings, including
  governing sources, consequences and required corrections/decisions.
- Supporting review-round paths and request/backend/session identities.
- Known reported cost subtotal and the number of attempts with unknown cost.
- Preserved work, any unconfirmed cancellation and a concrete recovery action.

The report requires no diagnostic model call. Policy stops are separate from
accepted completion, product findings and model-capacity escalation. There is no
automatic policy-driven rollback/subdivision, model upgrade, prompt experiment or
purchase of another diagnostic round. Explicit capacity recovery retains its own
contract and limits, with policy preflight before its rollback.

## Selecting and changing limits

A JSON file can override any default; omitted properties retain the documented
defaults. All numeric limits must be positive, and unknown properties are rejected.
For example:

```json
{
  "max_no_progress_reviews": 3,
  "max_requests_per_stage": 8,
  "max_active_seconds": 14400
}
```

Select it for a new run with `--operating-policy PATH`. It may also be saved as a
normal argv setting in `opsx-build.md`. The run stores the selected values;
changing that file's contents does not silently change an existing run's policy.
An explicit override on resume authorizes a policy revision:

```sh
opsx-build --repo /path/to/project --resume \
  --operating-policy /path/to/revised-policy.json \
  --direction 'Correct the governing rule across the affected paths and add its failing regression'
```

Do this after inspecting the stop's diagnosis and necessary external decisions.
The new allowance is cumulative. Raising a no-progress limit from 2 to 3 permits
one additional unsuccessful repair/review transition, not three fresh attempts.
Normal resume with the same exhausted allowance stops without another model call.
Request timeouts can resume the incomplete stage within the remaining request
allowance; diagnose the timeout or revise the limit rather than blindly repeat it.

If the process died without a final outcome, or cancellation was not confirmed,
the report identifies the unresolved request/session. Establish that the owned
backend work has stopped before resuming. The explicitly selected resume policy
can acknowledge exactly those reconciled IDs:

```json
{
  "acknowledge_unresolved_requests": ["request-id-from-stop-report"]
}
```

This does not invent a cost, mark its stage passed, or reset request/trend history.
Acknowledgements of old IDs cannot acknowledge a future request. Preserve the
checkpoint and ledger; forgetting/deleting them starts another run and discards
the protection of cumulative history.

## Acceptance and observation limits

The original requirements, source precedence and protected inputs remain
authoritative. Independent final commands retain their exact text, sequence and
timeouts and always execute independently of check receipts. An active-time stop
can interrupt that gate; its results/logs are retained as incomplete and resume
must rerun the pinned gate. No operating limit grants acceptance.

The runner counts dispatches it owns. Provider-internal requests, hidden subagent
work and automatic work inside a harness are not separately observable requests.
A CLI dispatch can contain several harness turns, including streamed steering
or compaction; its dispatch count is not a count of provider inference calls.
The request runtime bound still applies to that dispatch.
Cost reports may be absent or arrive at completion; recorded cost is a known
subtotal, never a claimed invoice cap or a zero total for unknown work. A forced
kill may lose part of the current active-time interval; its unfinished dispatch
is retained as unresolved instead of silently granting another attempt. The
report flags that active-time gap; the saved elapsed value is a known subtotal,
and acknowledgement does not reconstruct missing time. Runtime
cancellation includes transport shutdown/control grace; the deadline does not
certify that every external service stopped instantaneously. Unconfirmed cleanup
is explicitly reported and blocks further dispatch pending reconciliation.

## Validation and Sirocco handoff

Deterministic fixtures cover productive repairs, persistent open findings,
reopening despite other progress, missing coverage/usage, request bounds inside
backend continuations, pinned policy files, bounded explicit resume, cumulative
history across iterations, pre-rollback stops, owned-child cancellation and an
interrupted independent gate. Existing acceptance and evidence-reuse regressions
remain required. These establish runner behaviour, not a measured reduction in
live model waste or a universal optimal threshold.

The maintainer needs no new project semantics or rewritten campaigns. For a fresh
evaluation, retain frozen inputs and model settings; inspect avoided requests,
actionability, false alarms on productive repair and substantive defects caught.
Use ordinary defaults first unless a different operating allowance is deliberate.
Existing campaign inputs, workspaces and processes are outside this change.
