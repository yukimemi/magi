# Recovering base synchronization

Synchronization has two counters: `BaseSync.attempts` retains lifetime evidence,
while `base_sync_recovery.episodes` limits active base chasing to four reserved
operations per interval. An operation reserves its slot and immutable starting
HEAD/base SHA before Git runs. No restart refunds or double-counts it. A stopped
conflict, foreign remote divergence, publication failure and movement exhaustion
remain different checkpoints. Resource waits are infrastructure evidence, never
textual conflicts.

## Infrastructure interruptions

A successful synchronization or an actually executed gate/current-head E2E check
records progress. Saving a structured resource stop after progress atomically
records an interruption boundary (including pass ordinal, head and time). Only a
new explicit owning-task resumption after that boundary can open a new four-slot
interval. The boundary and owner generation are consumed once. The latest task,
matching instructions and retained HEAD are checked by the live driver after
fetching the base. No new interval is granted by daemon restart, automatic
resource retry, elapsed idle time, repeated wait records, or repeated resume
requests without new progress followed by an interruption. `Unrecorded` resource
causes and unrecorded pass endings establish no renewal authority.

Actual progress after a wait supersedes its boundary, retaining the old evidence.
A new interval begins with no progress evidence of its own. Synchronization does
not replenish gate-fix, review, or conflict-fixer budgets. Separate repair-budget
grants retain their existing interface and accounting.

## Historical exhaustion and explicit recovery

Inspect the retained changes and ownership, then authorize a new finite interval:

```sh
magi run --resume <saved-run> --recover-base
```

A new explicit owner queue resumption after an exhaustion stop uses the same
recovery machinery. A generic release or idle polling cannot authorize it.
Authorization takes the existing owner lock and execution maintenance lease and
rejects live/unknown drivers (including descendants), transferred ownership,
non-latest runs, dirty trees and branch/worktree mismatches. Publication and
landing checkpoints retain their own workflow. Conflicts and foreign branch
commits cannot be bypassed with `--recover-base`.

Approval is pinned to run, task, immutable stop HEAD, fetched base SHA, task
instruction, active interval and, for queue approval, its generation. Consumption
rechecks these bindings. A moved base or changed instruction invalidates the
approval, records why, and requires a new explicit approval. Resume a pending
request rather than creating duplicates. The CLI, notices and web report the
actual next action and both interval and lifetime consumption.

Older records migrate conservatively: their lifetime count becomes initial
interval usage; dates and old wait messages grant nothing. Exact historical
`rebase(s)` and current `synchronization(s)` exhaustion diagnostics are recognized.
Existing authorization, attempt and stop evidence remains readable. A pending
old-format single-attempt approval retains its original one-attempt limit; a new
approval grants the documented four-slot interval. No saved task records need
manual editing, deletion or replacement.

## Crash recovery and verification

An unfinished operation resumes against its reserved SHA even if main has moved.
The retained branch/worktree and actual Git state are checked before continuation.
A completed rebase can be reconciled by ancestry and patch containment; an
unrecognized HEAD stops for inspection rather than inventing another reservation.
The scratch rebase and conflict-fixer accounting keep their existing recovery
semantics. Resource interruption leaves the reservation pending. A terminal
operation retains its spent slot even when it failed.

After a successful rebase, the winner worktree is synchronized and stale gate
and current-head E2E evidence is archived through the existing verification
history. Old review rounds stay attached to their reviewed heads and continue to
count against review limits; the existing review/catch-up policy still applies.
Current-head E2E and gate checks must execute before delivery; no old gate can
certify a new HEAD. Recovery does not approve merging. Repository coordination,
PR publication leases and head-specific owner merge approval remain required.

An already-integrated PR whose closure failed is a separate resumable checkpoint:
resume rechecks integration and public ownership and retries closure without any
new synchronization allowance.

## 停止診断と消費数の表示

次の行動には、現在のエピソード番号、エピソード内の消費数／上限、
生涯の同期回数が一行で表示されます。承認済みなら、その承認を使って
保存済みの実行を再開するよう案内します。

停止記録の `unverified_sync` は、同期が成功したHEADについてゲートが
実行される前にbaseが再び進んだことを示します。これは原因を説明する
診断情報であり、新しい同期枠を付与する根拠にはなりません。
検証の進捗と同期の成功は区別して記録し、古い記録に同期の証拠がなければ
この診断は付けません。承認の拒否は `consumed` と `invalidated` の理由を
まとめて記録し、再開後にも拒否理由を確認できます。

A legacy resource stop with a standing scratch rebase can import that operation
once. Migration verifies its saved original HEAD, branch identity and target,
and the retained branch/worktree, then saves a spent reservation against that
saved target. It completes before a separate reservation follows newer main;
no interval renewal or repair-budget grant is inferred from the old wait.

Conflict-resolution validation saves the exact accepted result HEAD on the
matching base-sync reservation before removing scratch evidence. A restart can
accept that exact result even when resolution changed the original patch IDs.
It still checks the retained branch/worktree; a different HEAD does not inherit
this validation.

If Git continuation completed before result validation was saved, reconciliation
revalidates the remaining scratch using the same completion checks. Scratch
repository, branch, original HEAD, current HEAD, clean tree and recorded picks
must match the reserved operation. Validation is saved before cleanup; refused
or incomplete evidence stays intact for inspection and grants no new attempt.
