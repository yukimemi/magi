# Inactive checkout evidence

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

The pass visits at most 64 runs, oldest first with a rotating continuation,
and evicts at most five copies. New evidence has a 64 MiB per-pass budget and a 64 MiB per-run archive bound
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
