# Isolated editing seats

Repository editing seats need more than the checkout as a write root. Git
worktrees share a Git directory outside that checkout. A repository may require
an exclusive lock there for the entire work session. `magi ask` also writes
outside the checkout: question records, temporary files, update locks, leases
and panel assets live in `questions`. Filing a question covers matching notices
in `notifications` through `Questions::put` / `notices::quiet_for`.

`agent::invoke` prepares these additional directories before starting editing
nodes (`implement`, `fix`, `graft`, `gate-fix`, `land`, `rebase`). It resolves
`git rev-parse --path-format=absolute --git-common-dir` from the actual seat,
canonicalizes the directories, and pins the child's `MAGI_HOME` to the parent
store. Resolution/preparation failure prevents agent startup. The central path
also covers session resume, fallback agents and waiter continuation. Recovery
repairs use the `fix` node. Read-only bump decisions do not receive grants.

This permits the repository's lock protocol; it does not acquire a lock for the
agent or change its lifetime. Acquisition failure still requires stopping work.
Do not delete lock files, forge ownership or use short-lived locks for a whole
work session. Granting the common Git *directory* also permits other Git metadata
writes, including refs and sibling worktree administration. Directory-level
permission is broader than a single lock file and is deliberately documented.

Only `questions` and `notifications` are added from the control plane. Queue,
run and chat stores and the state root are not added. Commands such as
`magi task add` are consequently not enabled for sandboxed implementers by this
change. Notification webhooks remain best effort; network access is not added.
Deputies and consulted chats keep their existing explicit, purpose-specific
grants and do not gain repository metadata access through this policy.

## Adapter and platform limits

| Adapter | Additional scope | Enforcement limits |
| --- | --- | --- |
| Codex | `sandbox_workspace_write.writable_roots`, before `resume` | Retains `workspace-write`, `approval_policy=never`; unrelated writes remain denied by the CLI's OS sandbox. Host/managed policies can further restrict these grants. |
| Antigravity | Repeated `--add-dir` on initial and resumed invocations | Workspace membership, not a verified equivalent OS write boundary. Existing permission mode remains unchanged. |
| Claude | No additional flag required | Existing permission bypass has no checkout-only filesystem boundary. |
| opencode / omp | No additional flag required | Existing auto modes provide no enforced read-only or checkout-only filesystem boundary. |
| command | No sandbox imposed by magi | The configured executable is responsible for confinement and lock compliance. |

Read-only seats retain their existing policy and receive none of these new
grants. A backend with enforced read-only access cannot satisfy a repository
rule requiring every reader to acquire a writable shared lock, or run
`magi ask`. Review/judge/deliberate/vote also cannot ask by CLI policy. Such a
repository needs a separately approved coordination design before the whole
workflow can be called repaired; widening read-only access is outside this fix.

macOS/Linux enforcement depends on the installed CLI and OS sandbox. Windows
sandbox behavior and Antigravity terminal restrictions have not been validated
by the host tests here. An already-running seat retains its old scope: a source
patch or newly built binary cannot widen the outer sandbox that launched it.

## Verification and recovery

Run focused checks with the stable toolchain and the configured shared build
cache:

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib agent::tests::repository_write_scope
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib agent::tests::writable_directories
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope shared_lock_contention -- --ignored --nocapture
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope host_seat -- --ignored --nocapture
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope sandbox_seat -- --ignored --nocapture
```

The host tests are explicit because they need PowerShell and an installed CLI
whose `sandbox` command accepts a command directly (verified help on 0.160.0).
They use disposable repositories with linked worktrees and a separate fake
state store, never production records. The lock fixture uses the real .NET
`FileStream` / `FileShare.None` contract: contention refuses work, closing the
holder releases it, and reacquisition succeeds without deleting the file. The
sandbox fixture first verifies contention prevents question creation, then
acquires the released lock, denies a sibling queue write, files a real
`magi ask`, and receives a reply through `magi answer`. Temporary-directory
default grants are disabled for the denial assertion. Neither test substitutes
for verifying the deployed daemon and its actual seat environment.

Task `20261016-031545-f3b5` remains manually held. Its failed runs
`20261016-060517-e0cf` and `20261016-061144-d496` produced no implementation;
the latter's artifacts show lock-open and question temporary-file permission
failures. Question `28fc` recorded an instruction to keep waiting. Task
`20261017-130123-28b7` owns this infrastructure repair; the earlier completed
task `20261008-203938-9168` concerned conversational permissions, whose bypass
must not spread to unattended seats.

Recovery must follow these steps:

1. Finish the parent graph's review and verification, publish the normal PR,
   and obtain explicit owner approval before merging. Do not review a running
   candidate recursively or merge from inside a candidate seat.
2. Install the reviewed merged/released binary and restart the relevant daemon
   through its supported operating procedure. Inspect a newly launched seat's
   actual arguments and run the sandbox fixture on that host. Verify the target
   repository's lock and the supported question route with the actual backend;
   resolve the read-only coordination limitation above wherever applicable.
3. Only after that evidence exists, run
   `magi task release 20261016-031545-f3b5`. This releases the original task with
   its original instructions/history; do not restart a failed no-change run,
   file a replacement or override duplicate ownership with `--force`.
4. Confirm the next attempt holds the required lock while working and can use
   its question route. Repo-sync diagnosis, implementation, review, rollout and
   Mac verification remain with that task; its minipc verification exemption
   remains in force.

At implementation time the outer seat refused shared Git writes (including
`git fetch`'s `FETCH_HEAD`) and nested sandbox startup failed with
`sandbox_apply: Operation not permitted`. Network resolution also prevented
`git ls-remote` from checking upstream freshness. The available `origin/main`
was `3393efac3837257589499a9d686cdedbb54cd844`. These are blockers to live
repair/recovery claims, not evidence that the hold can be lifted.

Staging this candidate also failed creating the worktree administration
directory's `index.lock` with `Operation not permitted`. The parent graph must
capture the candidate and finish its normal review/PR workflow; no alternate
Git directory or sandbox bypass was used to manufacture a commit.

The real PowerShell lock fixture passed: a second process could not acquire
the held lock, its work marker was absent, and acquisition/work succeeded after
the holder disposed the handle. The separate host control-plane fixture also
passed a real `magi ask` / `magi answer` round trip while holding the lock.
These host successes establish the lock/CLI contracts, not sandbox usability.
The explicit sandbox test failed at sandbox startup under this outer seat;
unrelated-write denial and the question round trip inside the intended sandbox
still require an un-nested host run. No production task was released or edited.
The 60 `agent::tests` passed, as did both default `seat_write_scope` tests,
including actual invocation argument capture for every editing node on first
turn and resume, and preservation of read-only/deputy/chat scope.
