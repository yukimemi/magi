<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/magi/main/assets/logo-dark.svg">
    <img src="https://raw.githubusercontent.com/yukimemi/magi/main/assets/logo.svg" alt="magi — multi-agent implementation competition" width="540">
  </picture>
</p>

<p align="center">
  <img src="assets/demo-desktop.gif" width="800" height="480" alt="Desktop demo, two-pane layout: Asuka's chat files an uploader retry task, the Queue list and task detail sit side by side, three blind candidates compete and the judges' tally and review rounds show in the detail pane, then the owner answers an agent question and approves the pull request merge.">
</p>

<p align="center">
  <img src="assets/demo.gif" width="360" height="646" alt="Phone demo: Asuka's chat files an uploader retry task; three blind candidates compete, judges rank them and reviews fix a finding; the owner answers a question and approves the pull request merge.">
</p>

# magi

**A multi-agent implementation competition, as a CLI.**

Three agents solve the same task in isolation. Three judges rank the results
without knowing who wrote what. If they disagree, they argue, and then vote
privately. The winner survives a review + real-machine verification loop before
anything is allowed to merge.

The premise is that one agent's *first choice of approach* is the part no amount
of reviewing fixes. Review a bad design carefully and you get a well-polished
bad design. So magi does not review one implementation — it holds an election
between several, and only then reviews.

<p align="center"><img src="assets/flow.svg" width="760" alt="magi flow: the queue loop (conductor), one competition from prep to land, and the standing chat / release bump"></p>

<details>
<summary>Mermaid source (for editing; keep <code>assets/flow.svg</code> in sync)</summary>

```mermaid
flowchart TD
  subgraph Q["queue loop — `magi serve` (outside any one run)"]
    direction LR
    conductor["conductor ×1 per poll cycle\n[roles] conductor"]
  end
  conductor -.->|arranges blocked_by / recovery, then hands off| prep

  subgraph Run["one competition — `magi run`"]
    direction TB
    prep["prep: worktrees + blind hook\n(no agent)"]
    advisor["advise: advisors ×A, read-only\n[roles] advisors (falls back to judges)\n[graph] advise, advisors"]
    synth["advise: synthesis ×1, blends the advisors\n[roles] synthesizer"]
    impl["implement ×N\n[roles] implementers\n[graph] implementers"]
    judge["judge ×M, blind\n[roles] judges\n[graph] judges"]
    delib["deliberate\n[roles] judges\n[graph] deliberate_rounds"]
    vote["final vote, private\n[roles] judges"]
    tally["tally\n(mechanical, no agent)"]
    fold["fold losers\n(no agent)"]
    review["review ×R + E2E + fix, bounded\n[roles] reviewers, fixer\n[graph] reviewers, review_rounds"]
    gate["gate commands\n[verify] gate — shell, no agent"]
    merge["merge\n[graph] land, land_approval — no agent"]
    land["land loop: watch CI, fix, ask to merge\n[roles] fixer, reused"]

    prep --> advisor --> synth --> impl --> judge
    judge -->|first choices agree| vote
    judge -->|split| delib --> vote
    vote --> tally --> fold --> review --> gate --> merge
    merge -->|merge = pr, land = true| land
  end

  subgraph Standalone["standing chat / release bump — `magi chat`, `magi bump` (outside any run)"]
    direction LR
    chatter["chatter ×1 per turn\n[roles] chatter"]
  end
```

</details>

Every box above that spawns an agent has a `[roles]` field to pin it to a
specific one:

| Seat | Runs | `[roles]` field | Other knobs | Pinnable? |
|---|---|---|---|---|
| conductor | once per poll cycle | `conductor` | — | yes |
| advisors (design deliberation) | ×`advisors` (default 3) | `advisors` *(unset falls back to `judges`)* | `[graph] advise` on/off, `[graph] advisors` count | yes |
| advise-synthesis (blends the advisors into one brief) | ×1 | `synthesizer` *(unset falls back to a claude seat, else roster order)* | — | yes |
| implementers | ×`implementers` (default 1) | `implementers` | `[graph] implementers` | yes |
| judges (rank / deliberate / vote) | ×`judges` (default 3) | `judges` | `[graph] judges`, `deliberate_rounds` | yes |
| reviewers | ×`reviewers` (default 3) | `reviewers` | `[graph] reviewers`, `review_rounds` | yes |
| fixer (review loop and land loop) | ×1, only when findings block | `fixer` *(unset falls back to the winner's own seat)* | — | yes |
| fixer (graft, between tally and review) | ×1, only with 2+ candidates and judge-named strengths in the losers | `fixer` *(same chain; unset uses a fresh seat for the winner's author)* | `[graph] graft_runner_up` on/off (default on) | yes |
| chatter (`magi chat`, `magi bump`) | ×1 per turn | `chatter` *(unset falls back to a claude seat, else roster order)* | — | yes |
| language_judge (PR title / body English check) | ×1 per check, only when set | `language_judge` *(optional; unset spawns nothing and the built-in heuristics decide, as they do after a failed, timed-out or unparsable call)* | `[graph] github_text_guard` | yes |
| tally / fold / gate / merge | n/a — no agent seat | — | `[verify] gate`, `[graph] land`, `land_approval` | n/a |

**Role lists are also an ordered backup list.** An array-valued `implementers`,
`judges` or `reviewers` may be longer than its `[graph]` seat count. With
`[graph] implementers = 1` and `implementers = ["a", "b", "c"]`, only the first
entry sits on a seat (`judges` starts one place along, because its list is
rotated). The rest are spares: when a seat's agent fails or hits quota,
`graph::pick_successor` walks the full, untruncated roster
(`ResolvedRoles::implementer_roster` / `judge_roster` / `reviewer_roster`) from
that seat's position to the right, preferring agents no other seat holds and
trying each agent at most once per call. A reviewer seat can additionally
retry an agent that failed it earlier once the forward entries are used up.
This is separate from the fallback chains of `synthesizer`, `chatter`,
`fixer` and `language_judge`.

## What makes the judging blind

A judge that knows which model wrote a candidate stops grading the patch and
starts voting on the model's reputation. Four mechanisms prevent that:

| Mechanism | Where |
|---|---|
| Candidates are `A`/`B`/`C` by seeded shuffle; each judge gets its own presentation order | `src/blind.rs` |
| Branches are named after the **label** (`magi/<run>/B`), so a judge can `git show` a candidate without learning its author | `src/run.rs` |
| A per-worktree `commit-msg` hook deletes `Co-Authored-By:` / `Generated with …` before they can land | `src/blind.rs`, `src/git.rs` |
| The same trailers are stripped again at presentation time, and uncommitted work is rescued under a neutral identity | `src/blind.rs`, `src/git.rs` |

The facilitator is **code, not an agent**. magi assigns the labels, relays the
transcript, and collects the final votes one-to-one. A moderator that never
learns an author cannot leak one, and there is no seat for a model to be
persuaded out of neutrality.

Two more consequences of that design:

- **Seats, not agents.** Conversations are keyed by seat (`impl-B`, `judge-2`),
  never by agent id. The model that wrote candidate B and the model sitting as
  judge 3 may be the same model, and it still cannot recognise its own work,
  because the judge seat's conversation never contained the implementation.
- **Private final votes.** After deliberation, each judge is asked separately,
  told that nobody sees the answer, and no running tally exists to drift toward.

Vendor names appearing in the *patch body* are a different problem: redacting
them would corrupt the artifact under judgement. magi scans for them, records
what it found, and `blind.on_leak` decides (`warn` by default, or `redact` /
`fail`).

## Conversation continuity

Deliberation and the review loop are multi-turn. Re-sending three patches on
every turn is both expensive and worse — a judge should argue from what it
already said. magi therefore keeps one CLI conversation per seat:

| CLI | open | resume | notes |
|---|---|---|---|
| `claude` | `--session-id <uuid>` (magi mints it) | `--resume <uuid>` | addressable before the first turn |
| `opencode` | `run --format json` reports `sessionID` | `run -s <id>` | id captured from the event stream |
| `agy` (Antigravity) | `--output-format json` reports `conversation_id` | `--conversation <id>` | `--print-timeout` is raised to the node budget |
| `codex` | `exec --json` reports `thread.started.thread_id` | `exec … resume <id>` | `resume` is a subcommand: every option precedes it. The prompt goes on stdin |
| `omp` (oh-my-pi) | `-p --mode=json` reports `id` on its `"type":"session"` line | `-p … --resume <id>` | the answer is the last non-empty assistant text block: a turn that ends on a tool call emits **no** `agent_end` line to read it from |

When a seat has no live conversation — sessions disabled, or a first turn that
never reported an id — magi re-sends the full context instead of letting the
agent argue from memory it does not have. Set `graph.sessions = false` to force
that everywhere.

Gemini CLI is deliberately **not** supported: Google retired the standalone
client for individual accounts in favour of Antigravity, so the adapter would be
dead code. `kind = "command"` covers anything else.

## Install

```sh
cargo install magi-cli
```

magi drives *subscription CLIs*, not API keys: `claude`, `opencode`, `agy`,
`codex`, `omp`, or
any command you point it at. It spends your existing plan and nothing else.

## Use

Help and usage errors are coloured; `--no-color`, `NO_COLOR` or a pipe makes them plain.

```sh
magi                          # the observation deck (see below)
magi init                     # write a starter magi.toml
magi doctor                   # check CLIs, repo, roster, seat assignment
magi run "add retries to the uploader"
magi run --file task.md
magi run --issue 42           # task from a GitHub issue, via gh
magi run --resume 20260830-153012-a1b2
magi show                     # full report for the latest run
magi list
magi stats                    # win rates, reviewer precision, E2E yield
magi fold --all               # remove a run's worktrees and branches
magi self-update              # or let the background check tell you
magi task add "port the retry logic to the uploader"
magi task list                # the backlog `magi serve` drains
magi task done <id>           # work that landed some other way
magi serve                    # run the queue unattended
magi web                      # the phone UI, over Tailscale — Chat is where new work starts
magi repos                    # local checkouts found under [repos] roots
magi answer                   # what an agent is waiting to hear from you
```

`magi run --resume <run-id>` continues only work that was interrupted or
stopped at a resumable boundary. A completed failed final gate is not retried
by resume: magi preserves the recorded command output, reports the run as
`blocked`, and exits unsuccessfully. Fix the cause and start the intended
follow-up run instead; this avoids presenting an old failed command as a new
verification attempt.

**Every run is a task.** `magi run` and `magi review` first file a task in the
queue (the same path as `magi task add`) and print its id - open it with
`magi task show <id>`. If a `magi serve` / `magi web` loop is alive on this
machine the task is filed `urgent` (its own one-slot lane, other runs are not
paused) and the command follows the loop's run and exits with its outcome;
otherwise the command runs the task itself, holding its claim so a loop that
starts meanwhile cannot take it. `--merge`, `-c`, `-j`, `--review-rounds`,
`--seed` and `--config` ride on the task, so they hold either way. A run that
fails standalone leaves its task `held` (`magi task release` retries it).

To recover an existing task PR from an implementation the operator has chosen,
use `magi task recover-pr <task-id> --pr <PR-URL> --head <full-public-SHA> --disposition land`
(or `--disposition review` for local review only). Optional `--source-head <full-SHA>`
preserves a reviewed fix independently of the public-head lease. See
[the recovery contract and validation setup](docs/existing-pr-recovery.md).
This queues a single review candidate from the fetched public commit with its
complete ancestry, preserving the task's instructions. The selection overrides
historical review branches, resume pins and references extracted from prose.
The PR must be the unique open PR for `magi/task-<task-id>` in the task's
repository, targeting the configured base. Active or unknown previous
publishers prevent takeover; historical source branches and worktrees remain.

Recovery synchronizes through the ordinary current-base path and still needs
verification, review and owner merge approval. Publication checks ownership
again and leases the public update to the approved head (or this run's last
successful push). A moved head requires a new explicit selection. Startup and
candidate preparation failures leave one actionable hold rather than automatic
new competitions; correct the cause before releasing or selecting a source
again. Partially created recoveries are recorded in run history for inspection.


`magi run` starts spending money, so an instruction whose first word names a
subcommand is refused as a probable typo: there is no `magi run show`, and
without the guard `magi run show 3cbf` opens worktrees and pays agents to
implement the sentence "show 3cbf". Write `magi run -- show 3cbf` when that is
genuinely the task.

A run branches off the **base branch's tip**, not off your working copy, so it
starts whether or not you have uncommitted work — and that work is not part of
the competition. `magi serve` would otherwise decline every task for as long as
you had something in progress, which is most of the time. magi says so in the
log when the tree is dirty, because someone watching a candidate fail to use a
change they just made deserves to know why.

## The observation deck

Bare `magi` (or `magi tui`) opens every run in one screen, refreshed from disk
once a second, so a competition can be watched instead of polled:

```text
 magi   2 runs  1 active  1 done  0 attention  |  filter: all
┌ runs ─────────────────────────┐┌ report ────────────────────────────────┐
│> reviewing   a1b2  add retries ││magi run 20260830-153012-a1b2  reviewing│
│  ready       50f1  fix the …   ││  candidates                            │
│                               ││  A  opus    3 files, 2 commits  <- winner
└───────────────────────────────┘└────────────────────────────────────────┘
 j/k move  Tab pane  J/K scroll  a filter  r refresh  o open dir  ? help  q quit
```

| key | |
|---|---|
| `j` `k` `↓` `↑` | move in the focused pane |
| `Tab` | switch pane (runs / report) |
| `J` `K`, `PageDown` `PageUp` | scroll the report |
| `g` `G` | newest / oldest run |
| `a` | cycle filter: all → active → attention → done |
| `r` | refresh now |
| `o` | open the run's directory in the OS file manager |
| `?` | help |
| `q` `Esc` `Ctrl-C` | quit |

It is **read-only on purpose**: the runs are the record of what the agents did,
and a keystroke that could rewrite one has no business being a `j` away from
browsing. Cleanup stays in `magi fold`.

Piped or in CI, bare `magi` does not raise an alternate screen — it prints the
latest run's report instead, so `magi | head` behaves.

## Running unattended

A competition takes tens of minutes, almost all of it agent latency. Sitting in
front of that is the wrong job for a human, so magi has a queue and a loop that
drains it:

```sh
magi task add "port the retry logic to the uploader"
magi task add --file tasks/rework-config.md --priority 5
magi task add --issue 42
magi serve                    # take the next task, run the graph, repeat
```

One task is one JSON file under `<data_local>/magi/queue`, so the backlog is
readable, editable, and greppable with the tools already on the machine, and a
daemon killed mid-run leaves a queue the next one picks up.

`magi serve` runs **one competition at a time** on purpose. The graph is already
parallel inside — candidates times judges — and two graphs at once doubles the
burn on the agent-CLI quota that is the real constraint.

`magi task add --urgent` is the escape hatch for the rare task that genuinely
cannot wait. It dispatches the moment it is runnable, through one extra,
temporary slot, alongside whatever is already running — the run already in
flight is never paused, parked, or restarted to make room. It is not a way to
raise `[daemon] max_concurrent_runs` for good: that setting, and its default
of one, is unchanged, and while an urgent task is in flight quota burn can
briefly reach `max_concurrent_runs + 1` runs at once instead of just one.
Change an existing task with `magi task urgent <ID>` to enable urgent, or
`magi task urgent <ID> --clear` to disable it. IDs may be unambiguous short IDs.
`magi task show <ID>` reports the saved enabled/disabled state. In the Web
Queue, open **Actions** and press **Urgent: off/on**; the task detail's action
sheet has the same toggle. The URGENT badge reflects the saved setting.
Only unclaimed queued or held tasks can change urgent. Changing a held task
keeps it held, with its reason and attempts intact; release it separately when
ready. Claimed, running, and other task states are refused; wait for the claim
to release and requeue or hold the task before retrying. This changes neither
priority ordering nor the interrupt park/resume request.

Two `--urgent` tasks do not both jump the queue — the second waits for the
first to leave the urgent slot, same as any other pair of tasks waits for a
free ordinary one. `--urgent` and `magi task interrupt` (`[daemon]
pause_for_interrupts`) do not compose, in either direction: while an urgent
run is in flight, interrupt cannot start (it only ever begins once exactly
one run is in flight); and while an interrupt's own park/resume handoff is
under way — for that same task or an unrelated one — an urgent task waits
out the same gate an ordinary task would, because the handoff's own "one run,
never two" guarantee has to win over urgent's own "never wait" one. Both are
opt-in and, together, a rare combination; `pause_for_interrupts` is off by
default.

| verb | |
|---|---|
| `magi task add` | file work; text, `--file`, `--issue`, or stdin |
| `magi task list` | the backlog, newest first |
| `magi task show <id>` | one task in full |
| `magi task urgent <id> [--clear]` | enable or disable urgent on an unclaimed queued or held task |
| `magi task implementers <id> <count>` | change implementers on a queued or held task (1 = Solo, 2–3 = competition) |
| `magi task hold` / `release` | park work, or give it a real second chance |
| `magi task done` | mark it finished, for work that landed by a route the loop did not see |
| `magi task rm` | delete it |

`done` and `release` are one keystroke apart and do opposite things. Reach for
`done` when the work is already in `main` — merged by hand, or merged by a run
that recorded its own merge as a failure. `release` would put the task back in
line and pay for the whole competition again to redo it.


A queued or held task's implementer count can also be saved from its Queue card
or task actions sheet. This changes only that task, preserving its ID,
instruction, attachments, creation metadata and run history. Running and other
states cannot be changed. Switching to or from Solo may start a fresh run when
an earlier run used a different count; earlier candidate work is not reused in
that case. Explicit `magi review` tasks remain reviews of their requested branch.
Count edits on tasks without run overrides continue inheriting the daemon's
options. Existing run overrides, including empty and count-only ones, retain
configuration isolation across Solo changes. Conversation preferences and
repository defaults are unchanged.

### Agents file their own work

`magi task add` is not a human-only command. Every agent the graph spawns gets
`MAGI_RUN` and `MAGI_NODE` in its environment, so an implementer that notices
something worth doing but out of scope can file it:

```sh
magi task add "the config loader re-reads the file on every lookup"
```

The task records `implement@a1b2` as its source rather than `human`. That
attribution comes from the environment `agent::invoke` sets, which no flag can
forge by accident — which is what makes "most of the backlog was filed by
agents" a measurement rather than a claim. It is also the whole point: the CLI
is the operating surface, and the agents are its users as much as you are.

### Bounded on purpose

An autonomous loop that retries forever is a way to spend money on a task that
cannot succeed. Every attempt is counted; a task that burns its attempts becomes
`held` and waits for a person, not for another agent.

A **quota stall is refunded**. When the agent CLIs hit their rate limit the
judging panel collapses, the run stops as `stalled`, and the task goes back in
line *without* spending an attempt — a quota window closing at 4am must not
leave a backlog of tasks that were never actually judged.

Only a rate limit earns that refund. A judging panel can also collapse because
the judges answered with the wrong shape — which is ordinary flakiness, can
recur on every attempt, and is charged to the task, so the attempt counter
still bounds it. Refunding *that* would take the bound off the loop entirely
and pay for a fresh implement wave every time.

## Working out what to build, with someone

A task file without completion criteria produces a competition whose candidates
cannot be compared, and you find out forty minutes and several dollars later.
So the first step is a conversation — Chat, the standing conversation on the
phone UI (`magi web`):

```sh
magi web
```

Open Chat, talk the idea through with an agent that can read the repository
and run commands, and once the shape of the change is settled, tell it to file
the work. The agent runs `magi task add --solo` on your behalf — the same
command you would type yourself — and the conversation stays open, so any
number of tasks can come out of it over time rather than ending the moment one
does.

## Panels: the agent formats its own confirmation screen

One line of prose is not enough to decide anything. So an agent can hand over
a page it wrote itself:

```sh
magi ask --summary "Which cache key shape?" \
         --choice "u64 hash" --choice "string key" \
         --panel panel.html --asset before.png
```

The panel is the agent's own HTML and CSS — a diffstat table, a coloured diff,
a screenshot — rendered on your phone under the question. Attachments are
copied into the question, so a panel still renders after `magi fold` has
deleted the worktree it was written in.

### Why this is safe

Rendering someone else's HTML in your browser is the thing this UI otherwise
refuses to do: nothing from the API is ever put into `innerHTML`, and even an
href out of a run record goes through a scheme check. A panel is the exception,
and it is only acceptable because of three things together:

- The frame is `<iframe sandbox>` **with no tokens at all**. No JavaScript
  runs. It cannot reach the page around it, your cookies, or `localStorage`.
  There is one function in the client that builds it and a comment forbidding
  anyone from adding `allow-scripts`.
- The panel is served under a **strict CSP**:
  `default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; …`.
  Inline CSS is allowed, because formatting is the point. **Everything that
  reaches the network is denied**, so a panel cannot phone home through a
  remote image or a beacon — verified with the browser's own violation log.
- Assets come from magi, by bare filename, from that question's own directory.
  The name must match `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`, is validated on the
  way in and again on the way out, and an `.svg` is served
  `Content-Disposition: attachment` so it can only ever be an `<img>`.

A panel that will not load says so and leaves the choices usable: you still
have to be able to answer.

## Approving a merge

With `land_approval` on — the default whenever `land` is on — magi asks before
it merges, and the question carries a panel with the whole case: the diffstat
as a table, the diff itself with a `+`/`-` gutter so it reads without colour,
the checks, the review comments that were addressed, the commits being squashed
and **the subject the merge will use**, because you are approving that too.

The diffstat comes from `git diff --numstat`, not `--stat`: the `+++---` bar in
`--stat` is scaled to terminal width, and fabricated numbers have no business
in an irreversible decision.

**Silence is a hold.** An unanswered approval never merges, and neither does
anything other than the word `merge`.

A standing conversation can answer an exact owner decision even when the
question was consulted elsewhere or never consulted. Use
`magi answer <full-question-id> --reply '<exact offered choice>' --quote '<whole latest owner send>'`.
Direct instructions use `<full-question-id>: <exact offered choice>`. Explicit
batch authorization, including `おっけーそれぞれ回答してー`,
`ありがと！じゃあそれぞれ回答してー！` and `answer each as recommended`,
binds the immediately preceding persona reply. Recommendations use one list row
`<question-id>: <exact choice>` per question, or separate ID and choice table
cells. Duplicate/conflicting rows, conditions, quoted examples and generated
messages confer no authority. Unique short IDs remain supported in recommendations;
full IDs remain required for guarded individual non-hold instructions.

For low-risk internal recovery, structured resume and keep-held actions support
`resume-run` / `resume run` / `再開` / `実行を再開する` and
`keep-held` / `hold` / `保留` aliases independently of localized labels. Unique short question IDs work for explicit `<id>: <action>` replies
(and `<id> resume run` without a colon).
The assistant normalizes the alias before both authorization and answer storage;
an explicit recovery reply can use persisted owner evidence without `--quote`.

For natural conversation, the assistant first presents a single recommendation
row, for example `<question-id>: resume-run`. Publication saves the canonical
choice, action, target run and question fingerprint with that persona reply.
On the next owner turn, the assistant interprets contextual approval such as
`はい、それで進めてください` or “yes do that”, then runs
`magi answer <id> --reply resume-run --recovery-consent '<entire real latest owner send>'`.
`--recovery-consent` records the assistant's interpretation; it must reproduce
the full persisted send exactly. `--quote` is optional here; if provided it must
match the same full send. Operators need not type internal IDs or labels again.
This mechanism uses the existing question/evidence locks and directory grants.
It never synthesizes an owner quote or retrospectively binds old recommendations.
Generated notices can interleave without authorizing or retracting a decision.
Later owner sends, changed targets, multiple recommendations and ambiguous intent
require a fresh interpretation or clarification. Do not repeat confirmation after
clear approval of the single current recommendation.

Only structured internal resume/hold effects qualify, with issuer-specific
support for legacy triage keep-held (its resume may requeue and is excluded).
Requeue, completion, destructive effects, merge and public posting retain their
separate consent requirements. Recording recovery does not create a competition,
release ownership or approve a pending merge: the existing daemon applies the
answer once, validates the current target and keeps all approval boundaries.

Recommendation manifests persist the reply fingerprint and each question's
choices, actions, PR/head and content fingerprint at publication. Changed facts,
newer queued owner sends and already answered/abandoned questions are rejected.
Old history without a manifest can still establish a safe hold; new non-hold
batch authority requires a persisted manifest rather than guessing old targets.
The question retains its original consultation and the originating owner send,
turn/message positions and recommendation fingerprint. Closed questions cease
being pending consultations. Concurrent writers settle once under the question
lock and retain the winning evidence; non-hold cross-chat answers wait for an
active original consultation to finish. The existing owner/daemon applies the
answer; no additional task or driver is created. An authorized standing-chat
dismissal additionally grants the queue and exact run directories required for
claims, the ownership lock and retirement writes, while keeping the sandbox
enabled. `use my text` in the same consulted chat requires current bound owner
evidence with the literal replacement title; it cannot reuse old thread text.

| Decision / execution effect | Individual standing instruction | Explicit batch | Existing execution requirements |
| --- | --- | --- | --- |
| Hold | `<id>: hold`; same consulted chat also accepts `hold` | Yes | No structured action on hold; no closure implied |
| Dismiss obsolete approval | `<id>: dismiss: end obsolete approval without changing PR` | Yes | Dead original driver, no armed auto-merge, matching run approval; PR unchanged |
| Fix findings and re-review | `<id>: fix findings and re-review` | Yes | Offered/recoverable repair, same run and PR, bounded review and fresh approval |
| Ordinary queue/conductor/agent questions; retry, rerun, requeue, resume, release recovery | `<id>: <exact choice>` | Yes, except destructive effects | Existing claims, operator holds, task generation and recovery state |
| Free-text questions | `<id>: <complete answer>` | No: no offered choice to bind | Existing asker/waiter dispatch |
| Consultation task add / release / hold / done / priority | Latest whole post-delivery owner send: `task-operation: <typed JSON operation>`, quoted whole with `magi task --quote ...` | No | Full resolved task id and exact arguments; existing queue claims/state checks; add uses this conversation's repository, positional text and optional `--solo` only |
| Outcome outside the offered choices | `magi answer <id> --reply-other <entire latest owner send> --quote <same send>` | No | Saves an evidence note and ends consultation only; question remains Open, waiter remains held, no answer or choice action |
| Withdrawal / PR closure / `reimplement:<agent>` | `<id>: <exact consequence token>` | No | Existing closure confirmation, owner claim and replacement transitions |
| Triage discard, dependency discard/detach, divergence overwrite, structured `done`, conductor complete/configure | `<id>: <exact consequence token>` | No | Individual explicit consequence; existing consumer checks |
| Merge (land and release-watch approvals) | `<id>: merge <displayed head>` | No | Recorded PR/head (legacy release cards recover only their matching watch record); final head, review, gate and owner checks still apply |
| Guarded `post original` / `use rewrite` | `<id>: <choice> <question/content fingerprint>` (reported by CLI) | No | Explicit saved content binding; final posting gate |
| Guarded `use fallback` | `<id>: use fallback` | Yes | Existing safe fallback and posting gate |
| Guarded `use my text` | `<id>: use my text <question/content fingerprint>` followed by a newline and the literal replacement title | No | Persisted owner title and final posting gate; a choice token alone supplies no title |

For consultation task operations, the CLI accepts a closed typed instruction rather
than inferring execution arguments from prose. For example, the owner's complete
send `task-operation: {"add":{"instruction":"Reimplement from scratch","solo":true}}`
authorizes `magi task --quote '<that entire send>' add --solo 'Reimplement from scratch'`.
Other payloads are `{"release":{"task":"<full task id>"}}`,
`{"hold":{"task":"<full task id>","reason":"Wait for replacement"}}`,
`{"done":{"task":"<full task id>"}}`, and
`{"priority":{"task":"<full task id>","priority":5}}`, each prefixed with
`task-operation: `. JSON whitespace does not matter; every field does. A missing,
stale, queued, pre-delivery, generated, conditional or mismatched instruction is
refused. The chat presents the required instruction once when natural language
does not bind it; it must not promise unsupported writes. Task operations need
separate owner instructions and should precede `--reply-other`.

Only `magi answer` settles a question. `--reply-other` deliberately does not
wake its waiter or interpret the decision as a choice action; it prints that the
question and wait remain. Its note retains the conversation, send and literal
owner evidence. Answered/abandoned questions and ended consultations no longer
restrict the **next** turn, provided no other consultation remains active.
The current invocation retains its original restrictions throughout. Additional
queue writes never authorize merge, discard, withdrawal, public posting or other
mutations; existing answer-specific effect rules above still apply.

The launcher grants only question/evidence locks, queue storage, and exact run
history directories needed for an evidenced `done` (plus existing answer-effect
grants). It does not grant the whole state root or bypass the sandbox, including
when ordinary chat editing is enabled. The supported CLI is an authorization
boundary; filesystem grants cannot prevent an agent from directly editing an
allowed store, so prompts still prohibit those writes.

All three consultation destinations (same chat, other chat, never consulted)
use this matrix. The CLI and standing turn write grant share evidence validation;
consulted-chat legacy single decisions keep their existing quote policy. The
owner's terminal CLI and web question-card clicks remain direct owner decisions
and use the same question lock, offered-choice validation and downstream
execution transitions. A web chat send uses standing-conversation validation.
Deputies settle only their own question seat, using the latest persisted owner
thread and the same execution-effect inventory. A deputy's replacement-title
instruction requires the same question/content fingerprint and literal title as
a standing-chat instruction; hold and retractions cannot supply posting consent.
A deputy's one-question thread
has no cross-question recommendation manifest: generic batch words alone cannot
select a choice there; the active standing conversation's manifested evidence
uses the answer CLI for those questions. Triage/release/daemon/waiter
consumers apply recorded answers, never infer authorization from recommendations.
Missing head, content or consequence confirmation is reported as such, rather
than a consultation-destination error. For chat answers, operator-held refusal
is specific to approval questions; ordinary triage, dependency and recovery questions remain
answerable, and recording their answers does not itself lift the task's hold.

When review hands off with unresolved Major findings or final rejecting votes,
the decision also offers **fix findings and re-review**. Selecting it runs one
repair and a fresh bounded review and gate on the same PR branch, under the
existing run's ownership. The question ID, head and original review evidence
are retained in the run's repair history. Even a repair that changes nothing
is reviewed again. This action invalidates the earlier approval and disables
auto-merge; updated results always require a fresh owner decision, including
when `land_approval` is off. **Hold only holds** and never starts a repair.
The CLI accepts the same choice with
`magi answer <question-id> --reply "fix findings and re-review"`.
This command records the choice and prints its execution status. A running
`magi serve` / `magi web` resumes the existing run; without a daemon, use the
printed `magi run --resume <run-id>` command to start the repair.
Legacy merge/hold-only cards are upgraded on read, with the same durable
choices used by the Web API, CLI and standing chat. Repair resumes the latest
owning task and run; it never files a competing task. A held owner is released
with a pin to that run, and repeated delivery of the same question is harmless.
Existing chat consultations receive the current choices on their next turn.
Repair obeys normal concurrency and quota waits rather than using the
merge-decision priority lane. Missing or unsupported owners produce recovery
guidance; restore the existing owner instead of adding another task.
If an installed server still shows only merge/hold, update and restart it from
a release containing these changes, then reload the card. The version alone
does not distinguish a release from later source builds at the same version.
Repair cycles are capped by `land_rounds`, with each new review bounded by
`review_rounds`. A changed PR head, dirty worktree, failed repair or interrupted
fixer holds with recovery guidance in `magi show`; an interrupted fixer is
never dispatched a second time automatically.

## Chat: a standing conversation, turn by turn

Chat runs as a **turn-based conversation**: your message, one headless agent
turn, repeat. Each turn resumes the CLI's own conversation (`claude -p
--resume`, `opencode run -s`, `agy --conversation`), so a turn sends only your
new sentence rather than replaying the transcript and paying for it again.
Your message is written to disk **before** the agent is invoked, so a quota
window or a crash cannot lose something you typed.

While a reply is running, Chat accepts the next message as one durable pending
draft instead of starting a second CLI turn. The web composer shows that draft;
use **Resume queued draft** to run it unchanged, **Edit text** to replace only
its text while keeping its attachments, or **Clear** to discard the exact draft
you are viewing. Clear and Edit use a snapshot check, so they refuse rather
than overwrite a draft changed by another action. A server restart cannot
resume an in-flight CLI process, but the recorded transcript and pending draft
remain on disk and are visible after reopening Chat. Use **Resume queued
draft** for either text-only or image-only recovery; do not resend the stored
message. Editing an idle recovered draft starts its drain, but unchanged drafts
have their own Resume action.

The conversation runs with writes disabled by default: an agent that had
already edited the repository mid-conversation would make a later
competition's diffs unjudgeable. `[talk] allow_write = true` lifts that for a
repository that is never entered into one — a dotfiles or personal config
checkout — so a one-line fix does not have to go through the queue to land.

Unlike a one-shot interview, Chat does not end when a task is filed: the agent
runs `magi task add --solo` from inside the conversation, tells you the task
id, and the conversation stays open for whatever comes up next.

## When an agent needs you

```sh
magi ask --summary "Which storage backend?" --choice SQLite --choice Redis
```

That is the command an *agent* runs, mid-task, instead of guessing. It blocks;
the question appears on your phone with the choices as buttons; the answer goes
back on the agent's stdout. From a terminal, `magi answer` does the same job.

A question is attributed to the seat that asked it — `run 9fb7 · node implement
· seat impl-A` — because "who wants to know" is the first thing you need in
order to answer. Runs blocked on an unanswered question are marked as such in
the runs list: a parked run consumes nothing and progresses never, so being
noticed is the only thing that moves it.

### Asking back

Answering isn't the only move. If the question doesn't make sense yet, say so
instead of guessing at an answer:

```sh
magi answer ab12 --say "why not Postgres, it's already a dependency?"
```

(the phone's question card has the same "ask back" box). `magi ask` on the
agent's side returns at once — exit code 0, your words on stdout — and the
agent replies on the same question rather than starting a new one it would
have no context for:

```sh
magi ask --thread ab12 --summary "Postgres needs a server to run; this is a CLI tool" --choice SQLite
```

That appends the reply and waits again. The question never leaves the card it
started on — the phone shows both turns stacked under the original summary —
and it counts as one open question throughout, however many times you go back
and forth. The one thing that changes: while your last word is waiting on the
agent, the phone locks the answer buttons and says so, because there is
nothing to decide until it replies. You won't be paged again for a reply that
lands within five minutes of your own message — long enough to mean "still
reading" — but a slower one still notifies, in case you stepped away.

Set `[notify] command` to be told out of band:

```toml
[notify]
command = ["ntfy", "publish", "magi", "{summary} — {url}"]
```

`{summary}`, `{run}` and `{url}` are substituted into the argv, never into a
shell string, so a question containing `; rm -rf` stays one argument. `{url}`
comes from `MAGI_WEB_URL`, which `magi web` prints at startup — a run cannot
discover the address another process bound. Unless the configured URL already
has a fragment, questions link to Questions and notices link to Notifications.
Question notifications identify the question and how to answer it; a notice
has no reply of its own. Related open questions have a separate response link
in the notification centre. A notification that fails is logged
and ignored: a broken webhook is not a reason to throw away an implementation.

The same command also fires for the web UI's notices (the header bell): a run
that ended badly, a held task, a failed release bump, a merge with red checks.
It fires when a notice is new, its message changed or its severity rose - not
for an identical repeat, a dismissed notice, or one an open question already
pages for - with the notice's message as `{summary}` and, when it links to a
run, that id as `{run}` (empty otherwise).

A placeholder is replaced verbatim inside one argv element: nothing escapes it
for JSON or anything else. For a Discord webhook, a naive
`-d '{"content":"{summary}"}'` breaks on a summary containing a quote or a
newline, so build the body in a wrapper that takes the summary as an argument:

```sh
#!/bin/sh
# discord-notify.sh <summary> - Discord rejects content over 2000 characters
body=$(jq -n --arg content "$1" '{content: $content}') || exit 1
exec curl -sS -H 'Content-Type: application/json' -d "$body" "$DISCORD_WEBHOOK_URL"
```

```toml
[notify]
command = ["/path/to/discord-notify.sh", "{summary}"]
```

`curl --form-string "content={summary}"` also avoids hand-written JSON (unlike
`-F`, it never reads a file for a value starting with `@` or `<`); that form
has not been verified against Discord here.

## Landing it

With `merge = "pr"`, magi opens the pull request and then keeps going: watches
the checks, reads the review comments — human and bot — runs a fix round when
either is unhappy, pushes, and asks you to merge when they are happy.

```toml
[graph]
land = true            # default: take over the watching
land_approval = true   # default: never merge without being asked
land_rounds = 4
```

**Both are on by default, and that pair is the point.** `land` takes over the
watching an operator would otherwise do by hand; `land_approval` keeps the
irreversible step a human decision. An unattended merge needs both flipped, and
that has to be chosen deliberately twice.

It **never force-merges**: out of rounds leaves the pull request open with a
comment saying what is still failing, and `checks: unknown` — no signal at all —
never merges either.

**A branch the base moved under is rebased, not fixed.** A competition that
runs for two hours against a repository merging pull requests all day will
conflict on the way in, and that is arithmetic rather than a defect. magi
rebases the winner onto the base *as the remote has it* and force-pushes with
a lease. A rebase that does not apply is a decision rather than a chore: it
stops and hands over what git said.

A pull request opened a second ago has no checks yet, and "not yet" is
indistinguishable from "this repository has no CI". So magi waits three minutes
for the checks to appear before believing there are none. Whatever it then
decides, a run that got as far as opening a pull request is never re-competed:
the task is **held** with the reason, because the implementation exists and is
waiting on CI or on you. Retrying would race a second branch against your open
pull request and spend the whole competition budget again.

## Project conventions in the prompts

```toml
[prompts]
all = "This repo uses jj, not git. Never run `git commit`."
review = "Ignore formatting; a hook owns it."
```

Appended to the node prompts, under a heading of their own — **never merged
into them**. The built-in prompts carry the invariants the competition rests
on: a judging prompt names no authors, structured answers arrive as one fenced
`json` block, judges are told not to speculate about authorship. A config that
could *replace* a prompt would let a typo un-blind the panel, and the symptom
would be "the judges got worse" rather than an error.

Repository-wide context belongs in `AGENTS.md`, which every agent already reads
from the checkout. These fields are for what a *magi node* needs to know and a
repository file cannot say.

## The phone UI

```sh
magi web                      # http://100.x.y.z:7878
```

The same runs, the same queue, from a phone. `--bind auto` (the default) finds
the machine's Tailscale address and serves there; with no Tailscale it falls
back to loopback and says so.

**There is no authentication. The tailnet is the security boundary.** That is a
deliberate choice for a single-operator tool on a private network, and it is the
reason the default bind is not `0.0.0.0`.

It is still one binary. The interface is three files compiled in with
`include_str!` — no JavaScript toolchain, no CDN, no remote font, nothing
fetched at runtime. `cargo install magi-cli` gives you the phone UI too.

You can watch runs, read the full report, browse the queue, hold and release
tasks, talk new work through in Chat, and delete a task or a finished run that
is no longer wanted. Deletion is guarded rather than hidden: a task the
daemon is holding a claim on, and a run a live daemon is working on right now,
are refused with the reason. So is any run whose candidate worktrees have not
been folded — that is the guard that keeps "delete" meaning "remove a record"
rather than "throw away a worktree". A run left unfinished by a killed daemon
is a leftover, not work in progress, and can be removed once it is folded.

**A stalled run can be folded and resumed from the phone.** Those are the two
halves of "this run is in the way", and they are opposites, so the run's page
offers both and says so:

- **Resume** carries the run on from where it stopped, re-asking only the
  seats whose absence collapsed the panel. The candidates are already built
  and paid for; the alternative an operator actually had was releasing the
  task, which competes three fresh implementations against work that exists.
  It answers **202** and runs in the background — a resume takes minutes, and
  a phone that locks mid-request must not lose it.
- **Fold** removes the run's worktrees and branches, which is what the delete
  button is waiting for, and is where the disk goes: three finished runs here
  were holding 53 GB. Folding a run makes it unresumable, which the
  confirmation says in as many words.

Both are refused while a live daemon is working on that run. A resume is also
refused while the loop is running *at all*, not merely on that run: magi runs
one competition at a time on purpose, and a tap that quietly started a second
graph would double the quota burn for no extra throughput.

Before this the delete button greyed itself out and said "Run `magi fold`
first" — a phone being told to open a terminal, in the one product whose point
is that it does not need one.

**Update & restart, from the phone.** One tap arms it, the second replaces
this binary with the newest release and brings the deck back on it:

1. a run in flight **parks** at its next node boundary and stays resumable, so
   this costs at most the step it is on rather than the competition;
2. `kaishin` puts the new binary in place — it *renames* the running image
   aside, so the swap itself needs no downtime;
3. the server drops its listener, spawns a detached successor, and exits; the
   successor waits out the address and binds it;
4. the loop resumes the parked run rather than competing again.

Measured end to end on a 0.1.0 deck upgrading itself to the 0.2.0 release:
**eleven seconds** from the request to the successor answering on the same
address, with no terminal involved. Refused when the loop belongs to another
process, because replacing this binary would leave that one running an old
binary against the same queue.

**Stop, replace the binary, resume.** A plain stop never abandons a run: it
finishes the competition it is on, which can be an hour. That is right when
you only want the queue to drain, and useless when you have fixes to install —
and killing the process instead loses whatever the seats in flight had not
written.

So a stop that is waiting out a run offers **Park at the next step**. The run
stops at its next node boundary, which costs at most the node in progress
because every node writes its state before the next one starts and every node
skips what is already recorded. The run comes back as parked-and-resumable,
the process exits, the binary can be replaced, and the next loop **resumes
that run** rather than competing again — or you tap Resume yourself.

A park spends no attempt. It is the operator asking for the process back, not
a run that failed, and a few upgrades must not exhaust a budget meant for
agents that actually misbehaved.

**The loop runs inside `magi web`.** `GET /api/loop` reports whether it is
running and who owns it; `POST /api/loop {"running": true|false}` starts and
stops it. Only the process serving the page can control its own loop — a loop
started elsewhere is reported with the owning pid and both calls are refused,
because a button that silently did nothing would be worse than a refusal, and
two loops on one queue race for the same claims and bill the agent quota twice.

Answering an interview turn returns **202** immediately and runs the agent in
the background: a turn takes twenty to ninety seconds, and a phone that walks
behind a wall while the request is open loses the answer the server had already
produced. The operator's turn is persisted before the response returns, so the
transcript is never missing what you actually said.

Every list is ordered by creation, never by last activity. A list that reorders
itself while you are reading it moves the row out from under your thumb.

## Configuration

Config files are TOML rendered by
[teravars](https://github.com/yukimemi/teravars), and they **deep-merge in
increasing precedence**:

```text
<config_dir>/magi/config.toml   <   <repo>/.magi/config.toml   <   <repo>/magi.toml
```

That split exists because the roster is a *machine* fact — which CLIs and which
plans you pay for — while the gate is a *repository* fact (`cargo make check`
here, `pnpm test` there). Declare the roster once per machine and let each repo
state only what is its own. `--config <path>` uses that single file instead.
With no config at all, magi builds a roster from the agent CLIs on `PATH`.

Inside a config file you have `[vars]`, `{{ env.NAME }}`, `{{ system.os }}`,
`{{ repo }}`, `{{ repo_name }}`, and `include = [...]`:

```toml
[vars]
cache = "{{ env.MAGI_CACHE | default(value='/tmp') }}"

[verify]
gate = ["CARGO_TARGET_DIR={{ vars.cache }}/magi-target cargo make check"]
```

One trap, not two: teravars renders the raw text before TOML unescaping, so use
single quotes inside the braces (`value='/tmp'`, never `value=\"/tmp\"`).
Comments used to be the other one — since teravars 0.2.2 they are stripped
before Tera renders, so a comment may quote `{{ ... }}` or `{% ... %}` freely.

**Tables merge; arrays do not.** teravars appends arrays when it merges layers,
which is wrong for every array magi has: `implementers` is an ordered list of
seats, `verify.gate` is the commands to run, `notify.command` is an argv.
Concatenating two of those produces something nobody wrote — three
implementers from a machine's two and a repository's one, or an argv of
`["ntfy", "publish", "curl", "-X"]`.

So magi refuses it. Declare any given array in **exactly one layer**: either
the machine states the roster and repositories override only scalars, or a
repository states its own. Naming the same array in two layers is an error that
names both files, rather than a roster you did not ask for and are paying for.
`magi doctor` prints what actually resolved.

It also names the states nothing will move on its own: tasks **held** for a
person, and tasks left **running** by a daemon that is no longer alive. The
second is the one worth having a line for — the loop only ever offers itself
runnable tasks, and `running` is not one, so a competition interrupted by a
killed daemon leaves its task sitting there forever while the summary counts
it as work in progress.

The full surface:

```toml
[[agents]]
id = "opus"
kind = "claude"        # claude | opencode | antigravity | codex | command
model = "opus"

[[agents]]
id = "sonnet"
kind = "claude"
model = "sonnet"

[[agents]]
id = "opencode"
kind = "opencode"

# Leave a role empty to rotate through the roster. The judge seats are rotated
# by one, so judge i is never the author of candidate i.
[roles]
implementers = ["opus", "sonnet", "opencode"]
judges = ["sonnet", "opencode", "opus"]
reviewers = ["opus", "opencode"]
# fixer defaults to the winner's own author, continuing its own conversation.
# chatter is who Chat asks; unset picks a claude seat, else the first
# runnable agent in roster order. Name one explicitly if that agent is also a
# judge seat — Chat is opened far more often than any single competition and
# would otherwise compete with that judge for the same account.
# language_judge = "jev"  # kind = "command" wrapper deciding if PR text is English; unset keeps the built-in heuristics
# "jev" is an example name for such a wrapper, not a built-in agent. When unset,
# or when the call fails, times out or returns an unparsable reply, the PR text
# guard falls back to the built-in heuristics.

[graph]
implementers = 3                # default 1 (solo); 2+ runs a blind competition
judges = 3
deliberate_rounds = 1
reviewers = 3
review_rounds = 6
incomplete_review = "block"  # block | warn — a round a seat never answered in
max_parallel = 4
language = "en"          # prose language for the agents; "ja" etc.
github_language = "en"   # language of PR / issue titles, bodies and GitHub comments; anything but English drops the English-only check (commits stay English)
sessions = true
timeout_implement = 3600
# A re-ask that only has to restate an answer the seat already worked out gets
# a quarter of these budgets, not the whole one again.
worktree_root = '{{ home() }}/wt/magi'   # optional; see path notes under [repos]

[blind]
commit_msg_hook = true
on_leak = "warn"         # warn | redact | fail
# seed = 42              # reproduce a run's label assignment

[verify]
# Run once per review round in the winner's worktree; failures are fed back to
# the fixer. This is the "real machine" leg of the review.
e2e = ["cargo test --locked"]
# Final gate. Every command must exit 0 before a merge is attempted.
gate = ["cargo make check"]
# Mechanical fixers (formatters) run on the winner just before the gate; changes
# become one commit. A failing command only warns. Uses timeout_verify.
pre_gate = ["cargo fmt --all"]

[worktree]
# Prepared in order right after magi creates a worktree, before any agent or
# verify command touches it (see "Worktree setup" below).
setup = [
  { copy = ".env.example -> .env" },
  { copy = ".env.local", optional = true },
  { run = "pnpm install --frozen-lockfile", timeout_secs = 900 },
]

[merge]
mode = "none"            # none | local | pr

[update]
mode = "notify"          # off | notify | install
# interval = "24h"

[repos]
# Where `magi repos` (and `GET /api/repos`) look for other local checkouts.
# `roots` is an array, so it follows the same "declare it in exactly one
# layer" rule as `[[agents]]` - and since which checkouts exist on disk is a
# machine fact rather than a repository one, that layer is almost always the
# machine config.
# The root is the directory above <host>, e.g. src above github.com.
# These roots and worktree_root do not expand ~; use {{ home() }} instead.
# Single quotes keep home()'s raw Windows backslashes from being TOML escapes.
roots = ['{{ home() }}/src']   # scanned as <root>/<host>/<owner>/<repo>
scan_ttl = 86400                # seconds a scan is trusted before re-scanning
fetch_interval = 600            # seconds between `git fetch origin` in every checkout (0 = off)
```

`mode = "none"` is the default on purpose: magi prints the merge command and
stops. It does not touch your base branch unless you ask it to.

An arbitrary agent, or a deterministic stub for testing:

```toml
[[agents]]
id = "mock"
kind = "command"
command = ["sh", "./mock-agent.sh"]   # {prompt_file} {cwd} {label} {session}
```

`command` agents also receive `MAGI_SEAT`, `MAGI_TURN`, `MAGI_PROMPT_FILE` and
`MAGI_ALLOW_WRITE` in the environment.

### External repository coordination

Coordination is disabled unless `[coordination]` is configured in the machine
config or a repository config layer. All three keys are required after normal
layer merging and template rendering; unknown keys are rejected. For example,
replace these placeholder values with the external updater's protocol:

```toml
[coordination]
contract_file = "example-updater.contract"
contract_token = "example-protocol-v1"
lock_file = "example-updater.lock"
```

`contract_file` and `lock_file` are plain file names inside the Git common
directory: empty or whitespace-only names, `.`, any `..`, separators (`/` or
`\`), NUL, absolute paths and drive prefixes are rejected. `contract_token`
must be nonempty, single-line, and have no surrounding whitespace. The trimmed
contract file contents must equal that token. A missing contract leaves the
repository unregistered; an unreadable or mismatched contract fails closed.
Without this section no contract is inspected and no coordination lock is taken.

The launcher shares one exclusive OS handle among its readers, serializes
editors fairly, and holds reader locks until their processes terminate.
External contention uses `[graph] timeout_resource_wait` and remains an
infrastructure resource wait. See [seat write scope](docs/seat-write-scope.md).
Operators relying on the former built-in file names must set all three keys in
their machine config to keep coordinating with their external updater.

### Worktree setup

magi creates its worktrees with plain `git worktree add` and does not read
`renri.toml`, so a copied `.env` or a one-off bootstrap that `renri add` would
have produced is missing there. `[worktree] setup` is an ordered list of steps
run in each new worktree:

- `{ copy = "src -> dst" }` (or `{ copy = "path" }`) copies from the **primary
  checkout's working tree** (an absolute source is allowed; this is not the
  `origin/main` commit the rest of the config is read from). It never
  overwrites a file that already exists in the worktree, and a missing source is
  an error unless the step has `optional = true`. `dst` must stay inside the
  worktree (no absolute path, no `..`, nothing under `.git`).
- `{ run = "..." }` goes through the configured shell with the worktree as cwd,
  `timeout_secs` (default `timeout_verify`) and stdin closed. `MAGI_RUN` and
  `MAGI_NODE` are removed from its environment so nothing seat-shaped leaks.

A failing step (copy error, non-zero exit, timeout) fails that worktree with the
step number, the command and the tail of its output; the run is `Blocked`, never
carried on. The key is an array, so it is declared in exactly one layer; the
default is empty and changes nothing.

Covered worktrees: implementer candidates (hence the winner/verify worktree,
which is the candidate itself and is not set up twice), judge and advisor
seats, reviewer seats (set up again every round, because the reset wipes them),
the review-takeover checkout and the operator-fix checkout. **Not covered:**
the throwaway worktrees used for rebases and local releases; a release command
prepares its own checkout.

Things to know:

- Setup must not edit tracked files; one that does fails the worktree. For the
  same reason it is the wrong place for things like `kata apply`: every
  candidate would carry the same diff. That is explicitly out of scope.
- What setup leaves untracked is hidden from `git add` through a per-worktree
  `core.excludesFile`, and magi's rescue commits skip it too, so a copied `.env`
  never lands in a candidate branch.
- The cost is paid once per seat (and per review round), so prefer light
  commands, and write `run` steps to be safe to repeat.

## The review loop

The winner — and only the winner — enters a bounded loop:

1. Each reviewer gets its **own detached worktree** pinned at the exact commit
   under review, so no reviewer can perturb the tree and the fixer never races
   one.
2. `verify.e2e` runs in the winner's worktree. Its output is fed to the fixer.
3. The fixer addresses the blocking findings, or **rejects one with an
   argument** — a rejected finding with a checkable reason is a correct outcome,
   and magi records it as such.
4. Repeat until no blocking finding remains and verification is green, or
   `review_rounds` is exhausted (`blocked`). A round where the fixer produces no
   commit stops the loop instead of spinning on an unchanged tree.

Finding ids (`R2-1-3`) are assigned by magi, never by the agent, because the
fixer's adoption report is keyed by them — that is what makes reviewer precision
measurable rather than self-reported.

**A seat that never answered is not a seat that found nothing.** Every round
records how many reviewers it expected against how many actually came back, so
a timeout, a crash or an unparsable reply cannot pass for a clean pass. `magi
show` labels such a round `incomplete` and names the seats that went missing,
`magi stats` carries a per-seat timeout rate, and by default
(`graph.incomplete_review = "block"`) the round is never treated as clean: with
nothing raised to fix, it is re-reviewed, and if the round budget runs out
while the panel is still short the run ends `blocked` rather than `ready`. Set
`incomplete_review = "warn"` to let the round gate on whatever did answer — the
gap stays in the report either way.

`magi run` and `magi review` **exit non-zero when the run ends `blocked` or
`stalled`**, after printing the report — a script or CI step that only reads
the exit code must not take "nothing panicked" for "the change is reviewed".
The one exception is a run that left a pull request open: that is a hand-off
to a human, not a failure, and exits 0.

## Statistics

`magi stats` aggregates every run on disk:

- **implementation** — win rate per agent, and how often an agent produced
  nothing at all.
- **review** — findings per round, precision (adopted / submitted), unique
  find rate (findings no other reviewer in the same round raised, matched by
  normalised title or same file within five lines), and the share of its
  seated rounds a seat never answered in at all.
- **verification** — how often E2E failed while every static review was clean:
  the runtime defects only execution found.

These are a by-product, not a benchmark. Seats rotate, the task distribution is
whatever you happened to ask for, and an agent that drew harder tasks looks
worse. Read them as *relative performance on your workload*.

## Where a run spends its time

Measured on a real run (2 candidates, 2 judges, 1 reviewer, 219 s wall):

| node | wall | spent by |
|---|---|---|
| prep | 2.8 s | git: four worktrees plus the hook |
| implement ×2, parallel | 93.0 s | the slower agent (69 s / 92 s) |
| judge ×2, parallel | 84.6 s | agent latency |
| vote | 9.2 s | agent latency |
| tally / fold / merge | 0.5 s | magi |
| review | 26.1 s | agent latency |
| e2e + gate | 1.6 s | your commands |

magi's own compute was 3.3 s of 219 s. Everything else is the agent CLIs, so the
three levers that matter are:

- **`max_parallel`** — agent processes are network-bound, so raising it is close
  to free. Implement, judge and review run as parallel waves.
- **`deliberate_rounds`** — deliberation is *sequential* by design, because a
  turn has to be able to answer the one before it. Each round costs
  `judges × turn latency`. Set it to `0` to skip arguing and go straight to the
  private vote.
- **`verify.e2e` on a compiled language** — each worktree would otherwise build
  from scratch, which dwarfs every agent call. `verify` commands run through
  `sh -c`, so share the cache:

  ```toml
  e2e = ["CARGO_TARGET_DIR=${TMPDIR:-/tmp}/magi-target cargo test --locked"]
  ```

  Only the winner's worktree runs them, one at a time, so nothing contends on
  cargo's lock.


### Disk

`candidates + judges + reviewers` worktrees exist at peak (8 with the defaults).
Judge worktrees are removed as soon as the tally lands, and `magi fold` clears
the rest. On a large repository that is real disk; lower `judges` or point
`worktree_root` at a roomier volume.

State lives in `<data_local>/magi/runs/<id>/` — `run.json` plus every prompt and
raw agent reply under `artifacts/`. `MAGI_HOME` moves it.

## License

MIT

### MAGI message appearance

Automatic task updates, watched-job reports, consult handovers, task posts, and
system notices use the **MAGI** sender name, a gold bubble in both themes, and
a dedicated built-in avatar. Operator sends and conversation persona replies
keep their own names and icons. Older operator records retain their original
attribution; message text alone is not evidence that MAGI wrote them.

In the web **Settings** page, use **MAGI sender icon** to choose an image from
your phone's image picker. Review the preview and tap **Save MAGI icon**, or
**Reset to built-in** to restore the supplied artwork. The choice applies to
all conversations, independently of operator and persona icons, and survives
reloads and server restarts. It is stored atomically as `magi-icon.data` in the
magi home directory; it does not rewrite repository or machine configuration.
The picker resizes images to at most 384 pixels per side. PNG, JPEG, GIF, and
WebP inputs are accepted; uploaded icons are limited to 256 KiB (including the
data URI). Invalid or unavailable saved images fall back to the built-in icon.

### Choosing the next action at the merge gate

The approval card offers these actions (localized for `graph.language = "ja"`):

| Action | Result |
| --- | --- |
| Fix findings and re-review / 指摘を修正して再レビュー | One bounded repair and fresh review on the **same PR, branch, run and task**. Previous findings and gate evidence remain in repair history. A new decision is required; this action never approves merging. |
| Reimplement with selected agent / 選択した agent で再実装 | Select a configured implementer explicitly. The old execution retires and auto-merge is disabled; closure of the old PR must be confirmed before the **same task** starts a new attempt and PR. The original instruction, attachments and review context survive. |
| Hold decision and stop work / 判断を保留して作業を停止 | Leaves the PR open, blocks the run and manually holds the queue task. Stops repair, rebasing and merge work, including when the PR head changed. It is not a request for repair. |
| Withdraw: close PR and end task / 取り下げ | A second confirmation explicitly closes the PR and terminates the task. History remains. Failed or unconfirmed closure holds the task instead of declaring it done. |
| End obsolete approval (no merge; PR unchanged) / 古い承認を終了 | A second confirmation retires a stopped execution and its pending approval, preserving history even when the original queue task is missing. It never contacts the forge, closes a PR, or starts replacement work. Later attempts and completed tasks are unaffected. A live or unknown driver, an existing auto-merge arm, or a conflicting decision refuses dismissal; stop the driver and disable auto-merge first. Use Withdraw when you explicitly intend to close an active PR. |
| Merge this pull request / マージを承認 | A second confirmation approves only the displayed head, including the outstanding findings shown in the review panel. A changed head needs a fresh decision. |

CLI answers use stable tokens, independently of translated button labels:

```sh
magi answer <question-id> --reply 'fix findings and re-review'
magi answer <question-id> --reply 'reimplement:<configured-agent-id>'
magi answer <question-id> --reply hold
magi answer <question-id> --reply 'withdraw: close PR and end task'
magi answer <question-id> --reply 'dismiss: end obsolete approval without changing PR'
magi answer <question-id> --reply merge
```

`merge` and `hold` remain compatible with older cards and CLI clients. Open
legacy cards are extended in place, keeping their question ID and displayed
head. Answered cards retain their choices and decision history.

Dismissal is applied locally while saving the answer; no daemon or original
queue task is required. It is permanent for that execution, unlike Hold.
For the other actions, the daemon resumes the existing parked owner to apply the answer. Answering
alone does not dispatch an agent. Without a running daemon, explicitly resume
the existing owning task/run; do not file a second repair or replacement task.
After an applied hold, explicitly run `magi task release <task-id>` with the
daemon running, or `magi run --resume <run-id>`. This keeps the same run and PR,
retains the old answered card, and parks at a fresh decision card before any
repair, rebase or merge. Resuming does not approve a merge.

The replacement implementer selection is persisted and enforced in both initial
dispatch and implementation fallback. It applies to implementation seats;
reviewers, advisors and repair/fixer roles retain their configured assignments.
A missing or quota-limited selected implementer leaves the task manually held,
without substituting another implementer. Restore availability and explicitly
release the held task to retry. Ambiguous previous-driver ownership or a crash
between minting a replacement and linking it to the queue also holds for manual
inspection rather than starting a concurrent attempt. The run records link the
old question, attempt, PR and replacement attempt/PR.

Question free-text replies require a live recipient or one the running daemon
can automatically recover. The card distinguishes listening, waiting for startup
or restart/capacity, and unavailable recipients. It disables only text submission
when the deputy has exhausted its start budget, is disabled/unavailable, or the
asking seat cannot resume. The running daemon publishes its effective recovery
capacity; editing settings requires restarting it before new capacity becomes
available. Conversational text commands check again before saving; a stale card
cannot submit into a dead thread. Rejected drafts
remain in the input, and earlier saved messages remain in the question history,
marked undelivered when no recipient can read them. Offered decisions (including
free-text answers) and the explicit originating-chat consultation route keep
their existing authorization;
delivery availability never authorizes merging or closing a PR.

Standing chat and deputy consultation leave the card unanswered while discussing
it. For a new card, chat approval requires the owner's exact latest message
`merge <displayed-full-head>` and that same complete quote. Stop, repair,
withdrawal and replacement require the exact offered token as the latest whole
message. Silence, discussion, repair authorization and stale approvals never
permit a merge.
