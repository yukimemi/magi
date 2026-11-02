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
must be 1 through 3. Budgets that are not exhausted, timeouts, infrastructure
failures and missing current-head failure evidence are refused. Existing run
configuration and its provenance stay unchanged; this command cannot change
timeouts, synchronize the base or recover conversations.

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
