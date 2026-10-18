# Notification candidate recovery

This recovery belongs to task `20261017-183905-871c`, original run
`20261017-190425-05eb`, seat `impl-A`. It does not replace that task's ownership,
approve any merge, or authorize discarding its other candidate or pull request.

`provenance.json` records the original base, transcript SHA-256, ordered edit
command hashes, original command exit statuses, and reconstructed file list.
`reconstructed.patch.gz` contains the reconstructed changes against that base;
its uncompressed SHA-256 is recorded in the manifest. All 22 changed/new paths
match the transcript's final status listing (`item_211`). The changes are also
included in this recovery candidate's source for normal parent-owned review.
The infrastructure changes in this candidate are additional to that patch.

Reconstruction replayed the recorded Python edits and file creation in command
start order, including the Python portion of `item_22`: its subsequent test
failed, but the edits completed. Recorded formatting commands were replayed on
those files. No missing edit was inferred from the final summary. This is a
reconstruction, not a recovered original commit or a claim of byte-for-byte
identity with a vanished worktree. The source transcript's test results are
historical evidence, not validation of the reconstructed tree.

## Evidence and separate failure boundaries

- At inspection, the task was running and the original run was reviewing B,
  with A recorded empty, zero commits and folded. A's worktree and branch were
  absent; B's local tip was `e7dec87365047220012fd76b58b3f9bd0637d7ce`.
- The transcript records failure creating the linked worktree's index lock
  with `Operation not permitted`. The original CLI could edit source but could
  not stage or commit it. Shared Git-directory permissions had already been
  implemented in `agent::worktree_writable_roots` and documented in
  `docs/seat-write-scope.md`. That directory includes linked worktree metadata.
  No duplicate sandbox grant or bypass is introduced here. The deployed launch
  arguments and enclosing host policy were not captured in the durable run
  artifacts, so the precise host-side denial remains unproven.
- The implementation collector swallowed rescue errors, then substituted zero
  commits and empty diffs for failed Git reads. This independently permits
  uncommitted work to be recorded as no change. The original parent's rescue
  error itself was not logged, so its precise failure cannot be reconstructed.
- The saved events record A as no change at `10:55:30Z`, then a park at judging
  for upgrade `0.155.0` to `0.155.1`, then folding A at `11:38:18Z`. Cleanup used
  forced removal and ignored failures. This establishes chronology and an
  unsafe deletion route, not proof that the upgrade itself destroyed data.
- Read-only refs/worktree metadata and reflogs yielded no surviving candidate
  A commit. A `git fsck --no-reflogs --unreachable` inventory contained 5,818
  objects, including 1,561 commits; none of those commits directly descended
  from the recorded base. No original candidate tree was identified. This is
  not a proof that every historical blob or external backup has been exhausted.
- `magi task hold 871c` was refused because the daemon owns the task claim.
  No claim/queue/run files were edited. GitHub PR lookup failed because the API
  was unreachable; current remote PR state has not been verified.

## Recovery proposal and stopping boundary

There is no supported CLI to attach an imported implementation to an existing
run's candidate list and invalidate its prior uncontested tally. `recover-pr`
selects an approved public head; it is not a candidate-import operation. Merely
resuming the old binary would continue the already chosen B review.

1. Keep the existing notification PR unmerged. The operator can park its owning
   loop at a safe node boundary through the existing UI. This affects other
   in-flight runs too, including this recovery, so it was not invoked from this
   candidate. Once the original task claim is released, use
   `magi task hold 871c` to retain the original task while arranging recovery.
2. Review this recovery candidate, including the reconstructed notification
   changes and the provenance manifest, through its parent run. The parent runs
   the full repository gates. Do not independently publish or land the original
   competing branch, and retain the owner's merge approval requirement.
3. To resume the original competition safely, add a separately reviewed,
   supported import operation: require an idle held original task and its claim,
   validate a pinned recovery commit against the recorded base, retain source
   artifacts and old decisions, register the recovered candidate transactionally,
   invalidate the uncontested tally and dependent review/approval records, then
   rejudge both candidates and review the resulting notification work. No such
   import was performed here. Without that operation, attaching this commit by
   editing `run.json`, rewriting candidate refs, or fabricating a review finding
   is the unsupported mutation at which recovery must stop.
4. If reconstruction completeness is unacceptable, explicitly rerun the original
   implementation seat using that supported recovery operation rather than
   silently substituting a different candidate. Do not release/requeue the
   original task in a way that abandons its existing PR ownership.

The recovered source is durable and reviewable in this candidate. Its association
with the original competition remains a deliberate unresolved recovery step;
this document does not claim that the original run has been repaired.

## Preservation behavior

Completed replies are saved before rescue or Git inspection. Pending candidate
preservation is persisted separately from empty/failed implementation status.
All candidates are collected before preservation errors block the graph. Resume
retries preservation without re-running completed seats. Git and patch-write
errors remain errors. Schema 15 makes older binaries refuse these records.

Best-effort recovery artifacts preserve staged and unstaged binary patches,
status, and untracked regular-file bytes without writing the index. The first
snapshot is retained across retries. An incomplete snapshot is reported;
untracked symlinks require manual recovery and are never followed. Snapshots
are supplemental evidence and never authorize deleting a dirty worktree.
Normal folding requires clean successful removal before marking a candidate
folded. Orphan/unreadable cleanup retains unknown or dirty content and recovery
artifacts. Migration inspects only unfolded legacy empty records; already
confirmed schema-15 results and intentionally folded candidates are not re-flagged.
If recovered work changes a previously judged candidate set, the run stops with
its existing decisions and recovery evidence retained for a supported recovery
operation. Partial loser cleanup saves each completed candidate and safely
retries missing worktrees or already removed branches.

## Commit boundary in this recovery seat

The recovery seat itself reproduced an `Operation not permitted` denial creating
its linked worktree index lock during `git add`. No staging or commit succeeded;
no permission bypass was attempted. The outer execution environment must secure
these changes before any cleanup. `implementation.patch.gz` captures all tracked
source modifications plus the new notification UI test; its uncompressed hash is
in `implementation.patch.sha256`. Keep this recovery directory too, since that
patch deliberately excludes the provenance files and its own archive.

This reproduction confirms a live Git-metadata permission problem despite the
existing common-directory grant. It does not identify which enclosing sandbox
rule denied it or prove the original parent's rescue had the same cause.

## Validation of this reconstruction and fix

All commands used the stable toolchain and shared build cache. These focused
checks passed; the parent owns the full test suite and repository gate:

- `cargo check --locked --bin magi`.
- `cargo test --locked --lib candidate_preservation -- --nocapture`: 2 passed,
  covering rescue/index-lock failure, other-candidate collection, staged and
  untracked evidence, Git-read failure, patch-write failure, cleanup refusal,
  upgrade-cause save/load, and persistence-only retry.
- `cargo test --locked --lib clean::tests::fold_ -- --nocapture`: 9 passed.
- `cargo test --locked --lib graph::tests::fold_run_ -- --nocapture`: 3 passed.
- `cargo test --locked --lib notices:: -- --nocapture`: 39 passed.
- Focused notification, resumption, release-question, failure-signature and
  prompt-display checks: 15 passed.
- Existing grant, preparation, shared-lock rescue and worktree lifecycle
  checks: 5 passed.
- `node --test tests/notification_readability.mjs tests/question_delivery.mjs tests/deputy_exhausted.mjs`:
  11 passed.
- `git diff --check`: passed; reconstructed patch digest and 22-path manifest
  matched. The complete source recovery patch was also compared with the final
  working diff and the new UI test.

The upgrade test persists and reloads a pass carrying `ParkCause::Upgrade`; it
is not an end-to-end deployment/restart test. The newly reconstructed live HTTP
notification test and real-browser rendering remain for parent verification.
No production run, queue record, candidate ref or pull request was rewritten.
