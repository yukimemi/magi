# Review startup reservations

A run identity is reserved before review takeover. At base commit
`518a282e2fe9aafa26953f463aeb54f7a7866c24`, `review_taking_over` reserved
an empty instruction, then propagated `handover::Refused` before `open_review`
filled the instruction. The daemon held the task before `task.start`, leaving
an unlinked PREP record. The launcher fetched this snapshot at
2026-11-01T16:43:35.624694Z; it is not evidence that the remote has not moved.

New reservations save their origin, purpose, requested branch and incomplete
startup marker before touching a branch. They remain blocked until construction
succeeds. Failures retain a Failed record with the original error and startup
stage. Interruption leaves an explicitly incomplete reservation requiring
inspection. Incomplete startups cannot resume or execute as implementation
runs, and automatic folding leaves their assets alone. A terminal-save error
is secondary to the original cause; the previously saved reservation still
requires reconciliation. Restoration is ownership checked and best effort,
and its observed outcome is recorded rather than assumed.

The task's `startup_failures` is audit history. It does not select a resume
source, grant recovery authority, add an execution pass or spend an attempt.
Existing startup error classification still determines holds and attempt costs.
Only successful construction and persistence of `task.start` dispatch execution.
`magi show`, `magi list`, and `magi task show` expose the startup evidence.

## The reported legacy reservations

| Reserved run | Originating task |
| --- | --- |
| `20261025-080132-b222` | `20261021-233844-7701` |
| `20261025-080219-1a4e` | `20261021-224654-a717` |
| `20261025-080253-7c8a` | `20261022-012707-d3cf` |

The supplied empty instruction, no candidates, incomplete preparation and
worker refusal logs are consistent with this path. Those facts alone do not
prove a run never executed. The operational store and worker logs are not part
of this isolated source checkout; no current task status (including a claim
that all three are done), liveness or approval state has been independently
verified here. No existing record was modified by implementing this fix.
The separate live-worker attribution investigation is unchanged: a legitimate
refusal must produce the same truthful startup history.

## Supported explicit reconciliation

Inspect each run and task with `magi show RUN --json` and
`magi task show TASK`. Match the worker log to the exact originating task,
review branch, reservation and refusal; preserve the relevant excerpt. Create
one evidence JSON file per reservation:

```json
{
  "run": "20261025-080132-b222",
  "task": "20261021-233844-7701",
  "branch": "the-exact-reviewed-branch",
  "refusal_log": "The relevant operator-reviewed worker log excerpt"
}
```

Run `magi reconcile-startup --evidence evidence.json` to validate and print a
plan. After reviewing that plan, use the same command with `--apply` to record
the repair. Every invocation, including apply, rechecks the task claim, owner
lock, execution lease, daemon claims, active or uncertain worker evidence,
durable never-claimed execution marker, origin, artifacts, worktree assets and
other runs' handover references. An unreadable run prevents excluding handover
references. Unknown liveness is a refusal, never permission. If any check fails,
leave the record untouched and investigate; do not use fold, delete files,
clear ownership or resume a held task to get past the check.

This deliberately narrow command handles empty legacy reservations only.
It marks the verified non-execution as Failed, records the branch and refusal
excerpt and appends separate startup history. It preserves the task's status,
attempts, timestamps, approvals and execution history, and does not remove any
store files or implementation work. Repeating the same evidence is idempotent,
including after a task-history write failed following the run update. Different
evidence cannot overwrite a completed reconciliation. Modern incomplete
startups with assets require inspection and an explicitly authorized new review
or selected-public-head recovery; they are never automatically retried.
