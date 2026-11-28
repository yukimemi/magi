# Inactive checkout evidence

## Completed read-only seats

Normal graph teardown (including preparation rollback and explicit folding)
discards uncommitted and untracked changes in owned detached `advisor-N`,
`judge-N` and `review-N` worktrees. Their outputs are replies and saved run
artifacts, not checkout files. The seat must be registered in the run's root,
belong to the expected Git common directory, and not be a candidate.
Discarded status entries are recorded in bounded `fold` events. Actual Git
removal failures still stop teardown.

Editing seats, attached branches and unknown trees retain clean-only removal:
their uncommitted work is never silently discarded. Existing execution,
coordination and output-lease protections remain in effect. The independent
inactive-copy eviction policy below remains conservative about dirty files.

Explicit `magi fold <run-id> --override-recovery-retention [--all]` is separate
from inactive-copy eviction. For a finished, non-resumable run with a proven
stopped driver it may bypass queue lineage and explicit source-retention markers
only. All target preflight and unincorporated-tip archiving in
`artifacts/fold-evidence` finish before a durable operator override event and
deletion. Dirty sources, leases, Git checkpoints, evidence limits/failures and
unknown liveness still refuse. Run records and markers remain; housekeeping
never uses this override.

## Inactive-copy eviction

Normal retained-output housekeeping also archives and evicts a small set of
inactive, non-resumable source copies when `disk.auto_fold` and a positive
`disk.worktree_reclaim_bytes` are enabled. This is independent of confirmed-merge
folding, task retirement, and incremental cache reclamation. Task history,
review rounds, retention markers, candidate branches and run records remain.

The scope is registered detached `review-N`, `judge-N`, and `advisor-N` copies,
plus empty Superseded candidate attempts whose actual branch, HEAD, index and
files still equal their original base. Complete snapshots prove detached seat
HEADs, including tips unrelated to the base or absent from review rounds.
Changed implementation candidates and resumable runs remain materialized.
Public/merged, approval, interrupted-preservation and recovery metadata retain
implementation authority independently of disposable read seats. Run IDs in
recovery selections alone do not borrow a read seat. Explicit source-retention
markers, owner questions, task resume pins/replacements and unfinished seat
answers still block eviction. Unknown run
or task records, concrete path references (including the owning run), live owners/drivers, borrowed output
leases, recent activity, Git operations/local refs, unknown administration,
unarchivable recovery history, dirty/index-hidden edits, ignored/untracked entries,
symlinks and even unknown empty directories block deletion. Build outputs are
never archived; a copy containing any such leftovers is retained.

The pass inspects at most 64 run roots and skips at most 64 proven absent roots
without eviction records, using separate budgets. Each visited run checkpoints
the census, so both budgets bound full-census writes even with thousands of
missing roots. It inspects at most five disposable seats, including unsuccessful
inspections. Missing and empty roots never spend seat slots.
Runs use immutable ID order, not `updated_at`:
ordinary bookkeeping cannot move the continuation. A cycle freezes its run IDs
and upper ID; newly added runs join the next cycle. Deleting a queued run records
an error and advances. A seat-budget interruption resumes after the last inspected
seat in the same run. Failures advance once per cycle, so a consistently failing
archive cannot starve later seats. A busy output lease protects its own seat,
without preventing independent seats in the run from being inspected.

`source-eviction-cursor` is an atomically saved, versioned census and continuation,
with the current cycle and the preceding completed cycle only (32 MiB maximum).
It records run root identities, discovery state, per-seat outcomes, stages,
reasons and observation times. Until a root is discovered, its run entry explicitly
says `not-yet-inspected`; the seat inventory is not yet known. Discovered seats
awaiting the seat budget each have their own `not-yet-inspected` entry. A completed
cycle covers the frozen run list, not newly created runs or checkouts added behind
its continuation. Root identity changes are errors until the next cycle.
Unregistered or noncanonical discovered seats remain protected. Missing roots
still require pending-ledger reconciliation; absence alone proves no eviction.
Legacy valid run-ID cursors start a new census because they prove no completed
coverage; malformed/unknown cursors fail closed and are never silently reset.
Checkpoint failures stop the scan and appear as failures rather than being ignored.
Observations are historical diagnostics and never reusable deletion permission.

`source-eviction-status.json` and `retention.json`'s `source_scan` report the
executing version, effective source-eviction switches, last attempt, cycle position,
completion and global ownership/scan errors. Separate
`source-eviction-{housekeeping,disk-pressure}-{dispatch,worker}.json` receipts
record scheduler/worker versions, configuration where available, cadence,
active-worker/queue skips and configuration/launch errors. Idle housekeeping and
active disk-pressure services share the same persistent scan. Their receipts do
not assert that a still-running service has finished; use the scan observation
time and completion field. `magi fold <run-id> --show-seats` includes these receipts
and both cycles' run/seat observations, even for runs with no evicted seats.
A global validation error prevents inspection and is distinct from an observed
seat protection. Do not interpret zero evictions or one bounded pass as a census.

### Finished-run folding on a busy machine

Idle housekeeping (`clean::fold_due`) is admitted only with no busy worker and no
runnable task, so a queue that is never empty folded nothing (issue 1275: 73 merged
runs, days past `fold_grace_secs`, on a machine whose housekeeping receipt always
said "active work or runnable task queue"). The idle gate dates from the first
janitor ("folding worktrees and pruning a cache mid-build would race the very
compile"); that concern belongs to the shared-cache prune, orphan reclamation and
worktree-registration pruning, which stay idle-only. Folding a finished run takes
the run's owner lock, execution lease, per-worktree output leases and the
repository coordination lease inside `graph::fold_run_when`, and
`clean::eligibility` refuses anything a live claim holds, so the `fold` service runs
exactly where housekeeping does not (`daemon::fold_turn_eligible`), under the same
decision function as the idle pass (same grace, resumable, live, recovery-source
and preservation checks).

**The record under the locks.** The fold reloads the run record after it holds the
owner lock and execution lease, and acts on that record only; there is no fallback
to the caller's earlier read. The reload must succeed and carry the locked id: a
record deleted meanwhile (or a foreign one) ends the fold as `record_gone` and a
record that no longer parses as `unreadable`, with nothing touched and nothing
recreated (the fold checks the record exists before it creates a lock file, and
removes the lock files and directory again if the deletion landed in between). A
handover that completes between the janitor's first read and the locks records
`released_to` / `released_branches` and removes the old worktree while keeping the
branch; the reload sees it, so the handed-over branch survives and the successor's
ownership record is not overwritten. The automatic callers (idle pass, busy turn)
read the record as saved, whatever schema wrote it, and re-ask the eligibility
decision on it (a run that became resumable, or live, is left alone); `magi fold`
and the phone route read it with the migrating loader they used themselves and
apply the same preservation checks to that fresh record.

Every other writer of the record takes the same two locks. Deleting a run (`magi
run rm`, the phone's delete button) takes the owner lock and execution lease
without waiting, reads the record under them and judges the unfolded-candidate
guard on that read: a fold in progress makes the deletion a refusal ("try again
later"), so a record cannot disappear under a fold and its worktree suffix cannot
be reserved by a new run while the fold removes that suffix's directories. The
abandoned-seat cleanup that `magi fold` and the phone run after a fold that removed
nothing writes under the locks as well, from a fresh read, so it cannot erase a
handover that landed after the fold released its own locks.

One `service:daemon-fold` worker runs per pass (at most every 120 s, never beside a
housekeeping worker). A pass judges at most 64 run records, starts at most 4 folds
(charged when the fold begins, whatever it returns) and starts no further fold after
5 minutes (a started fold finishes); at least one run is always judged. Judging
reads files (`run.json`, the live-claim sources and, for an eligible run, the queue
for recovery sources), so its wall time depends on the disk; the time budget covers
it as well as the folds.

**Cycles.** `<home>/fold-turn-cursor` holds `{cursor, high_water}`: the pass works
through the run ids after `cursor` up to `high_water`, the newest id present when
the cycle began, and removes the file when it has judged the last one; the next
pass starts a new cycle with a new high-water mark. With N records at the start of
a cycle, every one is judged within `ceil(N / 64)` passes plus the passes its fold
invocations consume (`ceil(F / 4)` for F invocations, plus one for each pass the
time budget cut short), and a run that left its grace after being judged is folded
within the next cycle. Run ids start with the local wall-clock time of creation, so
runs created after the mark sort after it and wait for the next cycle; a run created
in the same second as the mark, or after the clock stepped backwards, may join the
current cycle. Each such run adds at most its own judgement, and that is the whole
bound on them - no list of the cycle's members is kept. A cycle file that is
unreadable, names an id that is not a run id, has a `high_water` newer than every
run, a `high_water` below the cursor, a cursor naming a run that no longer exists,
or an empty range starts a clean cycle (`cycle_restart` says why). Every pass lists
`runs/` and checks each record once (O(history) syscalls); that inventory is counted
in `elapsed_ms`.

**Runs that were folded long ago.** A fold that completes under its locks having
found nothing left to remove, with the run's worktree bay gone (`try_exists` answered
`Ok(false)`), records that in `run.json` (`fold_settled_at`; absent on every older
record means unknown). The busy turn skips such a run without starting a fold or
spending the fold budget - counted `already_folded` - only while the mark is present
and the bay is still confirmed absent by a fallible check: a failing check, or a bay
that exists even empty, means the run is judged normally and the fold is invoked, and
a fold that finds work clears the mark in the same locked save. The mark says nothing
about branches or Git registrations: a stale registration or a branch without a bay is
not disk and is left to the ordinary paths. The idle `fold_due` never skips.

`source-eviction-fold-{dispatch,worker}.json` carry the skip reason (cadence, idle
machine, `auto_fold` off, unreadable startup configuration, a sibling folding
service) and, for a completed pass, a `fold` object: `records`, `inspected`,
`invoked`, counts `folded`, `nothing_left`, `already_folded`, `live` (a daemon
claim, or an owner lock, execution lease, output lease, Cargo build or repository
lease held - contention only; any other failure of a lock call is `failed`),
`resumable`, `within_grace`, `recovery_retained`, `proof_incomplete` (startup,
candidate preservation or the confirmed-merge source proof: Cargo residue, Git
checkpoints, the fetched base), `unreadable` (at the first read or under the locks),
`record_gone` and `failed` (git or filesystem errors of the deletion, or a lock call
that failed for a reason other than contention), the `cursor` and `high_water`, why
the pass `stopped`, `cycle_restart`, `cursor_error` if the cycle could not be saved
(the next pass then repeats the slice), `elapsed_ms` and `notes`: the scrubbed,
clipped text of up to eight `proof_incomplete` and `failed` runs. The turn owns no
deletion authority of its own: it never sweeps retained outputs, copies or orphans.

On a platform without Unix process containment, `retention::sweep` records its
refusal ("Automatic output reclamation requires Unix process containment") in
`retention.json` like any pass, and the housekeeping and disk-pressure worker
receipts carry the report's failures under `retention` (state `retention swept`).
The report is staged in a uniquely named file created exclusively and renamed over
`retention.json` (the refusal takes no `retention.lock`, and two passes may publish
at once); a staging file of this report older than a day is removed when a report
is published.

The former timestamp-ordered run-ID continuation could skip seats when records
moved across the cursor; a budget stop also marked an entire partially visited run
as passed. Isolated historical fixtures reproduce these progress failures. This
does not establish which failure or legitimate blocker explains every production
checkout: follow a completed census and each remaining seat's observation before
making that claim. No production checkout/store changes are required to diagnose
coverage with these records.

New evidence has a 64 MiB per-pass budget and a 64 MiB per-run archive bound
including incomplete publication products;
archives additionally bound expanded object bytes, object count and checkpoint
count. Ownership inspection rejects records above 16 MiB, run histories above
4,096 records or 256 MiB, and task histories above 4,096 records or 64 MiB.
Supported historical schemas load through normal migrations without rewriting
the store; unknown nested source/authority fields block eviction. Limits retain
the copy rather than dropping evidence. Source unlinking
has a separate per-pass logical-byte bound using `worktree_reclaim_bytes` (one
copy can exceed the remaining budget, after which the pass stops).

Each `artifacts/source-evidence/<head>-<base>/` contains complete base and source
trees/blobs in a non-thin pack, original commit metadata, the same binary diff
format used by fold evidence, and a raw NUL-delimited path/mode/blob index.
All HEAD reflog and ORIG_HEAD tips, including unrelated earlier tips, are independently
archived with the required commit/tree/blob closure of each tip. When HEAD
descends from the run base, an immutable `refs/magi/source-anchors/<base>` pin
retains the shared base history through GC. Version 2 snapshots exclude that
history with `^<base>` and record the boundary; unrelated heads still require
their complete independent closure. The pin never moves or replaces an existing
ref and is not removed while archives may depend on it. Shallow, borrowed
alternate, promisor and noncanonical object stores cannot authorize a pin.
Candidate/operator
branches and run/task records are unchanged. Closure enumeration and expanded
objects share the existing 4,096-object and 64 MiB limits; history beyond the
retained boundary exceeding either limit retains the checkout. The
archive is checked in a separate repository without alternates: both trees
must materialize and hash correctly, and their diff/index must match. Applying the
archived binary patch to the base in the inspection repository's private index
must reconstruct the recorded source tree. Every checkpoint archive is validated
again immediately before source removal. Publication
syncs files and directories before an atomic rename. Existing evidence is
immutable and revalidated, including after Git garbage collection.

`artifacts/eviction-<seat>.json` preserves the source/administration inventory,
raw administration contents, checkpoint archive locations, and the pending or
evicted phase. Evidence publication and a durable pending record precede clean
Git worktree removal. Retries reprove remaining sources. Missing sources only
complete a pending record when evidence validates and Git registration is also
absent. A missing-but-registered checkout can finish only when its entire
remaining Git administration exactly matches the archived contents under an
exclusive output lease. Missing/corrupt evidence or changed/unknown
administration remains a reported blocker. A changed or recreated checkout is not silently adopted.
Re-materialized seats use subsequent `eviction-<seat>-<generation>.json` records,
up to 128 generations; completed generations are never overwritten. Version 2
records pin their original base independently of later run base updates;
version 1 records infer and validate it from their immutable evidence identity.
Existing restored seats may follow recorded review/base checkpoints, while
unrecorded heads and unknown working files stop execution.
Truly empty, inactive, unborrowed roots without Git registrations are removed
with `remove_dir`, without retiring the run or deleting any records.

A successor's lineage ID reference alone does not borrow a checkout. Explicit
implementation recovery selections still protect implementation checkouts.
Concrete path references name the owning run/task and field in retention reasons;
continuation leases and resume contracts have separate reasons. Unreadable ownership records still block
source eviction because they may hide a borrower; other housekeeping continues.

To list archived seat tips or reconstruct one by its full recorded head:

```sh
magi fold <run-id> --show-seats
magi fold <run-id> --restore-seat <head> --dest <new-inspection-directory>
```

The list includes the source name, head, tree, patch bytes and eviction phase,
including independently archived historical tips. Both operations validate
record identities and every referenced archive, and work after the original
repository has disappeared. They do not run folding or retire the run.

To inspect or reconstruct a specific archive from an eviction record:

```sh
magi source-evidence <archive-directory> > saved.patch
magi source-evidence <archive-directory> --restore <new-inspection-directory>
```

Both commands validate the archive first. Restore requires a new destination
and produces an independent inspection repository with the source index and
files. Original commit metadata and complete base/source trees remain in the
archive; restoring does not select a recovery source, resume a task, change a
public branch, or grant approval. Inspection restoration also reinstalls the
original HEAD. An anchored independent inspection explicitly declares its base
as a shallow boundary; it has no alternates or dependency on the original repo.
Execution import requires the exact retained pin and complete shared base ancestry
before writing objects, and never changes the shared repository's shallow state.
A missing, moved, symbolic or symlink pin fails closed before import. Full
independent snapshots still restore complete ancestry through the original tip.
Legacy tree-only archives still restore files/index and expose diffs; when their
commit ancestry is incomplete, inspection HEAD remains unborn and execution
import is rejected before writing into the shared repository. The original base
commit remains archive metadata unless it belongs to the proved HEAD closure.
Independent connectivity checks precede eviction and import, including retries;
old immutable evidence is never rewritten to fill missing ancestry.

Execution has a separate restoration contract before graph dispatch, prep shortcuts
or continuation delivery: under owner/driver and repository coordination leases,
validate the latest generation, import complete objects without touching the
repository index or refs, recreate the original detached worktree, and run its
normal setup. This covers explicit reentry of terminal runs after GC. Resumable
runs remain protected rather than relying on this to recover persistent sessions.
An existing changed/unregistered/noncanonical entity, pending eviction, corrupted
archive or interrupted setup stops execution for inspection. A durable
`restore-<seat>.pending` marker fences incomplete restoration; it is removed only
after setup succeeds. Restoration never selects an implementation or changes
recovery/approval authority.

The structured run report has an “archived seat diffs” section. Its run-scoped
`seat-evidence` API lists validated heads and serves their archived binary diffs;
no repository, checkout or live Git object lookup is needed. Unknown heads and
corrupt archives fail closed.

`retention.json` distinguishes source logical bytes unlinked, estimated
allocated file blocks unlinked, evidence logical/allocated growth, copies
evicted, and empty roots removed. Source allocation counts regular checkout
files, not Git administration or directory blocks, and discounts hardlinks
whose final link was not removed. Open handles, CoW/shared extents, filesystem
compression and concurrent writes mean these are allocation estimates, not
claims about physical free-space gain. The observed home-volume free-space
change remains separate and cannot be attributed solely to this pass.

Focused fixture checks:

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib source_eviction
```

These fixtures exercise normal housekeeping with historical review rounds,
binary changes, deletion and executable modes after deleting refs and Git GC,
restore without the original repository, ownership/content protections,
interrupted archive publication/removal, corrupt archives and unchanged task
lineage. They never reclaim operator worktrees.
