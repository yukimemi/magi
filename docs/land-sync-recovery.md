# Recovering Exhausted Landing Base Synchronization

Landing base synchronization (`land.rs::rebase_landing_worktree`) tracks its own
cumulative attempt counter (`land_sync_attempts`) against `config.graph.land_rounds`
after a pull request is already open. It is distinct from:

- Pre-publication base synchronization (`base_sync` / `recover-base`),
- Review or base-conflict repair grants (`grant-repair`), and
- Unchanged-tree verification recovery.

When a run exhausts `land_sync_attempts >= land_sync_budget()` while synchronizing
an open PR against a moving base, magi stops the run (`RunStatus::Blocked`) with
`"landing base synchronization budget exhausted"` and holds the owning task.
Ordinary `resume`, `release`, `triage`, standing automatic recovery, automatic goal
turns, and daemon restarts never refill this budget or start a replacement
competition.
An explicit exact-run resume can still reconcile an externally merged or
closed PR without granting or spending a synchronization attempt. An open PR
at exhaustion still requires the recovery command below.

## Supported CLI and Chat Command

Inspect the retained worktree HEAD, the open pull request head, and the run record
(`magi show <saved-run>`), then explicitly authorize **one** bounded additional
landing base-sync attempt:

```sh
magi task control recover-land-sync <saved-run> --request <unique-id> --head <full-40-char-head-sha>
```

- **Operator terminal (CLI):** runs directly when invoked outside a chat or agent
  turn (`MAGI_RUN` and `MAGI_NODE` unset). Receipts are stored under
  `<home>/land-sync-recovery-controls/cli/<request>.json`.
- **Chat (authenticated control channel):** allowed **only** on an explicit owner
  input turn (`Source::Input`). Refused on standing automatic recovery turns and
  automatic goal wakes. Receipts are stored under
  `<home>/land-sync-recovery-controls/<talk-id>/<request>.json`.

`--head` must be the exact full 40-character hex SHA matching:

1. the clean retained winner worktree `HEAD`,
2. `refs/heads/<winner.branch>`,
3. the run's saved `pr_pushed_head`, and
4. the live open pull request's `headRefOid` from the forge.

## Budget Bounds and Legacy Runs

- `land_sync_attempts` is **never** reset; it keeps full cumulative attempt history.
- Each consumed authorization adds `GRANT_ATTEMPTS = 1` to
  `land_sync_recovery.extra_attempts`. The effective budget is
  `config.graph.land_rounds + land_sync_recovery.extra_attempts` minus grants
  invalidated before their attempt starts. Invalidated consumed grants still
  count against the lifetime limit; actual synchronization attempts never change.
- Lifetime recovery grants are strictly capped at `MAX_GRANTS = 4` per run. Once
  reached, further `recover-land-sync` requests are refused.
- Legacy runs saved before `land_sync_recovery` existed are recognized centrally
  when all of the following hold simultaneously:
  - `status == Blocked` with an open PR landing checkpoint (`landing_checkpoint`),
  - `land_sync_pending == true`, `land_sync_target == None`,
    `land_rebase_from == None`,
  - `land_sync_attempts > 0` and `land_sync_attempts >= land_sync_budget()`,
  - known exhaustion diagnostic (`"landing base synchronization budget exhausted"`),
  - no pre-publication `base_sync_blocked`, no active scratch rebase, no resource
    or sequencer stop, no owner `merge_decision`, and no active repair checkpoint.

## Two-Phase Request Receipt and Revalidation

Each `recover-land-sync` request executes a two-phase transaction under the task
claim, owner lock, and execution maintenance lease:

1. **Prepared (`prepared`):** validates ownership, task assignment, instruction,
   dead driver liveness, exact retained/published HEAD, open PR state, and base
   tip fast-forward lineage, then writes the immutable request/binding receipt
   before mutating `run.json` or the queue.
2. **Authorized (`authorized`):** appends the unconsumed authorization to
   `run.json` (`land_sync_recovery.authorizations`) and updates the receipt.
3. **Applied (`applied`):** atomically commits the exact-run queue pin
   (`resume_override.pinned_run`) and durable request marker
   (`task.land_sync_recovery_requests`) against the pre-transition task snapshot,
   then marks the receipt applied.

After an interruption, repeat the **exact same** `--request` ID and arguments:

- An already-applied request returns its prior result without re-authorizing or
  re-queueing.
- An interrupted `prepared` or `authorized` request reconciles safely without
  creating a duplicate authorization when the binding still matches.
  The original receipt binds the approved base and stop; normal fast-forward
  movement does not change that binding.
- A corrupt/unreadable receipt, mismatched request arguments, or an `authorized`
  receipt with no matching authorization in `run.json` is refused as uncertain;
  inspect the receipt file before taking action rather than minting a new ID.

## Dispatch, Execution, Verification, and Approval

Pending authorizations are revalidated **again** at daemon dispatch and **again**
inside `land.rs` after acquiring the execution lease, before changing `status` to
`Landing` or adopting any observed PR head:

- Any change to ownership, task assignment, instruction, owner decision, retained
  HEAD, open PR state/head, or non-fast-forward base branch history invalidates
  the authorization durably (`consumed = true`, `invalidated = Some(...)`) and
  holds the task without touching Git.
  An unavailable observation (including fetch failure) instead holds the task
  without invalidating the authorization. Resume the exact run to retry the
  observation under the same owner generation.
  A confirmed invalidation after consumption returns the run to exhaustion;
  a new request can authorize another attempt within the remaining lifetime
  limit, after the usual exact-head and base-lineage admission checks.
- Normal fast-forward movement of the base branch is permitted.
- On valid execution, the authorization is marked `consumed = true` and
  `extra_attempts` is incremented and saved to `run.json` **before** any Git
  operation.
  A restart before the authorized synchronization starts revalidates the same
  binding without consuming another grant. A rewritten base is refused.
  If the retained head already contains the base, the attempt is charged and
  saved before verification or gate repair begins, not after publication.
  An interrupted repair resumes the same attempt without another charge or
  revalidation against the original authorized HEAD.
- Prior gate, pre-gate, `head_e2e`, `gate_repair_e2e`, and review `verified_head`
  markers are archived into `land_sync_recovery.authorizations[*].archived_verification`
  and `verification_history` and cleared so **fresh head-bound E2E and gate**
  must pass before pushing to the existing PR (even if HEAD is unchanged).
- Prior merge approval questions asked before the consumed recovery are never
  reused; fresh head-bound owner approval is requested whenever required.

## Inspecting Diagnostics and History

- `magi show <saved-run>` displays the **landing base sync** section with
  cumulative `attempts <used>/<budget>`, `grants <used>/<max>`, active stop
  details, next action command, and every authorization's status (`pending`,
  `consumed`, or `refused (<reason>)`).
- Machine holds, notices, and stop reasons include the exact attempt/grant counts
  and the `magi task control recover-land-sync` command.
