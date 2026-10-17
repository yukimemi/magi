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

The editing-seat grants permit the repository's lock protocol; editing agents
still acquire their own lock and retain it for the required lifetime.
Acquisition failure still requires stopping work.
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

Read-only seats retain their sandbox policy and receive none of these new
grants. For a repository registered with `kanade-sync.contract` containing
`shared-lock-v1`, `repo_lock::SeatLease` acquires the real exclusive
`kanade-sync.lock` in the launcher before creating the seat's prompt or starting
its CLI. The handle stays open for the entire invocation, including waiting
for termination on timeout. The prompt identifies this actual launcher-owned
lock and tells the reader not to try taking a second exclusive handle. This
applies to repository-reading invocations regardless of backend,
including advisors, judges, deliberation, voting, review, bump decisions and
waiter resumes. Concurrent readers in the same launcher share the held handle;
other processes still contend with it. Nonblocking acquisition attempts retry
on external contention as an infrastructure wait, before creating artifacts,
starting the CLI or starting its timeout. No error reaches fallback, seat
history or review budgets merely because an updater owns the lock. A caller
can cancel this wait without having started any work. The last reader releases
by closing the handle, never by
unlinking the lock file.

Every registered repository seat also takes a fair per-repository local
reservation. Readers share a reservation; an editor takes an exclusive reservation before
starting its CLI, then acquires the required .NET lock itself. A queued editor
prevents subsequent readers from overtaking it. The last reader's OS handle
closes before its reservation wakes an editor. Thus a long read-only advisor or
review cannot cause this launcher's editor to start into a lock held by its own
readers. A reader waiting for an external lock releases its local reservation
between nonblocking attempts, allowing an editor to proceed.

Control-plane seats (`conduct`, `dupes`, `chat`, and `deputy`) stay outside
repository scheduling and must perform no repository work. This is independent
of write permission and explicit store grants, including ordinary chats with
empty writable roots. They do not claim to hold a repository lease. This preserves the supported
question/control-plane route while an editing seat waits for an answer; putting
that route behind the editor's reservation would deadlock delivery.

Repository readers (including judges, advisors and reviewers) still wait for
the editor: allowing them through would violate the shared-lock contract.
Non-checkout command seats need no Git resolution. Checkout detection uses the
filesystem marker rather than localized Git errors; once a marker is found,
resolution failures remain fatal before spawning.

The native lock interoperates with .NET `FileShare.None` using Unix
`flock(LOCK_EX | LOCK_NB)`. Unsupported platforms,
unreadable registrations and unknown contract versions fail closed. This does
not register repositories or change writable seats' obligation to acquire and
hold their lock. Unregistered repositories cannot be assumed coordinated.
It also does not replace the launcher's obligation to coordinate worktree
creation or unrelated repository operations before recovery is accepted.
Review/judge/deliberate/vote remain unable to ask by CLI policy; granting readers
question-store writes is outside this fix.

`seat_process::Running` starts coordinated CLIs in a separate process group.
On cancellation (including waiter halt or chat turn lease loss), it transfers
the child and reservation/OS lease to a cleanup thread independent of the
cancelled future and async runtime. The same cleanup runs on timeout or normal
completion. Cleanup stops the group, discovers and freezes live descendants
(including terminal tools in separate groups), kills them and waits for process
termination before releasing coordination. An unreadable process table or
unknown termination keeps coordination held. Failure to start cleanup retains
the child and reservation instead of releasing them. This cleanup must finish
before another local editor can start or an external updater can take a reader's
OS lock.

Registered invocations currently require Unix process containment. Windows
registered seats fail before spawning until equivalent job containment is
implemented; the low-level Windows lock implementation alone is insufficient.
Wrappers must not daemonize/reparent tools before their CLI reports completion;
deliberately escaped processes and abrupt launcher process death are outside
this containment contract. Do not register such launchers as coordinated.

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
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope coordination:: -- --nocapture
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib repo_lock::tests
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope readonly_invocation -- --ignored --nocapture
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
   confirm the launcher-held read-only lease and coordination of worktree creation.
3. Only after that evidence exists, run
   `magi task release 20261016-031545-f3b5`. This releases the original task with
   its original instructions/history; do not restart a failed no-change run,
   file a replacement or override duplicate ownership with `--force`.
4. Confirm the next attempt holds the required lock while working and can use
   its question route. Repo-sync diagnosis, implementation, review, rollout and
   Mac verification remain with that task; its minipc verification exemption
   remains in force.

## Review round 1 verification

Initial attempts inside the outer seat failed writing shared Git metadata and
launching a nested sandbox (`sandbox_apply: Operation not permitted`). An
authorized host execution of the actual backend test subsequently succeeded:

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope sandbox_seat -- --ignored --nocapture
# sandbox_seat_can_lock_ask_and_receive_answer_but_cannot_write_unrelated_state ... ok
# 1 passed; 0 failed
```

This used the installed CLI's actual sandbox, resolved write roots and real
PowerShell `FileShare.None` lock. It first refused contention before question
creation, then acquired the released lock, denied a write to the sibling queue
directory, and completed a supported `magi ask` / `magi answer` round trip.
The test's inner sandbox remained enabled; host authorization only allowed
launching that sandbox outside the enclosing seat restriction.

The read-only invocation fixture also passed:

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope readonly_invocation -- --ignored --nocapture
# readonly_invocation_holds_dotnet_compatible_lock_and_refuses_external_contention ... ok
# 1 passed; 0 failed
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib repo_lock::tests -- --nocapture
# 2 passed; 0 failed
```

An external .NET holder prevented the read-only invocation from creating even
its prompt or starting its CLI. After release, the real invoked reader observed
that its launcher's native handle excluded a second .NET handle. After the
invocation returned, .NET acquisition succeeded again. Native tests additionally
verify that concurrent launcher readers retain their shared exclusive handle
until the last reader finishes, and never delete the lock file.

These fixtures establish sandbox and protocol behavior; they do not deploy a
binary or verify an existing daemon's environment. The original task remains
held, with no replacement or production-record edits. Merge approval, binary
deployment, daemon restart and verification of the actual target's full
coordination path remain prerequisites to releasing it. Windows interoperation
still needs validation on Windows; the tests above ran on macOS.

The 60 focused `agent::tests` and both default `seat_write_scope` tests also
passed after the read-only coordination change.

## Review round 2 verification

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope -- --nocapture
# 6 passed; 0 failed; 5 explicit host tests ignored
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope readonly_invocation -- --ignored --nocapture
# 1 passed; 0 failed
```

The new default fixtures use real processes and OS locks, without optional
PowerShell or backend installation. They drop the invocation future while a
CLI and a tool in a separate process group are running, and assert that an
external lock taker cannot succeed until both processes have terminated. A
second fixture queues an editor behind a long reader, verifies that a later
reader cannot overtake the editor, cancels the original reader, and confirms
the editor acquires its real OS lock after cleanup. A third holds an external
lock, verifies the invocation remains pending without artifacts or a child,
and confirms that release produces exactly one successful seat turn, rather
than failed attempts or fallback agents. The PowerShell fixture confirms the
same pre-spawn wait still interoperates with .NET `FileShare.None`.
The queueing fixture also confirms that purpose-specific question delivery can
still write its control-plane store while repository work is reserved.

The focused 60-test agent module and `cargo clippy --locked --lib -- -D warnings`
passed. The actual backend sandbox fixture was rerun successfully (one passed),
retaining lock contention, question round-trip and unrelated-write denial checks.

All waits are infrastructure scheduling, before the agent's timeout. They can
be cancelled by their caller; a held external lock is not retried as a new
agent attempt. Cleanup completion, not caller cancellation, determines when
the local reservation and reader OS lease may be released. No production task
has been released and no deployed-environment repair is claimed by these tests.

An authorized `git fetch origin main` succeeded during round 1. Both
`origin/main` and `FETCH_HEAD` resolved to
`3393efac3837257589499a9d686cdedbb54cd844`. A fresh supported `magi task show`
confirmed the original task still has its manual hold and both original runs;
no release was attempted.
