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
identity). Symlinks and unknown entries are retained. Other ignored directories,
custom target locations and cross-compilation layouts are deliberately not
adopted automatically. Adoption/deletion is recorded in the run's
`build-outputs.json`. This is permission to evict that generated Cargo tree,
not permission to delete arbitrary ignored files elsewhere in the checkout.

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

Every 30 seconds of daemon polling, cleanup can reclaim inactive targets while
other runs are busy, then attempts shared-cache pruning through its ownership
protocol. Continued pressure asks active runs to checkpoint and park. Managed
seat and verification waits also measure their checkout/cache volumes every five
seconds: below half `min_free_bytes`, or when measurement fails, they stop their
owned process trees and record a disk resource hold. Partial source changes are
kept for recovery; implementation rescue still runs. New starts and resumes
remain subject to the ordinary free-space gate. A zero floor disables monitoring.
The reserve and cadence are safeguards, not a filesystem quota: unrelated
writers and sudden allocations can still exhaust a volume between measurements.

## Recovery lineage and obsolete attempts

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
must still be clean, contain no untracked or ignored files, have HEAD exactly at
its original base, and have no unfinished Git operation. Recorded and unrecorded
candidate branches must also be at that base. Changed worktrees, unique commits,
PR owners, review/recovery checkpoints, explicit retention markers, operator pins,
selected replacement sources, pending candidate-preservation barriers and
unanswered run decisions are kept. A task's hold
is not released or completed. No PR is closed and no question is answered.

`retired_to` prevents execution independently of run status, before any worktree
is removed. Reports, branch refs and the retirement event remain. Interrupted
removal can retry its proof and finish; it never revives the obsolete attempt.
The ordinary fold path also preserves every source still referenced by an
unfinished queue task; retirement of a proved-empty attempt uses its own proof.
The existing `Superseded` status is not used to retire unfinished work because
it has separate settlement and publication effects. Historical recovery sources
retained by an earlier explicit recovery stay retained, even if that means a
backlog needs a later owner decision rather than automatic deletion.

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

Focused regression verification:

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib retention::tests
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib cache::tests
```

Full review and verification remain owned by the enclosing run: review with
`--merge none`, then `cargo test --locked --all-targets` and `cargo make check`.
