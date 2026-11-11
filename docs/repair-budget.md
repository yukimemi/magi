# Granting another repair attempt to a stopped run

`resume` does not replenish an exhausted repair budget. Use an explicit repair
grant after inspecting the owning task, retained winner, current failure and
last pass. The grant queues **that run**, not a replacement implementation or
another task:

```sh
magi task show <task> --json
magi show <run> --json
# Read winner HEAD from its retained worktree and passes[-1].index from the run.
magi task grant-repair <task> --run <run> --kind gate --rounds 1 \
  --head <full-winner-sha> --expect-pass <last-pass-index> \
  --request repair-ticket-1 --reason "One more attempt after inspecting the gate failure"
magi serve
magi task show <task> --json
magi show <run> --json
```

`gate` grants repair of an ordinary nonzero gate failure with diagnostic output,
including a failed landing gate. `review` grants repair of the final unfixed
exhausted review's blocking findings or failed E2E. `--rounds` is required and
must be 1 through 3. Budgets that are not exhausted are refused except for the
explicit conflict-repair follow-ups described below. Timeouts, infrastructure
failures and missing current-head failure evidence are refused. Existing run
configuration and its provenance stay unchanged; this command cannot change
timeouts or recover conversations. The separate `base-conflict` kind below
authorizes only an inspected textual merge repair.

The terminal command prints a JSON receipt. Authorized standing chats use
`magi task control grant-repair` with the same arguments and the existing
control bridge. Implementation and reviewer seats cannot grant themselves
budget. Run this command only for the particular task the owner requested.

The receipt and exact-run reservation are one atomic task write. After a lost
response or restart, repeat **identical arguments with the same request ID**.
This returns the original receipt, even after settlement, without queuing again.
Reusing an ID with different arguments is an error. A new intentional grant
needs a new ID and current pass/head evidence. Inspect `repair_grants` in the
task for receipts and in the run for applied checkpoints, reserved attempts and
outcome. The reason is retained in the receipt and run events.

The task claim, run owner lock, driver execution lease and worktree output
leases exclude concurrent drivers and surviving tools. Live or unknown driver
ownership, a changed task/run/pass/branch/head, dirty worktree, moved or closed
PR, auto-merge reservation, unresolved question, owner hold or pending decision
refuses admission without a partial budget transition. Resolve holds and
decisions explicitly with the existing controls first. Commit retained dirty
work before requesting a grant; unpushed commits are preserved. The worker
checks ownership, task contract, owner decision and HEAD again before importing
the receipt. If these change after acceptance, the receipt remains audit history
and the worker refuses to spend it; no replacement run is started.

Attempts are reserved before the fixer runs. If the driver dies mid-fix, resume
preserves its files and commits, consumes that reservation once, and proceeds to
fresh assessment instead of dispatching the same fixer twice. A crash can
therefore consume an attempt without a completed fixer report. Another explicit
grant is needed if fresh assessment still fails after the allowance is spent.
Historical counters, fixer reports and failure evidence are retained. To resume
an interrupted pass explicitly, use `magi run --resume <run>` (or the existing
chat/Web resume control). This preserves the pending grant's authorization;
a newer owner hold or decision still refuses it.

Every repaired tree owes a complete fresh review, configured E2E and final gate.
A confirmation review creates no implicit fixer budget. Failed repair remains
blocked on subsequent resume, and red work is never published. PR delivery uses
the existing owner and push lease; repair does not transfer PR ownership. Local
modes finish with reviewed work ready for inspection. A grant never approves a
merge, closes or abandons a PR, or discards work. PR landing requires a fresh
owner decision after repair, even if the repaired HEAD did not change.

## Standing-chat decision application and receipt recovery

`magi task control decide` records the owner's response; its successful return
means **recorded, effect pending**, not applied. Inspect the original inquiry's
`decision.status` and application history. The worker must confirm a durable
effect receipt before the decision is resolved. An answered dependency question
can therefore remain queued across restart. Grant admission refuses that pending
effect under the same task claim used by answer recording and worker application.
Wait for application (or reconcile an uncertain effect through its own supported
control), then retry the identical grant request. Do not invent another request
ID to get past a pending decision. Pending decisions belonging to other tasks do
not block this task.

An older grant can already be accepted and unspent when a response recorded
before acceptance is applied afterward. Ordinary resume deliberately refuses the
changed decision generation. Dependency triage now reports **Retained repair
receipt needs reconciliation** with the retained IDs and original answer time,
rather than offering another generic resume. Keep the original receipt and run.
Inspect the original question, task resumption history and retained outputs, and
obtain the owner's explicit approval for this specific reconciliation:

```sh
magi task control reconcile-repair-grant <task> --run <retained-run> \
  --head <original-full-head> --expect-pass <original-pass> \
  --receipt <original-grant-request-id> --question <original-question-id> \
  --answer-at <exact-original-answered_at> \
  --request reconcile-ticket-1 --reason "Owner authorized the original receipt reconciliation"
```

The terminal equivalent is `magi task reconcile-repair-grant` with identical
arguments. The operation atomically appends reconciliation evidence and queues
that same run. It never replaces the grant, increases its rounds, resets counters
or starts a replacement competition. A lost response is recovered by repeating
**the same request ID and arguments**; the original reconciliation receipt is
returned even after dispatch, without queuing another repair. Confirm the receipt
before settling the recovery discussion in chat with `settle-decision` if needed.

Recovery requires an unimported, unspent ordinary gate/review receipt and the
exact original triage release. The question's accepted answer must predate the
grant, its generation must match the preceding approval, and the durable task
application must identify that question, response and instruction. Modern task
records also bind the full source evidence and before/after application record;
legacy recovery requires the retained answer and resumption history to prove the
same transition. Missing evidence is refused, never inferred from a timestamp
alone. Unanswered generic dependency questions filed after this release are
explicitly superseded in the reconciliation ledger; they cannot authorize an
additional resume. Later answers, owner holds/refusals, changed source evidence,
contract, head, pass, ownership, live drivers and uncertain effects still refuse
execution. Imported receipts and foreign-review/base-conflict selections require
their own recovery workflow. Do not edit store JSON, issue duplicate grants or
merge retained feature branches to recover this ordering failure.

## Repairing a retained textual base conflict

A generic `retry`, release, resume or `--recover-base` does not authorize editing
through a textual conflict. `retry` and release report the dedicated transition
before changing the task. After inspecting the conflict, explicitly authorize
one narrow repair on the existing run:

```sh
magi task grant-repair <task> --run <run> --kind base-conflict --rounds 1 \
  --head <retained-full-sha> --tip <recorded-base-tip> \
  --expect-pass <last-pass-index> --request conflict-repair-1 \
  --reason "Inspected the retained conflict" \
  --instruction "Resolve only the timeout conflict by choosing 7200; preserve both histories"
```

The chat equivalent is `magi task control grant-repair` with the same arguments.
The instruction is saved separately from the original review criteria. If a
previous retry changed the task criteria, the command refuses with the retained
criteria: restore them using `task edit` before granting the narrow repair.
Review-only tasks retain their review selection; the grant resumes the same run,
not a new branch review. It does not change the saved runtime configuration.

The grant binds the exact stop, HEAD, recorded base tip, pass, task contract and
owner decision. Admission and execution recheck checkout and PR ownership. A
moved tracking tip requires inspection and a new authorization; use the explicit
replacement workflow below if an old approval is still queued. This transition
does not fetch or grant synchronization budget. Existing episode and lifetime
synchronization counters remain unchanged.

The one attempt is consumed before Git work. A detached scratch tree merges the
two recorded commits. The editing seat receives the approved instruction and may
change only the reproduced conflicted paths. Launcher checks reject markers,
Git-operation identity changes, edits outside those paths and staged changes to
the clean merge baseline. The launcher creates a two-parent merge commit,
compares and updates the retained branch, and keeps the result SHA in the grant
checkpoint. Both original histories remain ancestors. Foreign checkout delivery
still uses the existing guarded fast-forward path, after verification succeeds.

On interruption, resume validates and completes that saved operation; it never
restarts the editing seat for the consumed attempt. Missing checkpoints, changed
metadata or unresolved contents produce a precise blocker and retain the scratch
work for inspection. Repeating the same request returns its original receipt;
changing the instruction requires a different request ID and valid current
admission evidence. An unfinished grant must be reconciled before another grant. If interruption
occurred before the conflict checkpoint (so no editing seat could have started),
the consumed generation ends with a blocker naming the current pass; inspect the
scratch and explicitly grant a new request ID to try again.
Old review records remain historical; gate and E2E evidence are archived, and the
result owes a fresh complete review, configured E2E and gate. Publication and
HEAD-bound merge approval use their existing ownership checks.

A completed repair whose fresh review or verification is red keeps the same run
blocked. Generic retry and release refuse before changing the task, rather than
converting its retained review selection into a successor. Inspect the fresh
failure and explicitly authorize its remaining repair through the existing
review or gate repair workflow.

Follow-up review or gate grants accept the same retained review selection only
while its original criteria and branch ownership still match. After a completed
conflict repair leaves a head-bound confirmation failure, an explicit `review`
or `gate` grant can authorize bounded attempts even if the ordinary allowance
was not exhausted. This exception is confined to that repair lineage and current
failed head; it neither resets ordinary counters nor authorizes generic release.
Successful follow-ups also owe guarded delivery to the original checkout.

## Replacing an unused approval after the base advances

If the base advances before a conflict grant is imported, resume refuses with the
current tip and an explicit replacement command. Inspect that new base and use a
new request ID, restating the narrow instruction:

```sh
magi task grant-repair <task> --run <same-run> --kind base-conflict --rounds 1 \
  --head <same-retained-head> --tip <current-base-tip> \
  --expect-pass <current-last-pass> --replace-request <old-request-id> \
  --request <new-request-id> --reason "Inspected the advanced base" \
  --instruction "Resolve only the approved conflict; preserve both histories"
```

The chat control command accepts the same options. Replacement is one atomic
task-ledger append: the old receipt remains immutable and is never imported or
dispatched. Repeating either request returns its own original receipt and does
not queue it again. Stale queue writers cannot drop the replacement. A further
base advancement requires another explicit replacement naming the latest unused
request. No tracking ref is rewound, no run or stop is replaced, and no ordinary
synchronization allowance is renewed.

Acceptance repeats the task claim, owner lock, proven-dead driver, execution and
output leases, HEAD, pass, task criteria, checkout and PR ownership checks. The
new target must be the observed current tip and descend from both the recorded
stop tip and the old approval's tip. Rewritten upstream history requires separate
reconciliation. Imported or dispatched grants cannot be replaced through this
option: their saved operations and any partial work must be inspected. The refusal
identifies that restriction rather than claiming a new request will retarget them.

Before any grant has been accepted, the owner may explicitly approve a current
descendant tip with `--tip`; the original textual stop remains bound as evidence.
The repaired merge records the newly approved tip and still owes fresh review,
E2E and gate checks on the same retained run.
