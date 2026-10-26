# Isolated editing seats

Repository editing seats need more than the checkout as a write root. Git
worktrees share a Git directory outside that checkout. A repository may require
an exclusive lock there for the entire work session. `magi ask` also writes
outside the checkout: question records, temporary files, update locks, leases
and panel assets live in `questions`. Filing a question covers matching notices
in `notifications` through `Questions::put` / `notices::quiet_for`.

`agent::invoke` prepares these additional directories before starting editing
nodes (`implement`, `fix`, `graft`, `gate-fix`, `land`, `rebase`) and ordinary
write-enabled chat turns. For checkouts it resolves
`git rev-parse --path-format=absolute --git-common-dir` from the actual seat,
canonicalizes the directories, and pins the child's `MAGI_HOME` to the parent
store. Checkout resolution or store preparation failure prevents agent startup;
non-checkout command seats receive just the control-plane directories. The central path
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
change. Notification webhooks remain best effort; network policy is configured
separately as described below.
Deputies and chats keep their existing explicit, purpose-specific
grants and do not gain repository metadata access through this policy.

## Requested editing network policy

`[graph] editing_network = "restricted" | "enabled"` defaults to `restricted`.
For sandboxed repository writers, the launcher passes the requested boolean via
`sandbox_workspace_write.network_access`, retaining `workspace-write`,
`approval_policy="never"`, existing writable roots, coordination and process
containment. `enabled` permits outbound traffic as well as local test servers;
the supported setting cannot grant loopback alone. This repository opts in for
its browser tests. Other roles and adapters retain their existing policy.

`Invocation` carries the configured policy through initial turns, resumes,
fallbacks and waiter delivery. The same policy function supplies the argv,
`requested_network_access` in `*.access.json`, and current-turn guidance. This
is a request, not evidence of effective access: host or managed restrictions
can still deny networking, and a child cannot relax inherited parent restrictions.
Git administration-path denials are separate from network denials and remain
possible even with the common directory grant.

During infrastructure repair, a failed reproduction is evidence of the current
defect. It does not prevent independently permitted source edits for a future
launcher. Record denied operations, targets, exact errors and their impact;
continue independent edits and focused checks without bypasses or futile
retries. Stop when a required denial blocks all remaining permitted work.
Candidate preservation rescues eligible edits independently of a failure report;
this does not establish successful deployment or validation.

Repository configuration comes from the launcher's fetched base, never the
candidate working tree. Deployment therefore requires integrating the policy
and source changes, deploying the rebuilt launcher, and preparing a new seat
from the fetched base through the existing preparation/base-sync paths. Deploy
the updated launcher before it discovers a base with the new setting: older
launchers reject the unknown graph key. Saved runs retain their saved policy;
resuming an old restricted run does not reload the candidate configuration. No
seat-side fetch is required. A parent running the old launcher cannot gain the
new policy merely by executing a newly built child under inherited restrictions.
The parent-owned verification path must inspect fresh access evidence, verify
local server bind/browser execution and the unrelated-write denial, and check
coordination/containment before declaring the browser blocker resolved. Source
and fixture checks alone do not demonstrate resolution of the existing blocked
browser task. Genuine parent/managed-policy or protected Git-path denials must
be reported separately with the operation and error.

In the restricted repair seat, `RUSTUP_TOOLCHAIN=stable cargo test --locked
--lib editing_` also selected four existing web tests. All four failed at
`src/web.rs`'s `bind loopback` with `Os { code: 1, kind: PermissionDenied,
message: "Operation not permitted" }`; the ten other selected tests passed.
This is reproduction evidence under the current inherited seat restrictions,
not a test of the deployed correction. Do not retry those binds in the same
environment or treat them as a source-editing blocker. The existing browser
task's blocker remains unproven resolved until parent-owned real verification
succeeds after deployment.

## Parent-owned review and verification

Child seats in an existing run implement and commit their assigned changes and
run necessary focused checks. The parent graph owns the review loop, full
verification and final gate, including reruns after repairs. Child seats must
not launch `magi review`, `magi run` or another nested run to review their own
candidate. Advisor proposals and synthesized briefs must omit separate-review
instructions, including conditional `--merge none` advice, and must not assign
parent-owned full verification to implementers. Fixed ownership guidance follows
the brief in the implementation prompt and applies without a configured cache.
Review, judging, fix, graft, landing, rebase and implementation continuation
prompts carry the same ownership rule; question delivery retains the seat's
existing conversation and supported `magi ask` wait/thread flow.

This is prompt-only guidance. It does not filter generated brief text, intercept
shell commands, reject nested CLI invocations or guarantee agent compliance.
Filesystem grants and repository coordination remain separate mechanisms; they
are not a duplicate-review guard. Supported question/control-plane commands
remain available as explicitly instructed, and parent review/verification/gate
execution is unchanged. For genuinely hand-written changes outside a parent
run, the repository's existing `AGENTS.md` requirement still applies:
`magi review <branch> --merge none` before opening a PR. Kata-managed conventions
are unchanged.

## Adapter and platform limits

| Adapter | Additional scope | Enforcement limits |
| --- | --- | --- |
| Codex | `sandbox_workspace_write.writable_roots`, before `resume` | Retains `workspace-write`, `approval_policy=never`; unrelated writes remain denied by the CLI's OS sandbox. Host/managed policies can further restrict these grants. |
| Antigravity | Repeated `--add-dir` on initial and resumed invocations | Workspace membership, not a verified equivalent OS write boundary. Existing permission mode remains unchanged. |
| Claude | No additional flag required | Existing permission bypass has no checkout-only filesystem boundary. |
| opencode / omp | No additional flag required | Existing auto modes provide no enforced read-only or checkout-only filesystem boundary. |
| command | No sandbox imposed by magi | The configured executable is responsible for confinement and lock compliance. |

Read-only seats retain their sandbox policy and receive none of these new
grants. Repository coordination is opt-in through `[coordination]` in either
the machine or repository config: `contract_file`, `contract_token`, and
`lock_file` are all required after layer merging and template rendering. File
names must be plain names inside the Git common directory (no separators or
`..`). With no section, no contract is read and no lock is taken. A missing
configured contract also leaves the repository unregistered; other read errors
or a token mismatch fail closed. The contract's trimmed contents must match
`contract_token`. For registered repositories, `repo_lock::SeatLease` acquires
the real exclusive configured `lock_file` in the launcher before creating the
seat's prompt or starting its CLI. The handle stays open for the entire invocation, including waiting
for termination on timeout. The prompt identifies this actual launcher-owned
lock and tells the reader not to try taking a second exclusive handle. This
applies to repository-reading invocations regardless of backend,
including advisors, judges, deliberation, voting, review, bump decisions and
waiter resumes. Concurrent readers in the same launcher share the held handle;
other processes still contend with it. Nonblocking acquisition attempts retry
on external contention as an infrastructure wait, before creating artifacts,
starting the CLI or starting its timeout. No error reaches fallback, seat
history or review budgets merely because an updater owns the lock. External
lock waits are bounded by `graph.timeout_resource_wait`. Local reservation
waits have no resource deadline: editing seats in one registered repository
serialize, and a predecessor may legitimately run longer than that limit.
These waits check the run's pause/interrupt handles at least every five seconds.
An immediately available reservation still permits a completed node to save
its results before honoring a park at the next node boundary. External wait
expiry produces a repository-coordination resource block, not an agent failure or a permanent preparation failure; a pause remains
a park. Standalone invocations use the default graph limit and public
folding uses its configured limit, and callers can also cancel by dropping
the waiting future. No process or prompt artifact exists until acquisition.
The last reader explicitly
unlocks before closing the handle, never unlinking the lock file. Explicit
unlocking prevents an unrelated concurrent fork's inherited descriptor from
delaying release until that process executes or exits.

Every registered repository seat also takes a fair per-repository local
reservation. Readers share a reservation; an editor takes an exclusive reservation before
starting its CLI, then acquires the required .NET lock itself. A queued editor
prevents subsequent readers from overtaking it. The last reader's OS handle
closes before its reservation wakes an editor. Thus a long read-only advisor or
review cannot cause this launcher's editor to start into a lock held by its own
readers. A reader waiting for an external lock releases its local reservation
between nonblocking attempts, allowing an editor to proceed.

Launcher operations reacquire both the exclusive local reservation and the real
OS lock after a seat finishes. Candidate and fixer rescue commits, recovery
rollback, review verification (including catch-up), and the complete mechanical
pre-gate and final gate sequence cannot start while an external updater owns
the lock. Interrupted grafts, early recovery rollback and restoration between
fixer fallbacks all use the same operation lease. If acquisition fails or
expires, reset/clean do not run and an unsettled rollback record stays unsettled.
Fallback preparation releases
its operation lease before starting the next seat; the local gate is not
reentrant.

Whenever both resources are needed, acquisition order is shared build cache,
then repository reservation/OS lock; release is in reverse order. Verification
acquires repository coordination inside the cache lease, including mechanical
pre-gate edits. Rescue commits release their repository lease before entering
verification. This matches editing waves and avoids a cache holder waiting
for a repository reserved by the very verification waiting for its cache.

Control-plane seats (`conduct`, `dupes`, and `deputy`) and chat turns
(`queue::CHAT_NODE`), including consult drains, completion notices and waiter-style
continuations delivered through `talk::turn`, stay outside repository scheduling.
They never acquire or wait on a `SeatLease` or the per-repository gate. A
consultation turn is an ordinary chat turn: no separate investigation or
control mode and no repository lease.

Ordinary chats retain their existing filesystem grants and sandbox policy without
repository scheduling. Read-only chats remain read-only; a chat in a repository
that opted in with `[talk] allow_write` may write, runs unsandboxed and gets the
question and queue stores as writable roots. An open question or a consultation
grants no extra write access and no repository edit permission. A chat's
`magi answer --reply` / `--reply-other` and `magi task control ...` go over the
turn's inherited control bridge instead: the launcher records them outside the
sandbox, including an obsolete approval's dismissal that saves its run record,
so they work in a read-only chat without any runs directory grant.

Repository readers (including judges, advisors and reviewers) still wait for
the editor: allowing them through would violate the shared-lock contract.
Non-checkout command seats need no Git resolution and receive only question and
notification store grants when writable. Checkout detection uses the
filesystem marker rather than localized Git errors; once a marker is found,
resolution failures remain fatal before spawning.

The native lock interoperates with .NET `FileShare.None` using Unix
`flock(LOCK_EX | LOCK_NB)`. Unsupported platforms,
unreadable registrations and unknown contract versions fail closed. This does
not register repositories or change writable seats' obligation to acquire and
hold their lock. Unregistered repositories cannot be assumed coordinated.
Launcher worktree preparation takes `SeatLease::for_operation` before any
rollback, config/ref mutation, checkout creation or setup. It holds both the
local exclusive reservation and the real shared OS lock through preparation,
then releases them before invoking any seat (the gate is not reentrant).
This covers candidate/judge/advisor preparation, the attached under-review
checkout and takeover, reviewer creation/reset/setup on every round, and
operator-fix creation/setup and removal. Both `Runner::fold_losers` and the
public `fold_run` used by CLI, Web and automatic cleanup take the same lease
before removing worktrees/branches or releasing shared worktree config.
External contention waits before these mutations begin;
an unreadable lock or unsupported registration fails preparation closed.
These bounded preparation leases do not replace editing agents' session-long
locks or establish coordination of unrelated repository operations.
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

Before spawning, the launcher also creates a liveness pipe with a writer
inherited by the CLI and its tools. Cleanup retains repository, worktree-output
and cache leases until EOF on that pipe, including on normal CLI completion.
Output-protection leases are released before repository coordination is made
available again, so the next repository owner can immediately reclaim outputs.
Thus a tool that forks into a separate session and becomes reparented before
cleanup can no longer release ownership merely by disappearing from the CLI's
process tree. An undiscovered tool may finish naturally while cleanup waits;
there is no timeout that converts an unknown writer into permission to reclaim.
A wait exceeding one second logs a retention warning; pipe read errors retain
the leases and log an error. After descendant termination and witness EOF,
cleanup synchronously releases its worktree-output and cache reservations before
releasing repository coordination, then signals completion. A subsequent
repository operation therefore cannot contend with that cleanup owner's output
reservations. Output leases remain independent locks: other owners or retention
may acquire them, including between these releases. Reclamation is safe at that
point because the tools have terminated. This cleanup handoff order does not
change the shared build cache acquisition order described above.

Each invocation also gets
a fresh `MAGI_PROCESS_OWNER` environment token. Cleanup discovers processes
with that token even after reparenting and freezes/kills them with the other
tools. This covers tool launchers that close extra descriptors but preserve
their inherited environment; the pipe independently covers environment-clearing
tools that preserve the descriptor.

Registered invocations currently require Unix process containment. Windows
registered seats fail before spawning until equivalent job containment is
implemented; the low-level Windows lock implementation alone is insufficient.
Wrappers/tools must preserve the inherited liveness descriptor or the
`MAGI_PROCESS_OWNER` token when detaching from the CLI. Processes that discard
both ownership witnesses before reparenting, and abrupt launcher process death,
are outside this containment contract. Do not register launchers that discard
both witnesses while leaving tools running.

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
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib graph::tests::preparation_
RUSTUP_TOOLCHAIN=stable cargo test --locked --lib graph::tests::fold_run_
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


## 2026-10-18–19: editing-seat Git/network investigation

### 元席の実効設定と配備経路（R1-1-2）

保存済み run `20261018-084955-964b` の `impl-A.out` と、対応セッション
`01a14c4b-2cb9-7341-bef5-88fe101e2eb0` の `session_meta` / `turn_context` を確認した。
CLI は 0.160.0、実効設定は `workspace-write`、`approval_policy=never`、
`network_access=false` だった。追加 roots は対象リポジトリの common Git
directory、questions、notifications の三つで、二つの temporary-directory
除外設定はいずれも false だった。したがって、元席での失敗は common Git
directory の指定欠落ではない。通常 fetch は linked worktree の
`FETCH_HEAD` を開く段階で EPERM、`--no-write-fetch-head` は名前解決失敗だった。
run の `base_commit` は依頼文の SHA と一致する。これは別リポジトリの基底であり、
この修正へ cherry-pick するコミットではない。

元プロセス終了後に最新版のソースや現在のインストール版を見ても、当時の
実行ファイルは特定できない。そこで保存された Web handover ログを確認した。

| UTC 時刻 | 保存ログが示す事実 |
| --- | --- |
| 2026-10-17 23:40:36.960 | v0.159.0 の release archive を通常のインストール先へ配備完了 |
| 23:40:36.979–36.980 | その実行ファイルの `web` コマンドで後継 PID 1604 を起動 |
| 23:40:38.034 | 後継自身が `web process started (version 0.159.0)` と記録 |
| 23:49:55.408 | 問題の run を作成 |

元の kernel exec argv 全文を記録したログは残っていないため、履歴の復元と
推測を混同しない。取得不能な部分は、ログで特定した**配備版そのもの**を使う
管理された再現で補った。v0.159.0 の release asset を一時領域だけに取得し、
`magi --version` が 0.159.0 を返すことを確認した。

- tag の commit: `7738d9ad524726e33e482174207d6dd62bcae89c`
- release asset ID: `651740632`（元 handover ログの download URL と一致し、API の digest も下記 hash と一致）
- archive SHA-256: `84fc30f9c45880c92a521b09747a260c24daa6fd9ef0cb5d48f4829e592fd9b9`
- 実行ファイル SHA-256: `45d993d8a191a8fdff4c7c44c859beeeafff51d6c448010ae0bd2483ac78cae5`

再現では local bare remote、隔離した `MAGI_HOME` / `MAGI_CONFIG_DIR` と
`graph.worktree_root` を使用した。元 run の roster に保存されていたモデルと
extra args をそのまま設定し、単一編集席・advice 無効・retry 無効・merge none・
land 無効に限定した。`magi run` が通る通常の graph / `agent::invoke` 経路を使い、
PATH 上の記録用 CLI が**実際に受け取った**引数を JSON に保存して直ちに終了する。
この CLI は実装もモデル呼出しも行わない。意図した exit 1 の後、launcher は
`no candidate produced a change; nothing to judge` で終了した。本番 daemon、
元タスクの owner、元実装 worktree は操作していない。

取得した argv の正規化表記（パスと保存モデル値のみ置換）:

```text
exec --json --skip-git-repo-check -C <fixture-worktree>
--sandbox workspace-write
-c approval_policy="never"
-c sandbox_workspace_write.writable_roots=["<common>","<questions>","<notifications>"]
-m <saved-model> -c model_reasoning_effort="medium" -
```

`node=implement` / `seat=impl-A`、roots の実際の canonical paths、保存モデルと
extra args の一致、stdin delivery、bypass と network override の不在を機械的に
照合して成功した。raw 記録 `fixture/verified-invocation.json` の SHA-256 は
`d76be226fb1dade49c561cc46de0a494fc61cf213affe3f85bdb6bf9afebe41f`。
この再現は現在のソースから組み立てた想定 argv ではなく、当時の release
実行ファイルが通常の編集席起動で渡した argv である。ただし、元席そのものの
失われた raw trace を取得したとの主張ではない。network flag の不在だけで
実効値を推定せず、元セッションの `network_access=false` と合わせて判断する。

### ホスト上の実 sandbox 検証（R1-1-1）

最初の候補席では enclosing sandbox が loopback bind と入れ子の sandbox 起動を
拒否した。この失敗を検証成功とは数えていなかった。今回、許可されたホスト実行
経路から、**内側の sandbox は有効なまま**既存の ignored fixture を実行した。
実行環境は macOS 27.0.1 / arm64、インストール済み CLI は 0.163.0。
以下の直接検証はこの版に限り、元席の 0.160.0 を再実行したとの主張ではない。

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope sandbox_git_operations_and_network_policy -- --ignored --nocapture
# 1 passed; 0 failed
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope sandbox_seat -- --ignored --nocapture
# 1 passed; 0 failed
RUSTUP_TOOLCHAIN=stable cargo test --locked --test seat_write_scope readonly_invocation -- --ignored --nocapture
# 1 passed; 0 failed
```

最初の fixture は使い捨て linked worktree と local remote を使い、実 CLI の
`sandbox` コマンドで workspace-write と同じ common Git / 制御ストア roots を
設定する。temporary-directory の既定許可を除外し、許可外書込みの検査を厳密にする。
ネットワーク以外の設定を固定して false / true を比較した。

| 操作 | network=false | network=true |
| --- | --- | --- |
| worktree のファイル編集 | 成功 | 成功 |
| `git status` / `git diff` / `git log` | すべて exit 0 | すべて exit 0 |
| `git add` / `git commit` | `index.lock` 作成が EPERM、exit 128 | 同じ拒否 |
| local remote の `git fetch` | `FETCH_HEAD` open が EPERM、exit 255 | 同じ拒否 |
| 許可 roots 外への書込み | 拒否、ファイルなし | 拒否、ファイルなし |
| ホスト listener への接続 | errno 1 | 成功 |
| DNS | 名前解決失敗 | 成功 |

直前の sandbox 外ホスト DNS も成功した。したがって、この比較では DNS の
失敗はホスト DNS 障害ではなくネットワーク sandbox policy によるものと確認できた。
ネットワークを有効にしても Git 管理領域の拒否は変わらず、両制限を切り分けられた。
比較用 true はこの fixture のみであり、production の編集席設定は変更していない。

検査後は Git を手動実行するだけでなく、実際の `git::rescue_commit` をホストで
呼び出した。`committed=true`、withheld なしで成功し、`git show HEAD:probe` の
内容が sandbox 内で書いた内容と完全一致することを assert した。これで
管理領域を席へ開放せずに編集を launcher が保存する経路を直接検証した。

二番目の fixture は実 sandbox 内の .NET `FileShare.None` ロックで、競合中は
質問作成前に拒否、解放後は取得成功、許可外 queue 書込み拒否、実際の
`magi ask` / `magi answer` の往復成功を確認した。三番目は読取り専用席の
launcher 保持ロックについて、外部競合時には prompt / CLI を開始せず、
実行中には外部取得を拒否し、終了後に再取得できることを確認した。

### 修正、限界と配備・復旧

launcher の base fetch は `SeatLease::operation_with` で調停し、成功時だけ
remote / branch / SHA / 取得時刻を保存する。初回・resume・fallback・waiter は
この保存値を渡し、resume のために基底を更新しない。旧 run の証跡は欠落のままとし、
tracking ref から取得成功を捏造しない。既存の base-sync 処理を使う。

共通 invoke 経路は、取得済み固定基底、Git 更新拒否時の launcher 保存、必須操作が
拒否されたときの操作・対象・エラーを含む報告を説明する。質問文字列の判定で
正当な仕様質問を禁止しない。scope artifact はあくまで要求設定であり、実効設定の
証明とは区別する。sandbox bypass、chmod、広い state / home 許可は追加しない。
依存取得や rebase 続行など、席内で必須の操作が拒否されるケースは引き続き
インフラ blocker であり、これらすべての対応を今回実証したわけではない。

配備は次の順序で行う。

1. 親グラフの全体検証・`cargo make check`・必須レビューを完了し、通常の承認で
   merge / 配備する。候補内で本番 graph を再帰起動しない。
2. README の既存「Update & restart」手順で安全に更新し、二重 daemon を作らない。
   実行イメージと版、起動時刻を確認する。ディスク上の更新だけでは既存席は変わらない。
3. 配備先の実 CLI で上の三 fixture を再実行し、新席の debug `spawning agent`
   argv、access artifact、保存セッションの実効 policy を照合する。固定 SHA の
   読取り、launcher 保存、coordination、質問経路と許可外書込み拒否を確認する。
4. 元タスクの現在の owner / run / hold を `magi task show` で確認し、既存 owner の
   復旧経路を一つだけ使う。稼働中 graph の並行 resume、代替タスクの新設、元実装の
   肩代わりは行わない。旧 run に取得証跡がなければ検証済み fetch と準備経路を経る。

今回のホスト検証は成功したが、production バイナリの配備・daemon 再起動と
新しい production 席の確認は実施していない。全体 gate とレビューは親グラフの担当。

### 旧形式の rebase 停止記録の移行

配備後は元の owner / run の既存 continuation を使う。`index_outside` がない記録は、
launcher が保存済み `REBASE_HEAD` の親、現在の `HEAD`、`REBASE_HEAD` から
`git merge-tree` で三者マージを再計算する。競合パスが保存記録と一致し、競合外の
index が再計算結果と一致した場合だけ、基準と停止点を同じ run に保存する。
既存の修正記録や消費済み予算は残す。元の index、解消済みファイル、ブランチ、
sequencer はこの計算では更新しない。検証後の staging / continuation は通常の
launcher 経路で行い、新しい fixer round を要求しない。

計算は調停 lease とキャンセル時のプロセス回収を通す。再取得した lease 内で
停止点と index を再検証してから記録を保存する。不一致、未対応のマージ形状、
Git 書込み拒否はインフラ停止とし、保持中の内容を削除せずに理由を記録する。
現在の index を手作業で基準として書き込んだり、別 driver で元の run を進めたり
しない。移行不能時は元の所有者が不一致の原因を確認し、保持された内容の修復を
判断する必要がある。
