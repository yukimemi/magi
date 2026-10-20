# Retained worktrees and disk pressure

Retaining unfinished source work does not require retaining its compiler outputs.
The daemon's bounded retention sweep covers all readable runs in its store and
uses each run's repository and worktree-root snapshot. Git registrations identify
candidate, reviewer and unrecorded preparation worktrees; the candidate array alone
is never a deletion inventory.

## Disk policy

`[disk] worktree_limit_bytes` defaults to 10 GiB across retained worktree Cargo
outputs, separately from the existing shared-cache `cache_limit_bytes`.
`worktree_reclaim_bytes` defaults to 16 GiB per pass. A target larger than the
remaining budget is retained with a reason; raise the budget deliberately if
needed. `worktree_limit_bytes = 0` disables target eviction. The existing
`fold_grace_secs` (six hours by default) also protects recently used targets.
Oldest run activity is considered first. Low free space permits reclamation below
the cap, but does not override grace, ownership or the per-pass budget.

Only plain, canonical, registered worktrees are eligible. A legacy `target` must
be ignored, contain no tracked files, carry Cargo's cache tag and rustc identity,
and contain only recognized Cargo root entries (`debug`, `release`, the tag and
identity). These checks do not authorize recursive deletion. Cleanup removes only
regular artifact files declared as outputs in rustc dep-info (`.d`) records in
`debug`, `release` or their `deps` directories, with matching artifact names and
recognized binary/library extensions and compiler-artifact signatures. Text that
replaced a stale output remains. Dependency inputs never authorize deletion.
Canonical paths and plain-file checks exclude symlinks and traversal. Unknown
files, including notes anywhere under a profile, build-script outputs, incremental
caches and dep-info metadata remain. Unsupported or missing dep-info retains the
outputs for inspection. Custom locations and cross-compilation layouts are not
adopted automatically. The exact deletion list is recorded before removal in the
run's `build-outputs.json`; a Cargo tag alone never owns all ignored contents.
When the only remaining files are the cache identities, empty profile locks and
dep-info corresponding to the reclaimed artifacts, cleanup also archives their
exact contents per worktree in `build-residue/` under the run's reports.

Deletion requires confirmed driver death, no daemon claim, the public-owner lock,
the execution lease, an exclusive worktree-output lease and the Cargo profile
locks. Registration and target identity are checked again inside those locks.
A blocked or landing status does not prevent output eviction. Source changes,
branches, commits, run reports and recovery checkpoints remain untouched.
Unknown ownership closes cleanup. Do not run unmanaged background builds in
retained worktrees while performing maintenance: external tools must honor the
output lease as well as Cargo's own lock.

Managed seats (including read-only reviewers) borrow the worktree-output lease.
Verification borrows it too. Shared-cache output leases additionally prevent a
new cache borrower, prune or clear while process cleanup still owns tools from
a cancelled invocation. The independent process cleanup owner retains these
leases until the CLI and discovered descendants terminate. These protections do
not depend on optional repository coordination configuration. Automatic output
reclamation is disabled outside Unix process containment.
Managed tools also inherit the actual output and shared-cache lock descriptors:
loss of the launcher and its cleanup thread does not alone make those outputs
eligible for reclamation while a surviving tool retains its descriptors.

The sweep runs its filesystem walks, metadata loads and history inspection on a
blocking worker, keeping the async runtime available for heartbeats and claims.
Registration inventory is shared per repository within a pass. Logical sizes are
cached for five minutes in `output-measurements.json`; low free space bypasses
that cache, and reclaimed outputs invalidate their entry. Disabled output eviction
does not enumerate or measure targets. Deletion always revalidates registration,
ownership and current output lengths, regardless of cached inventory size.

Every 30 seconds of daemon polling, cleanup can reclaim inactive artifacts while
other runs are busy, then attempts shared-cache pruning through its ownership
protocol. Continued pressure asks active runs to checkpoint and park. Managed
seat and verification waits also measure their checkout/cache volumes every five
seconds: below half `min_free_bytes`, or when measurement fails, they stop their
owned process trees and record a disk resource hold. Partial source changes are
kept for recovery; implementation rescue still runs. New starts and resumes
remain subject to the ordinary free-space gate. A zero floor disables monitoring.
Post-spawn interruption carries a typed resource error and durably marks the
implementation for snapshot/rescue before entering the hold. A failed rescue
keeps that barrier on disk; resume retries persistence before dispatching seats.
An interrupted turn with no source change remains pending, rather than becoming
a completed empty candidate. Its durable `interrupted_implementations` checkpoint
survives rescue failures and reloads; resumption dispatches the same seat again.
Admission waits that never started a process do not mark candidates completed.
The reserve and cadence are safeguards, not a filesystem quota: unrelated
writers and sudden allocations can still exhaust a volume between measurements.

## Recovery lineage and obsolete attempts

`[disk] retained_attempt_limit` defaults to eight unretired, unreleased run
records per task. Before creating a competition or review run, the daemon
deduplicates the task's history and holds when this bound is reached. Unknown
records also hold admission. Completed labels, empty candidate arrays, changing
upstream commits, refunds and even explicit releases do not prove retirement or
reset this count. The bound does not prevent resuming an existing owner, release
a hold, choose a source or delete anything. A selected review source is preserved
when admission holds. Verified retirement or ownership transfer frees a slot;
an operator can deliberately change the configured limit (`0` disables it).
This caps accumulation even when input changes create new failure generations.

Before candidate creation, a durable `prep-intent` event records the planned
branch, worktree, starting commit and cherry-picks. A creation failure or crash
before candidate registration therefore leaves an inventory for diagnosis.

`Task.recovery_budget` records failures against authored instructions, fetched
base, resolved reference identities and candidate patch fingerprints. Branch
containment prose, repeated history entries, automatic conductor requeues and
machine triage releases do not replenish it. Equivalent patches under different
commit identities are unchanged work. A changed input or patch starts another
generation; an explicit operator release approves another generation. Switching away from inputs and back retains their previous failure count. The daemon
holds exhausted generations before minting a new run, using its attempt limit.
Quota/resource waits and intentional parks do not spend this failure budget.

The sweep deduplicates all task history and establishes recovery authority from
one explicit public-head selection, otherwise one existing PR owner, otherwise
one useful historical candidate run. Multiple distinct sources without a unique
authority, unreadable history and unanswered task decisions retain the backlog.
Creation time is never a reason to prefer an empty newer attempt. Every selected
lineage durably records the full source-run set in `recovery-lineages` before
retirement. This inventory preserves other candidate changes as recovery sources;
it does not merge or discard them or transfer PR ownership.

At most five obsolete empty attempts are retired per pass. Each removed worktree
must still be clean, contain no untracked or ignored files except archived Cargo
residue, have HEAD exactly at
its original base, and have no unfinished Git operation. Recorded and unrecorded
candidate branches must also be at that base. Changed worktrees, unique commits,
PR owners, review/recovery checkpoints, explicit retention markers, operator pins,
selected replacement sources, pending candidate-preservation barriers and
unanswered run decisions are kept. A task's hold
is not released or completed. No PR is closed and no question is answered.
Cargo residue qualifies only after artifact reclamation recorded a complete
inventory, and only if every remaining file still matches that inventory under
the output and Cargo locks. Added files, changed metadata, symlinks and missing
proof retain the worktree. The archived metadata survives worktree removal.

`retired_to` prevents execution independently of run status, before any worktree
is removed. Reports, branch refs and the retirement event remain. Interrupted
removal can retry its proof and finish; it never revives the obsolete attempt.
On ordinary release, resume follows `retired_to` to its recorded authority within
the task's history, checking repository identity, cycles and resumability. If the
authority cannot be validated or no longer matches the task's authorized
instructions, the task is held rather than starting a replacement competition.
Explicit fresh competitions and selected public-head recovery retain their normal
precedence. The ordinary fold path preserves every unmerged source still referenced by an
unfinished queue task and reports the task and refusal reason; explicit folds
return an error and automatic folds log the reason. Retirement of a proved-empty
attempt uses its own proof.
The existing `Superseded` status is not used to retire unfinished work because
it has separate settlement and publication effects. Historical recovery sources
retained by an earlier explicit recovery stay retained, even if that means a
backlog needs a later owner decision rather than automatic deletion.
History inspection and Git proof run without holding the task write lock, so
owner updates and run links can be saved during slow inspection. Immediately
before committing retirement, a short write critical section re-reads the task
and requires the entire snapshot to be unchanged, rechecks owner pins and open
questions, and persists the lineage and retirement flag. Any intervening update
retains the worktrees for a fresh pass. Deletion happens after that lock is
released, under the run/driver/output leases and the durable retirement fence.
Age alone still cannot revoke a live task writer; unknown process liveness
retains its lock. Old identified locks require confirmed process death.

Ordinary folding of a confirmed `Merged` run with a successful merge record
does not wait for an unfinished queue task to settle, but its merge label is only
admission to proof. Before deleting anything, every targeted candidate branch,
worktree HEAD, HEAD/branch reflog and ORIG_HEAD must be incorporated in a single
pinned fetched base. Independent losing candidates and detached reviewers retain
the entire run. Git operation checkpoints, source changes and unknown ignored
files also retain it; a target directory needs the existing Cargo residue proof.
For a squash merge, the recorded winner's complete cumulative binary patch
(including modes and blob IDs) must exactly match a single-parent commit in
the pinned fetched history since the run's base commit. Only ancestors of that
proven winner qualify through this proof; independent losing commits and
rewritten-away checkpoints still retain the entire run. Later base changes do
not invalidate a matching historical squash. Missing base information or an
unproven squash/rebase retains sources for inspection; conflict resolutions or
base drift that change the patch require manual inspection. Explicit
recovery-source retention and pending implementation
preservation still refuse folding. Other terminal labels and successful PR
publication alone do not establish a confirmed merge.
Queue completion does not authorize folding historical sources either:
`Done` tasks retain their historical `Superseded` and `Failed` sources until
recovery lineage transfers them or a confirmed merge passes the source proof.
This includes failed candidates with no recorded tally. A newer successful
attempt alone does not prove that an older independent implementation landed.

## Observability and incident evidence

`retention.json` records measured logical target bytes, logical bytes removed,
retirement count, retention reasons and failures. It is exposed in disk health
and `magi cache show`; tracing records pass totals and detailed reasons. A
persistent cap failure raises a deduplicated notice. Logical lengths are not a
promise of physically reclaimed bytes on copy-on-write filesystems.

Investigation compared upstream `9fad4a6` with the implementation base; only the
release version differed. The earlier incident had 108 retained attempts and
only the first two recorded candidates. Later inspection found 218 history
entries but 109 distinct runs, and a later useful candidate: historical counts
are snapshots, not deletion authority. The repeated empty attempts recorded the
same reference cherry-pick conflict during preparation at the same upstream base.
The conductor's fresh Requeue calls `Task::requeue` then `release`, resetting
`attempts`; therefore the ordinary per-task attempt limit could not bound that
loop. Machine triage also calls `release`, but the preparation conflict is not a
resolved disk/cache hold and does not establish triage as this incident's cause.

Upstream already includes `f817610`, which records preparation failures, preserves
incomplete candidates and requires an explicit preparation retry/public-head
recovery, plus later operator-resumption protections. The durable failure budget
adds a bound across fresh automatic attempts and ordinary execution failures.
No historical task or worktree is modified by the implementation tests. No
operational cleanup task is created.

The continuation checked the launcher-pinned upstream `4a3aa69` without moving
the base. Its preparation failure hold and explicit retry protections are
already present. Read-only inspection again found 218 history entries, 109
distinct runs, three candidate-bearing runs, and 106 occurrences of the same
referenced-commit cherry-pick failure. The two old useful candidates and the
later candidate all remain recovery evidence. A completed task label does not
authorize discarding that work. No local historical run was changed.

Focused regression verification:

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib retention::tests
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib cache::tests
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib retained_attempt_bound
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib preparation_records_creation_intent
```

Full review and verification remain owned by the enclosing run, including
`cargo test --locked --all-targets` and `cargo make check`.
