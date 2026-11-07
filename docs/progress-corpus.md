# Passive progress corpus

Normal unattended runs automatically capture local progress evidence under
`.git/opsx-build/corpus/<operating-policy-run-id>/`. Resume retains the run ID,
appends a new event segment and continues its sequence. Existing runs start
capturing when resumed with this version; earlier observations are not invented.
Interactive sessions, configure and connection tests have no normal run guard
and do not create this corpus.

Capture is observational. It does not call a model, grant a progress extension,
change any deadline or request allowance, close findings, or grant acceptance.
Capture errors warn once and disable capture for that process, while the normal
workflow and operating-policy checks continue. A later resume can retry capture.

## Export

```sh
opsx-build corpus export --repo /path/to/campaign --output /path/to/new-export
# Optional: select one operating-policy run.
opsx-build corpus export --repo /path/to/campaign --run RUN_ID --output /path/to/new-export
```

The destination must not exist. Export does not load campaign settings, invoke
agents, resume a workflow or run recorded commands. It validates referenced
SHA-256 objects and publishes the bundle only after successful export. A failed
export leaves no apparently complete destination and never overwrites an
existing one. Capture may remain live; the bundle reports live/interrupted
segments and partial final records instead of inferring a successful outcome.

| File | Contents |
| --- | --- |
| `decisions.jsonl` | One record per decision point, with input, observed policy action, later outcome references, null annotation and `training_eligible: false`. |
| `model-inputs.jsonl` | Past-only inputs: task, context, events since the previous checkpoint, current and baseline file/review evidence, check scope/generation and available operating counters. No observed policy action, later outcome or actor model metadata. |
| `outcomes.jsonl` | Request results, independent acceptance reports and run summaries, kept separate from judge inputs. |
| `events.jsonl` | The complete captured observation stream for audit. |
| `objects/<sha256>` | Deduplicated source contents, file manifests and complete retained check/review artifacts. Only objects reachable from exported observations are copied. |
| `schema.json` | Version 1 JSON Schema, with named definitions for events, decisions and model inputs. |
| `manifest.json` | Format/runner version, counts, capture gaps, content-hash algorithm and eligibility/privacy notes. |

`decision_id` combines run ID and cutoff sequence. Each event records wall time,
sequence and, when available, request ID, invocation ID, backend attempt, session,
stage, change and campaign iteration. The check scope includes its scope ID and
generation. Monotonic event sequence establishes observation order even if the
wall clock moves. Baseline references and all input events precede the cutoff;
later outcome references follow it. Observation times are runner receipt times,
not independent attestations of when a provider or command performed the work.

## What is captured

- Exact dispatched stage prompts, bounded public agent text and stage reports.
- Public tool calls/results from Claude, Codex and OpenCode telemetry, including
  call correlation, actual reported exit status, failures and truncation.
  Provider-private reasoning blocks are excluded. Invisible/internal subagent
  actions remain unknown.
- Check receipts and adjacent stdout/stderr copied when observed. Mutating or
  deleting the original later does not rewrite a prior decision input.
- Review records and independent acceptance reports with bounded command logs.
  A verifier's finding is
  still a claim; a process exit alone does not prove test adequacy.
- File contents at request boundaries and sampled checkpoints, covering the
  campaign and runner-declared product repository. No Git commit is needed to
  retain a dirty intermediate version.
- Stage/review checkpoints, dispatch and completion observations, actual watchdog
  extension/stop decisions and a periodic observation at most once per minute
  while requests are polled. Periodic observations use action `observe`; they do
  not claim the watchdog assessed productive coding at that moment.

Source snapshots contain tracked and non-ignored regular files plus their root,
path, content hash, byte count and executable bit. They are bounded to 4 MiB per
file, 64 MiB
and 4096 retained files per snapshot. Symlinks, unrepresentable paths, missing or
unreadable files, sensitive filenames and budget omissions are explicitly
recorded. Ignored files, external services and toolchain/runtime state are not
captured. Each manifest records its capture interval: this is not an atomic
worktree snapshot or an independently replayable environment.

Visible payloads are limited to 64 KiB of serialized JSON with explicit
truncation; check stdout/stderr retain up to 64 KiB each. Complete structured
check/review artifacts are separately retained as objects. Structured credential
fields are redacted and `.env`, `.env.*`, private-key filenames and
`credentials.json` are omitted from file capture. Arbitrary source, command and
output text can still contain sensitive information. Run and export directories
are private on Unix. Nothing is uploaded; inspect and redact before external use.

## Annotation and training

These are unlabelled evidence entries, not automatic training judgments. The
historical watchdog action is recorded separately from the judge input and is
not a correct-answer label. Add human annotations in a separate dataset keyed
by decision ID, retaining annotator, confidence, disagreement and review state.
Review inputs blind to subsequent outcomes first. Missing evidence permits an
indeterminate judgment; interrupted runs and environment failures do not imply
an agent was stuck.

All entries remain ineligible for training until reviewed and selected by the
downstream corpus workflow. Run IDs provide an initial grouping, but task/source
lineage and near-duplicate detection are still needed before allocating training
and evaluation splits; `family` remains null rather than being guessed. This
version provides no training upload, reward assignment or model training command.
