# Inactive checkout evidence

Normal retained-output housekeeping also archives and evicts a small set of
inactive, non-resumable source copies when `disk.auto_fold` and a positive
`disk.worktree_reclaim_bytes` are enabled. This is independent of confirmed-merge
folding, task retirement, and incremental cache reclamation. Task history,
review rounds, retention markers, candidate branches and run records remain.

The initial scope is detached `review-N`, `judge-N`, and `advisor-N` copies with
recorded base/review HEADs, plus empty Superseded candidate attempts whose actual
branch, HEAD, index and files still equal their original base. Changed
implementation candidates and resumable runs remain materialized. Public, selected recovery,
approval and explicit source-retention authority blocks eviction. Unknown run
or task records, cross-run references, live owners/drivers, borrowed output
leases, recent activity, Git operations/local refs, unknown administration,
unique recovery history, dirty/index-hidden edits, ignored/untracked entries,
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
All HEAD reflog and ORIG_HEAD tips admitted by preflight are independently
archived, without walking or bundling the repository's entire history. The
archive is checked in a separate repository without alternates: both trees
must materialize and hash correctly, and their diff/index must match. Publication
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
Truly empty, inactive, unborrowed roots without Git registrations are removed
with `remove_dir`, without retiring the run or deleting any records.

To inspect or reconstruct a specific archive from an eviction record:

```sh
magi source-evidence <archive-directory> > saved.patch
magi source-evidence <archive-directory> --restore <new-inspection-directory>
```

Both commands validate the archive first. Restore requires a new destination
and produces an independent inspection repository with the source index and
files. Original commit metadata and complete base/source trees remain in the
archive; restoring does not select a recovery source, resume a task, change a
public branch, or grant approval. Implementation sources requiring automatic
resumption remain protected.

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
