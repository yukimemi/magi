# Resource wait cancellation and recovery

## Evidence from the interrupted review

The following observations are read-only. No task release, question answer,
run mutation or worktree removal was performed during this investigation.

- Task `20261023-003739-d4bc` retains review run
  `20261024-150755-3378` as its latest attempt and branch `magi/8da0/B` as its
  requested review source.
- The saved run has a six-hour resource wait budget (`21600` seconds).
  Its final resource block names `fix`, `shared build cache`, and exactly
  `the run is parking`, at `2026-10-24T09:01:51.597498Z`.
- Run events record cache waits at `06:15:36.729578Z` and
  `08:30:28.987015Z`. These are separate acquisitions: between them, the fixer
  completed round 1, and round 2 reviewed commit `72feeb1`.
  The final acquisition lasted about 31 minutes 23 seconds, not six hours.
- The saved run remains `reviewing`, `parked=false`; its resumed pass has no
  end time or end reason. Its driver record identifies the same process as
  the web/upgrade logs. Those logs identify the running binary as v0.192.0.
- At `09:01:51.695469Z`, `web.log` records the task being held after this run.
  At `09:01:55.494281Z`, `upgrade.log` records binary replacement, followed by
  HANDOVER signalling at `09:01:55.500114Z` and handover entry at
  `09:01:55.503568Z`.
- The later task hold describes a clean review worktree with HEAD and branch
  tip both at `72feeb1`, but refuses takeover because the previous run is
  considered to be worked on. This is historical evidence, not proof that
  its worktree or driver is idle at a future recovery time.

## Proven defects and remaining uncertainty

In v0.192.0, the cache wait returns cancellation when either pause handle
requests parking. The wave records `the run is parking`. `resource_halt`
then calls `stop_for_resource`, which deliberately avoids changing status
when parking is requested but does not call `park_here`. That early return
skips the durable park flag, pass ending and park cause. Queue settlement
therefore takes the resource-hold path instead of the parking path. The
old report and hold wording unconditionally called every resource block a
resource-wait timeout. That conclusion was unsupported for this record.

The v0.192.0 update handler calls `Ui::park_for_upgrade` before replacing the
binary. The near-immediate cancellation before binary replacement/HANDOVER
strongly supports update-driven parking. However, neither a direct request
record nor a saved `ParkCause` survives for this pass. An operator park or
interrupt racing the update cannot be excluded. The cache event names
ownership by running tools or process cleanup; it does not identify the
specific process that held those outputs. Increasing the resource timeout
would not repair either defect.

The current worker admission code has a separate reproducible self-conflict:
`current_work` projects a task worker reservation onto `task.runs.last()`.
Before a successor is created, that is still the previous run. Takeover
therefore sees the caller's own reservation as a live owner of the previous
run. Coalescing claims by task also loses evidence of other workers. Admission
now retains reservation identity and excludes only the confirmed current
process's exact running worker reservation. Anonymous legacy claims, other
workers, unknown ownership, driver identity and execution leases remain
protective. The holder is reloaded and checked under its owner lock.

A compatible latest unfinished review of the same branch now resumes in
place instead of attempting a new review takeover. Existing instruction,
review-budget and ownership checks still apply. The saved `reviewing` status
of this incident already passes `can_resume`; no review-budget relaxation is
needed. The saved authored instruction exactly matches the task, and one of
three review rounds is recorded (the round-2 fixer never started). The change
does not implement the task's original agent availability
feature.

## Reporting contract

`ResourceBlock.cause` is additive and defaults to `unrecorded` for old JSON.
New cache waits save a typed timeout with measured duration and limit, a
cancellation with the available park cause, or a separate infrastructure
error. Only exact known legacy cancellation details are interpreted as
cancellation; other legacy prose remains unrecorded. CLI output, the Web
report and daemon resource holds share `ResourceBlock::description`.
Verification badges name resource unavailability rather than assuming a
cache timeout. The `[resource]` hold prefix is preserved.

No-start evidence applies to the affected wave or command. It does not mean
that earlier reviewers, fixers or commands never ran. A cancelled acquisition
does not spawn the waiting agent, consume its budget, rotate it to a fallback
or create a quota failure. Parking uses the ordinary durable finalization and
is idempotent, so resource handling does not also settle a machine hold.

## Safe recovery of d4bc

Recovery is a separate owner-authorized action after installing the fix.
Do not delete a worktree, discard a run or answer the pending owner question
as a workaround.

1. Inspect current task/run history and the outstanding question; obtain the
   owner's recovery decision without silently resolving that question.
2. Confirm that run 3378 has no live or unknown worker, driver process or held
   execution lease, and that its owner lock can be acquired. Stop if any
   ownership evidence is live or cannot be established.
3. Confirm that the retained branch and review worktree preserve commit
   `72feeb1` and all later committed or uncommitted work. Verify that the
   approved instructions still match the existing run, or explicitly pin
   that run through the supported owner resume control.
4. Release/resume the existing queued owner through the supported task
   control path, pinned to run `20261024-150755-3378` where appropriate.
   The worker should resume that run and its existing worktree, not create
   another review run. Recheck the pending question and wait for its owner
   decision if the graph requires it.

The entire test suite and repository gate belong to the parent graph. Focused
regressions cover saved report evidence, typed timeout versus cancellation,
no-start acquisition failures, durable park finalization, refund routing,
matching review reuse and exact reservation exclusion.

Focused validation passed with `RUSTUP_TOOLCHAIN=stable cargo test --locked
--lib` using filters `resource_` (23 tests), `handover::tests` (12),
`graph::tests::driver_` (3), `admission_excludes_only`,
`starting_worker_projects`, `review_retry_resumes`,
`repository_contention_blocks_and_parks`, and `a_failed_driver_lease_record`
(one test each): 43 focused tests in total. Touched Rust files pass
`rustfmt --check`, and `node --check assets/ui/app.js` passes. A broader
`admission_` filter also selected the existing Web loopback-binding test;
that test could not bind its listener in the seat sandbox (`PermissionDenied`,
`Operation not permitted`). It needs the parent verification environment.

## Review follow-up: initial claims and persistent infrastructure errors

The in-memory first-claim exception did not survive a worker crash between
saving/linking a new run and claiming its driver. New run records now persist
`driver_claimed=false`, replaced atomically by `true` with the first pass and
process identity. Under the owner lock, initial admission requires that marker,
no contradictory driver/pass/activity evidence, no other worker claim and an
obtainable execution lease. A reloaded run can therefore retry its first claim,
including after an owner-lock wait failure. A missing legacy marker remains
unknown to the shared liveness judge, so queue preflight, handover, abandon and
approval dismissal keep refusing it. `claim_driver` itself refuses only a
proven-live driver or another worker's daemon claim: a legacy record with no
marker and no process identity can still resume, because the exclusive execution
lease taken immediately afterwards is what keeps a second driver out. Legacy runs
with matching pass metadata and a free execution lease are judged `Dead` from that
lease evidence.

Resource triage now checks the latest run's structured stop cause. Only a
recorded shared-cache timeout is automatically resolved by an available cache
lease. Infrastructure errors, cancellation, unrecorded causes, unreadable run
records and other resource kinds remain held for inspection; a free cache lease
does not demonstrate that a malformed manifest or failed metadata write was
repaired. This also makes legacy unclassified resource holds conservative.

The incident record was rechecked read-only during follow-up: it has
`driver_exited=true`, no retained driver PID and one saved pass. In the
absence of a competing live claim, this is already `Dead` under the shared
liveness policy, not `Unknown`. The proposed blanket relaxation to allow
unknown legacy owners is therefore neither required for this incident nor
consistent with preserving unknown ownership conflicts. Regression
`driver_admission_accepts_legacy_unknown_process_with_a_proven_free_pass_lease`
also confirms that the existing lease proof can authorize a resume without
setting `driver_exited=true` or trusting an incomparable process marker.

Follow-up validation passed with `RUSTUP_TOOLCHAIN=stable cargo test --locked
--lib` and filters `triage::tests` (29), `driver::tests` (11),
`graph::tests::driver_` (7), `a_run_resumed_mid_landing_reenters` (1), and
`resource_` (24, including one triage test already counted): 71 distinct
focused tests. Touched Rust files also pass `rustfmt --check` and the diff
passes whitespace checks. Full verification and the final gate remain
parent-owned. Staging was denied because the sandbox could not create the
Git index lock; changes remain in the candidate worktree for launcher capture.
