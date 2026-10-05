<!-- kata:agents:base:begin -->
## Shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). Codex and Claude Code
(>=2.1.277) read it directly; the `GEMINI.md` file is a thin shim
for tools that don't yet auto-load `AGENTS.md`.
**Edit AGENTS.md, not the shim.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
  Issues (titles, bodies, comments) and PR review comments too: GitHub
  is a worldwide surface, so everything written there is English in every
  repo, whatever language the task or the conversation was in.
- **Releases are PR-driven and tagging is automatic** — in repos that
  ship a release pipeline. Bump the version in the project's own
  manifest in a `chore/release-vX.Y.Z` PR; on merge to `main` the
  language layer's `auto-tag.yml` detects the bump, pushes the
  `vX.Y.Z` tag, and that tag is what fires `release.yml`. **Do not run
  `git tag` by hand** — the bot tag will collide and the manual push
  fails. The specifics belong to the layers shipping those two
  workflows, which are not the same layer: `kata:agents:rust:*` for
  which file holds the version and for `auto-tag.yml`,
  `kata:agents:rust-{cli,lib}:*` for what `release.yml` builds and
  publishes. A repo with no `auto-tag.yml` has no release pipeline at
  all: nothing tags, and the version field in its manifest may well
  be decoration.

### Pre-merge review

Review happens **before the pull request, on the operator's machine**,
via [magi](https://github.com/yukimemi/magi). This layer no longer
ships PR-side review bots: `claude-review.yml` and `claude.yml` were
removed from it. Their scope was
human-authored PRs — their own job-level `if:` already excluded
`chore/release-*`, `kata-apply/auto`, `apm-bump/auto` and
Renovate / Dependabot — which is exactly the set magi reviews, so
keeping them meant reviewing the same diff twice, a
`CLAUDE_CODE_OAUTH_TOKEN` secret per repository, Actions minutes on
private repos, and one trap that silently cost reviews: a PR editing
either workflow was skipped by `claude-code-action`'s
workflow-validation check and merged with a green check and no
review attached.

**"Removed" is a statement about this template layer, not about
every repo's current state.** Dropping a `[[file]]` entry stops kata
from managing the rendered file — it does not delete it. A repo that
had these workflows before this change keeps `claude-review.yml` /
`claude.yml` (and the `CLAUDE_CODE_OAUTH_TOKEN` secret) under
`.github/workflows/` until someone deletes them by hand, and until
then they still fire on every human-authored PR. Check
`.github/workflows/` before treating a PR as unreviewed-except-magi:
if either file is still there, its comments are a real review, not
noise to ignore.

- **`magi review <branch> --merge none`** runs only the review +
  verification + gate half of magi's graph: nothing competes, no
  implementation, no judging, no vote. That is the mode for
  hand-written work. `magi run "<task>"` is the full competition, for
  work handed over whole. Both end at the same gate.
- **Always pass `--merge none` on a `magi review` you invoke by
  hand.** `[merge] mode` in config defaults to `"pr"` for the queue
  loop (`magi serve` / `magi web`) that drains `magi task add` —
  there, ending in an actual PR is the point. `magi review` run
  directly inherits that same config default when no `--merge` flag
  is given, which would open a PR nobody asked for. `--merge none`
  overrides it for that one invocation and leaves the branch for the
  operator to turn into a PR themselves.
- What the loop actually does: each reviewer gets its **own detached
  worktree pinned at the commit under review** (no reviewer can
  perturb the tree, and the fixer never races one); `verify.e2e` runs
  in the branch's worktree and its output is fed to the fixer;
  finding ids (`R2-1-3`) are assigned by magi, not by the agent, so
  the fixer's adoption report can be matched against them; the loop
  is bounded by `review_rounds`; `verify.gate` must exit 0 before any
  merge is attempted.
- **`magi.toml` is repo-owned, not kata-managed.** Point
  `verify.gate` at the exact command CI runs, so a local pass means a
  green PR, and point `verify.e2e` at the invocation that actually
  covers the repo — feature flags included. A gate that differs from
  CI turns a clean magi run into a red PR, which is the one failure
  this arrangement cannot absorb.
- **If you did not run magi, the change was not reviewed, and nothing
  will tell you.** Do not open a PR for a hand-written change before
  `magi review` comes back clean; if you must, say so in the PR body
  and say why. What does *not* count as a substitute: a green CI run
  (it compiles and tests, it does not review), and CodeRabbit's
  silence.
- **CodeRabbit stays installed and is not part of the gate.** It does
  not auto-review repositories under 10 stars — the common case here —
  so treat it as absent unless it posts. When it does post, its
  findings are a real review: address them, reply **in the inline
  thread** with an `@coderabbitai` mention (the review-comment
  *replies* endpoint,
  `gh api repos/<owner>/<repo>/pulls/<N>/comments/<id>/replies -f body=…`),
  and reply even when declining — say why, because a silent skip
  reads as overlooked. A "review limit reached" quota notice carries
  no findings and counts as quiet; re-trigger with
  `@coderabbitai review` when the quota refills if you want a real
  pass.
- **Read the report, not the exit status.** A reviewer seat that
  times out is logged as `WARN agent timed out seat=review-2` and
  then summarised as "raised 0 finding(s)" — indistinguishable from a
  genuinely clean pass in both the summary and `magi stats`. Check
  for timeouts before believing a clean round: a round where half the
  panel never answered is not a clean round.
- **Review artifacts stay local.** magi comments on a pull request
  only when it *stops* landing one. Findings, the fixer's adoption
  report and reviewer precision live in the run directory
  (`magi show`, `magi stats`). When the PR needs a record — a
  non-obvious fix, a finding declined with an argument — paste that
  part into the PR body or a comment yourself.
- With `merge = "pr"`, magi opens the pull request and keeps going:
  watches the checks, reads the review comments (human and bot), runs
  a bounded fix round when either is unhappy, pushes, and asks before
  merging. `land_approval` is on by default and **silence is a
  hold** — nothing merges unanswered. `magi answer` (or the web UI)
  is where it asks. Out of rounds leaves the PR open with a comment
  saying what still fails; `checks: unknown` never merges.
- **Merge gate**: magi's gate green — or CI green for a change magi
  never touched — **and** every review that did post resolved (a
  leftover `claude-review.yml`, CodeRabbit, a human) **and** the
  owner's explicit approval. The irreversible step stays a human
  decision.
- **No review-monitoring poll loop for bots this layer no longer
  ships.** The old loop existed to wait on them. Where a repo still
  has `claude-review.yml` (see above) the old cadence still applies
  until it is deleted; otherwise, after opening a PR wait for CI and
  report the wait state to the owner. When magi is landing the PR
  (`land = true`), magi does the watching.
- Bot-authored PRs (Renovate / Dependabot) need no review pass at
  all: CI green + owner approval.
- **Version-bump-only PRs** — a single `chore/release-vX.Y.Z` branch
  whose entire diff is `[workspace.package].version` /
  `[package].version` plus the matching inter-crate refs and the
  lockfile — likewise. There is nothing in a version bump for a
  reviewer to find, and the release pipeline downstream of merge
  (auto-tag → `release.yml`) is time-sensitive.

### Worktree workflow

> **Before your FIRST edit to any file, run `renri add` — NEVER edit the
> main checkout.** Read-only inspection (Read / Grep / Glob) stays on the
> main checkout; the instant you intend to *change* a file, you must
> already be in a worktree. The trap that keeps catching agents: diving
> into a fix the moment the diagnosis lands and editing in place. A
> concurrent agent shares the main checkout — your in-place edits will
> clobber theirs or be clobbered, and in a jj-colocated repo a stray
> working-copy commit entangles unrelated WIP into your branch. If you
> slip and edit in the main checkout, capture the diff first (jj already
> snapshotted it into the working-copy commit, so `jj diff > patch`; for
> git, `git stash` or save a patch — if you got as far as committing on a
> branch, just push it). Then reset the main checkout to pristine main
> (`jj new main@origin`, or `git switch -`), `renri add` a worktree, and
> re-apply the captured diff there.

Use [`renri`](https://github.com/yukimemi/renri) for any
commit-bound change. From the main checkout:

```sh
renri add <branch-name> --from main@origin            # create a worktree (jj-first), off latest upstream main
renri --vcs git add <branch-name> --from origin/main  # force a git worktree, off latest upstream main
renri remove <branch-name> -y --non-interactive  # cleanup after merge (agent-safe; see note)
renri prune                        # GC stale worktrees
```

Read-only inspection can stay on the main checkout.

**Always pass `--from <upstream main>`** (`main@origin` for jj,
`origin/main` for git). Without it, `renri add` forks off the *cwd
worktree's current HEAD* — in a long-lived main checkout that often
lags upstream, so the PR later shows up CONFLICTING against a `main`
that had already moved (e.g. a refactor merged upstream before the
branch was cut), forcing a manual re-port of the whole change.
`renri add` does fetch first, but fetching only updates `main@origin`
— it never moves the checkout's HEAD, so an explicit `--from` is what
guarantees a fresh base.

**Agents / non-interactive shells:** `renri remove` prints a details
panel and waits for a confirmation prompt — without `-y` it **hangs**,
and `--non-interactive` *alone* errors asking for `-y`. Always pass
`-y`, and add `--non-interactive` so a mistyped/omitted name fails
instead of opening a fuzzy picker (the same picker-fallback applies to
`remove` / `cd` / `exec` with no name). Use `-f`/`--force` to remove a
worktree that still has uncommitted changes or conflicts. To sweep
every merged-PR worktree in one shot: `renri remove --merged -y`.

### kata-managed sections

Several files in this repo are managed by `kata apply` from the
[`yukimemi/pj-presets`](https://github.com/yukimemi/pj-presets)
templates — the bytes between `<!-- kata:*:begin -->` and
`<!-- kata:*:end -->` markers, plus the overwrite-always files
listed in `.kata/applied.toml`. **Editing those bytes locally
won't survive the next `kata apply`** — push the change to the
upstream template repo (`yukimemi/pj-base` / `yukimemi/pj-rust` /
…) instead.

The marker scopes are layered, one per applied layer:
`kata:agents:base:*` is this section, and each layer adds its own
(`kata:agents:rust:*`, `kata:agents:rust-cli:*`,
`kata:agents:pnpm:*`, `kata:agents:firebase:*`, …). Which ones apply
*here* is a grep away: `<!-- kata:` in this file.

### This project's own conventions

Everything a layer ships is generic by construction: it describes the
stack the template assumed, not what this repo grew into. **Bytes
outside every marker pair are yours and survive `kata apply`** — so
project-specific conventions belong in a section of their own, outside
the markers (conventionally at the end of the file; if a later layer
appends its block below yours, no matter — kata only ever rewrites
between its own markers). Same mechanism as the `.gitignore` /
`.gitattributes` blocks.

Write those conventions down there rather than leaving them in one
agent's head, in commit archaeology, or in a README the agent will not
read. What earns a line:

- **Any layer default that does not hold here.** A layer states its
  assumption flatly ("Hosting is the primary target", "these rules are
  a placeholder to replace"). When the project has diverged, say so and
  say why — the layer's text keeps asserting the opposite on every
  apply, and an agent that only reads the blocks will act on it.
- **Facts duplicated across files with no compiler in between** — an
  address or a path that appears in code *and* in a rules/config file
  that cannot import it, a timeout that has to stay inside another
  timeout. List every copy, so the next edit finds them all.
- **kata-shipped files this project deleted on purpose**, together with
  the `once_applied = true` line in `.kata/applied.toml` that keeps
  them deleted. Otherwise someone helpfully restores one.
- **Shapes the runtime forces but no tool checks** — an export form a
  platform requires, import specifiers that must (or must not) carry a
  file extension, a directory whose contents are reachable by URL.
- **Invariants that money or access rest on**, naming the file and line
  that actually enforces them.
- **Which language the code speaks versus what a user reads**, when the
  two differ.

A repo whose `AGENTS.md` is nothing but kata blocks is a repo where
every agent re-derives all of that from scratch — and gets the layer
defaults wrong the same way each time.
<!-- kata:agents:base:end -->
<!-- kata:agents:rust:begin -->
### Rust workflow

This repo follows the shared Rust toolchain conventions. The
language-agnostic conventions block above (`kata:agents:base:*`)
covers git workflow, PR review cycle, and worktree usage.

### Build / lint / test

```sh
cargo make check                    # editorconfig-check + fmt --check + clippy + test + lock-check (the pre-push gate)
cargo make setup                    # one-time hook install + apm install
cargo build                         # debug build
cargo build --release               # release build
cargo test                          # tests; add -- --nocapture for stdout
```

`cargo make check` is what `.github/workflows/ci.yml` runs and what
the local pre-push hook calls — anything that passes locally
should pass on CI and vice versa. Don't paper over a failing
clippy by sprinkling `#[allow(clippy::...)]`; fix the underlying
issue or push back on the lint with reasoning.

### Toolchain pin

The Rust toolchain is pinned via `rust-toolchain.toml` and the
project compiles with the `stable` channel. Don't introduce
nightly-only features without a real reason; if you do, document
the reason in the relevant module.

### Lint / format policy

`rustfmt.toml` and `clippy.toml` are kata-managed (sourced from
`yukimemi/pj-rust`). Edits to those files in this repo won't
survive the next `kata apply`; if a setting is wrong, push the
fix to `yukimemi/pj-rust` so every Rust project using these templates picks
it up.

### CI workflow

`.github/workflows/ci.yml` is also kata-managed. The source lives
in `yukimemi/pj-rust/.github/workflows/ci.yml.template` (the
`.template` suffix keeps GitHub Actions from running the source
itself in pj-rust); each Rust project receives the rendered
`ci.yml` via `kata apply`. Action versions are bumped centrally
by Renovate at `yukimemi/pj-rust` and propagate down on the next
apply, so don't bump them locally — Renovate is configured
(via the kata-distributed `renovate.json`) to ignore
`.github/workflows/ci.yml` and `.github/workflows/release.yml`
in each PJ to avoid the bump→clobber loop.

### Releasing: version bump PR + auto-tag

Releases are triggered from `main` by a Cargo.toml version
change. `.github/workflows/auto-tag.yml` is kata-managed (source:
`yukimemi/pj-rust/.github/workflows/auto-tag.yml.tera`). It
watches `main` and, whenever a commit lands that changes the
top-level `version = "..."` in `Cargo.toml`, it pushes a matching
`vX.Y.Z` tag — no manual `git tag` step is needed. The tag push
then fires `release.yml`; see `kata:agents:rust-lib:*` or
`kata:agents:rust-cli:*` for what release.yml does in each
crate shape.

Cut a release via a small PR — never `git push` the bump
straight to `main`, even though the base block lists version
bumps as an exception to "no direct push". `auto-tag.yml` only
fires on `main`-branch pushes, so the bump must land via a merge
either way; using a PR also gives CI a chance to gate the
release. Enable automerge so CI green = release start:

```sh
git switch -c chore/release-vX.Y.Z
# Edit `package.version` in Cargo.toml, then:
cargo build                     # let Cargo.lock follow
git commit -am "chore: release vX.Y.Z"
git push -u origin chore/release-vX.Y.Z
gh pr create --fill
gh pr merge --auto --squash --delete-branch
```

Once CI is green the PR auto-merges. `auto-tag.yml` then pushes
`vX.Y.Z`, which fires `release.yml`.

**In a workspace, the version is in more than one place.** A member
that is published and depended on by another member is declared
with both a `path` and a `version` — crates.io needs a
requirement it can resolve for somebody who is not building from
the checkout, so a bare `path` will not do:

```toml
my-core = { path = "crates/my-core", version = "0.4.2" }
```

That literal does not follow `[workspace.package] version`.
Nothing in Cargo makes it, and the release above will not either.

**It fails late and quietly.** `version = "0.4.2"` means `^0.4.2`,
so a stale pin keeps resolving through every *patch* release and
stops only at the first bump that crosses the minor — where
`cargo build` refuses with `candidate versions found which didn't
match`, in the middle of cutting the release. Two repos on these
templates hit exactly this, one of them three releases after its
pins were last correct, and the other had already written the
hazard down in prose and drifted anyway.

So bump the pins in the same commit, keep them in
`[workspace.dependencies]` rather than in each member, and assert
it rather than remembering it. A test is the cheapest place —
`cargo test` already runs in CI, and it needs no toolchain a Rust
workspace does not have. [pj-rust-workspace's
README](https://github.com/yukimemi/pj-rust-workspace#the-internal-version-pin-and-the-check-for-it)
carries one to copy into any member's
`tests/check_versions.rs`: `internal_pins_match_the_workspace_version`
fails when a pin and the workspace version disagree, and
`members_inherit_the_workspace_version` fails when a member writes
its own version or reaches for a sibling by path.

**Repo settings to set once:** enable
`delete_branch_on_merge=true` (Settings → General →
"Automatically delete head branches"). The `--delete-branch`
flag on `gh pr merge --auto` is effectively a no-op — gh
returns as soon as automerge is enabled, so the deletion has to
happen server-side, which requires the repo setting.

**Why `KATA_APPLY_TOKEN`:** GitHub refuses to fire downstream
workflows from tags pushed by the default `GITHUB_TOKEN`, so
`auto-tag.yml` pushes with `KATA_APPLY_TOKEN` (the same PAT
`kata-apply.yml` already uses). Each consumer repo needs a
`KATA_APPLY_TOKEN` secret set; if a version-bump merge silently
doesn't fire `release.yml`, the missing PAT is the first thing
to check.
<!-- kata:agents:rust:end -->
<!-- kata:agents:rust-cli:begin -->
### Rust CLI release flow

This is a Rust CLI crate, so the release pipeline is publish-aware.
`yukimemi/pj-rust-cli` ships a tag-driven release workflow in
`.github/workflows/release.yml` (rendered from
`release.yml.template` for the same don't-auto-execute reason
ci.yml uses).

Releases are triggered by a Cargo.toml version bump landing on
`main`. The bump flow itself (PR with automerge → `auto-tag.yml`
pushes `vX.Y.Z` → `release.yml` runs) is documented in
`kata:agents:rust:*` under "Releasing: version bump PR +
auto-tag" — that block also covers the `KATA_APPLY_TOKEN` and
`delete_branch_on_merge` setup. What `release.yml` then does for
a **CLI** crate:

1. Cross-compiles binaries for **three** targets — full triples
   `x86_64-unknown-linux-musl`, `x86_64-pc-windows-msvc`,
   `aarch64-apple-darwin`. Linux is musl (statically linked, so the
   binary runs on any glibc vintage); the Linux job installs
   `musl-tools` first. Intel Mac (`x86_64-apple-darwin`) is
   deliberately **not** built — Apple Silicon only.
2. Uploads them as a GitHub Release with auto-generated notes.
3. `cargo publish --locked` to crates.io using the
   `CARGO_REGISTRY_TOKEN` repo secret.

Set the `CARGO_REGISTRY_TOKEN` secret once per repo (`gh secret
set CARGO_REGISTRY_TOKEN`) before the first release. If the
crate is internal-only and shouldn't go to crates.io, either drop
the `publish` job locally (release.yml is `when = "once"` so the
edit survives subsequent applies) or set `package.publish = false`
in `Cargo.toml`.

The binary name is derived from the GitHub repo name at runtime
(`${{ github.event.repository.name }}`), so the workflow is
identical across CLIs using these templates unless your `[[bin]] name` in
`Cargo.toml` deliberately differs from the repo name — in that
case override `BIN_NAME` in the workflow's `env:` block.

### Release smoke target (`examples/smoke.rs`)

After `cargo build --release`, `release.yml` runs
`cargo run --release --target <T> --example smoke` on every build
matrix entry. `cargo test` runs only library code, so the produced
binary's startup path goes unverified — that's how shoka v0.10.0
shipped a rustls `CryptoProvider` panic to crates.io even though
all 13 CI checks were green.

The template's default `examples/smoke.rs` body is intentionally
no-op so kata can drop it into every consumer crate without
breaking releases. **Override it per crate** with the smallest
operation that exercises the regression-prone surface:

- HTTPS-using CLIs: build the API client (octocrab, reqwest, etc.)
  and issue a tiny no-auth GET — that forces the rustls handshake
  to run inside the same binary the release publishes.
- File-handling CLIs: write+read a temp file via the real I/O
  helpers (catches missing crate features, permission regressions).
- Pure library crates: leave as no-op.

A failing smoke blocks the release before publishing to GitHub
Releases / crates.io.
<!-- kata:agents:rust-cli:end -->

## magi's own conventions

Outside every `kata:` marker, so this survives `kata apply`.

### The package is `magi-cli`, everything else is `magi`

`magi` on crates.io is an 882-byte placeholder (`description = "Placeholder"`,
no repository), and crates.io does not reclaim names. So, exactly as
`yukimemi/yui` publishes `yui-cli`:

| thing | name | set in |
|---|---|---|
| crates.io package | `magi-cli` | `[package] name` |
| library | `magi` | `[lib] name` — keeps `use magi::…` working |
| binary | `magi` | `[[bin]] name` |
| GitHub repo | `magi` | — |

`release.yml` derives `BIN_NAME` from the *repo* name, so it already matches
`[[bin]] name` and needs no override.

**`CARGO_PKG_NAME` is therefore not usable as a repo or binary name.**
`src/updater.rs` spells all four out as constants and passes
`.crate_name("magi-cli")` so kaishin's `cargo install` fallback reaches the right
package. Deriving them would make `magi self-update` look for a
`yukimemi/magi-cli` repository that does not exist — which is a live bug in
`yui`, whose updater omits `.crate_name` and would `cargo install yui`, an
unrelated crate by another author.

### Agent CLIs are the only backend

magi drives subscription CLIs (`claude -p`, `opencode run`, `agy -p`,
`codex exec`) and never an HTTP API. That is a product decision, not an
unfinished one: the CLIs carry
the operator's own plan and expose the agent's whole tool loop. **Do not add an
API-key path**, and do not add a `reqwest`-shaped dependency — `examples/smoke.rs`
deliberately exercises filesystem and serialization instead of a TLS handshake
because there is no handshake to exercise.

### Gemini CLI is deliberately unsupported

`AgentKind` has no `Gemini` variant. Google retired the standalone client for
individual accounts ("This client is no longer supported for Gemini Code Assist
for individuals") in favour of Antigravity, which is the `agy` binary and the
`antigravity` kind. Anyone re-adding a Gemini adapter is adding dead code; the
escape hatch for any other CLI is `kind = "command"`.

### Session mechanics are per-CLI and verified by hand

Multi-turn nodes (deliberation, the review loop) depend on each seat keeping one
CLI conversation. The mechanics were established empirically, not from
docs, and each is asserted in `src/agent.rs` tests:

| CLI | open | resume | trap |
|---|---|---|---|
| `claude` | `--session-id <uuid>` | `--resume <uuid>` | the uuid must be RFC 4122 v4 or the CLI rejects it (`src/rng.rs` mints it) |
| `opencode` | `--format json` → `sessionID` | `-s <id>` | nothing to resume until a turn reported an id |
| `agy` | `--output-format json` → `conversation_id` | `--conversation <id>` | print mode defaults to a **5 minute** timeout; `--print-timeout` must track the node budget. `--disable-slash-commands` silently disables `--mode`, so magi never passes it |
| `codex` | `exec --json` → `thread.started.thread_id` | `exec … resume <id>` | `resume` is a **subcommand** and rejects every `exec` option that follows it (`unexpected argument '--sandbox'`), so magi emits all options first and the subcommand last. The prompt goes on stdin, which codex reads only when the prompt argument is `-`. The answer is the **last** `item.completed` whose item is an `agent_message`: earlier ones narrate the tool loop |
| `omp` | `-p --mode=json` → the `id` on its `"type":"session"` line | `-p … --resume <id>` | a turn that ends on a **tool call** emits **no** `agent_end` line, so the answer has to be the last non-empty assistant text block across `agent_end` / `turn_end` / `message_end` — reading only `agent_end` silently discards a complete review. The prompt goes on stdin. `--continue` opens a *new* session, so it is never used |

`agent::has_session` is the single place that decides whether a follow-up prompt
may rely on memory. If it says no, the node re-sends full context. Never assume
a resume worked.

### A reviewer seat remembers who failed it across review rounds

`RunState::seat_history` (`run::SeatHistory`, `#[serde(default)]`, so
`run::SCHEMA` is not bumped) records per `review-N` seat the roster ids that
failed it (`failed`), the agent that last answered (`last_ok`) and the class of
the last failure (`last_fail`, `run::FailClass`). Only the review loop carries
it (`WaveCtx::carry_seats`; every other node passes `false`). Sessions stay
keyed by seat, and an agent change always takes a fresh `SeatState`.

- **Start of a round.** `pick_start_spec`: `last_ok` if still on the roster and
  not failed, else the spec's own agent unless failed, else the next roster
  agent that has not failed, else the spec's agent (the whole roster failed).
  Ids no longer on the roster are ignored.
- **Two sets, on purpose.** `failed` is only a *priority*. The bound is the
  per-round `tried` set in `ask_json_wave`, which starts empty every round, so
  an agent that answered is never locked out and a seat can always hand over.
  Persisting `tried` itself would shut a seat once everyone had been tried.
- **Handover inside a round** is `next_for_seat`: forward from the seat's
  start, never wrapping, over ids neither tried this round nor failed earlier.
  Only when that is exhausted does it *rescue* a carried failure, scanning the
  whole roster in order - the one place the no-wrap rule is relaxed. Each id is
  rescued at most once per round and rounds are bounded by `review_rounds`, so
  it cannot loop. A failure is saved before the next agent is asked, so a
  restart does not forget it. A repeated `Other` failure class still stops the
  chain (`last_fail` seeds `prev`); a dropped stream that a nudge recovers is
  never recorded as a failure.
- **A review is still fresh each round**: the full prompt for the new patch is
  sent. Trade-off accepted: an agent with a transient failure is not used again
  in this run's reviews while another agent answers, so a persistent failure
  (quota, auth, prompt-shaped error) is not re-billed every round.

### `[roles]` synthesizer / chatter / conductor take a fallback chain

Each accepts a string (unchanged) or an ordered array of ids
(`config::AgentChoice`, untagged). `agent::pick_chain` resolves it: unset or
empty is `agent::pick`'s default order; a duplicate id keeps its first
appearance; an unknown or uninstalled id is skipped with a `tracing::warn`;
nothing resolving is an error naming the role. `agent::chain_advances` is the
one place that decides to move on (error, quota - judged apart from `usable`
- or an unusable answer).

- **Each id is tried at most once per call.** That is the whole retry bound:
  a plain `for`, never a loop back. An exhausted chain ends as the last
  single failed seat would (`Ok(None)` for the synthesizer, the first
  agent's failure note for chat, the same error for the conductor).
- **A fallback agent gets a fresh `SeatState`**, never the previous agent's
  seat renamed, so `agent::has_session` is false and full context is
  re-sent. The conductor falls back only through the call and the parse of
  its reply, never through `apply`, which would file queue changes twice.
- **A chat's fallback persists.** `talk.agent` switches to the agent that
  answered (with a magi note in the transcript); quota coming back does not
  move it home, an operator's agent switch does. Only a talk whose agent is
  in `[roles] chatter`'s chain falls back; an explicit `--agent` is a chain
  of one. A turn can now cost up to N x the turn timeout.
- `[roles]` roster roles (implementers, judges, reviewers, advisors) are
  untouched.

### opencode has no read-only mode

`--auto` gates **every** permission in opencode, reads included. A judge or
reviewer seat spawned without it cannot even open its own prompt file and drops
out with *"the user rejected permission to use this specific tool call"* — which
is exactly what happened on the first live run, silently costing a seat on the
panel. So magi always passes `--auto` for `kind = "opencode"`, and read-only-ness
for those seats rests on the prompt plus the fact that judge worktrees are
deleted after the tally and reviewer worktrees are `reset --hard` to the commit
under review every round. Do not "fix" this by withholding `--auto`.

### codex is the only seat whose read-only mode is enforced

`--sandbox read-only` is refused by the CLI, not by the prompt, so a codex
judge or reviewer cannot write even if it decides to. Implementers get
`workspace-write`. **Nothing ever gets
`--dangerously-bypass-approvals-and-sandbox`** — it would throw away the one
enforced guarantee in the roster, and `codex_is_sandboxed_reads_stdin_and_puts_resume_last`
fails if it appears.

Unattended seats also pass `-c approval_policy="never"`: a seat that stops to
ask blocks until its node timeout kills it, and there is nobody at the
terminal during a run.

### omp is opencode's shape, and its answer is not where the stream looks

`kind = "omp"` reaches DeepSeek (its models ship in `omp`'s own catalog) and
`gpt`-class models through the same CLI, but it has **no read-only mode**: like
opencode, `--auto-approve` gates every permission, reads included, and a
non-interactive seat without it cannot open its prompt file at all. So magi
always passes it, and read-only-ness for those seats rests on the prompt plus
the worktree discipline named above — never on the flag. `--dangerously-bypass-
approvals-and-sandbox` appears nowhere, exactly as for codex.

The extraction is the part that will be got wrong by anyone reading the event
names instead of the stream. `omp -p --mode=json` emits `agent_end` **only** for
a run that quiesces on a message turn; a turn that ends on a tool call
(`stopReason: "toolUse"`) — which is what the review seats here routinely do —
ends with no `agent_end` line at all, and the answer is in `message_end` /
`turn_end` instead. A first hand-written wrapper keyed on `agent_end` and
silently discarded three complete reviews; `omp_takes_the_answer_without_an_agent_end_line`
exists so that cannot come back. The answer is the **last** non-empty assistant
text block, because earlier ones narrate the tool loop (sometimes as a bare
`.`). `--continue` opens a *new* session rather than the stored one, so resume is
always `--resume <captured id>`.

### teravars renders the whole config file

`Config::load_layers` runs every layer through teravars, so:

- **Comments are stripped before Tera renders** (teravars >= 0.2.2), so a
  comment may quote `{{ ... }}` or `{% ... %}` freely — it is gone before the
  template parser sees it. Before 0.2.2 the opposite held: `# see {{ system.* }}`
  was a render error, not a comment, and `Config::starter_toml` warned about it.
- **Rendering happens before TOML unescaping.** `value=\"/tmp\"` inside a TOML
  string reaches Tera as `value=\"/tmp\"`, backslashes included, and fails. Use
  single quotes: `value='/tmp'`.
- **teravars ships `system.*` and `vars` only.** magi adds `env`, `repo` and
  `repo_name` to the context in `load_layers`; there is no `env` upstream.
- **`[vars]` stays in the merged table**, so `load_layers` removes it before
  deserializing — `Config` has `deny_unknown_fields`.
- **`env` is keyed by the OS's exact spelling.** Windows reports `Path`, POSIX
  reports `PATH`, and `std::env::vars()` passes that through verbatim into a
  case-sensitive map, so a config or test written against `{{ env.PATH }}`
  passes on one runner and fails on another —
  `env_is_available_to_templates_with_a_default` learned this from a red
  `test (windows-latest)`. Name only variables you set yourself, and always pair
  them with `| default(...)`.

### `kata status` will keep reporting AGENTS.md drift. Do not "fix" it.

`pj-rust/AGENTS.md.rust` is stored with **CRLF on all 130 of its lines** in the
template repository (`pj-base/AGENTS.md.base` is LF). kata copies those bytes
faithfully, so a local `kata apply` rewrites the `kata:agents:rust:*` block with
CRLF and leaves a 260-line whitespace-only diff plus a permanent
`update AGENTS.md` in `kata status`.

The fleet stays LF anyway because `kata-apply.yml` commits on ubuntu **through
git**, where `.gitattributes` (`* text=auto eol=lf`) normalises on commit. The
trap is local: **jj does not implement git's eol normalisation**, so a
`kata apply` followed by a jj commit lands CRLF that CI would have stripped.

So: this file stays LF. If `kata status` nags about `AGENTS.md`, that is the
upstream CRLF, not drift worth committing — normalise back to LF and
`jj restore .kata/applied.toml`. Tracked by yukimemi/pj-rust.

### Facts duplicated with no compiler in between

- **Prompt phrasing is load-bearing for the tests.** `tests/common/mod.rs`
  dispatches its mock agent by grepping the generated prompt for
  `Final vote`, `deliberation round`, `independent judges`,
  `reviewers of a patch`, `Your patch was reviewed`, `Your revote`. Rewording a
  prompt heading in `src/prompt.rs` breaks the end-to-end tests — which is the
  intended alarm, but update both sides together.
- **`blind.strip_lines` is consumed twice**: as case-insensitive substrings by
  `blind::strip_attribution`, and as generated `sed` addresses by
  `blind::commit_msg_hook`. Entries must stay plain literals, not regexes.
- **`run::SCHEMA`** must be bumped whenever a `RunState` field changes meaning,
  or a resumed run will half-read someone else's state file.
- **The settings screen writes the machine layer only** (`settings::save`, path
  from `Config::machine_layer`, never from the request) and validates by
  re-loading the layered config with the proposal standing in for that file.
  It has no `toml_edit`: `settings::patch_role` is a line patch of `[roles]`.

### Invariants the blindness rests on

- Branch names carry the **label**, never the agent id (`RunState::branch_for`).
  A judge inspecting `magi/<run>/B` with git must not be able to infer authorship.
- Sessions are keyed by **seat**, never agent id (`SeatState::key`). The same
  model may implement and judge in one run; its judge seat must never have seen
  the implementation.
- Rescue commits use a neutral identity (`git::commit_all`). A real `user.name`
  would name the operator; an agent-configured one would name the vendor.
- `blind::redact` must never rescan its own replacement. It once did, and
  `[REDACTED]` contains the letters of `codex` and `cursor` — an infinite loop.

### Rate limits, and what a collapsed panel means

- **Quota is detected, not guessed.** `agent::claude_quota` keys on the *only*
  observed shape (a JSON object with `is_error: true` and a `result` mentioning
  `session limit`) and treats everything else — other CLIs, unknown output — as
  an ordinary failure. `AgentOutcome::Quota` is distinct from `Failed`, and a
  quota'd seat is **not** re-asked, because a retry now fails the same way.
- **A verdict needs a quorum.** `tally()` counts a judge as present unless a
  `QuotaLoss` names that seat; a strict majority (`judges/2 + 1`) is required,
  or the run becomes `Stalled`. `Stalled` is a terminal-but-suspect status: it
  must never look like `Ready` in the report or the TUI (which is why
  `Filter::Attention` and `status_style` carry it), and the graph's
  fold/review/gate/merge stop at the tally so the run stays resumable.
- **A stalled run must stay stalled across `--resume` until its quota comes
  back.** `execute()` short-circuits at the top when the loaded status is
  already `Stalled` — otherwise `deliberate()`/`vote()` clobber it back to
  `Voting` and the run resumes into a tidy `Ready`. But the early return first
  gives the run one chance to repair itself: `recover_stall()` re-asks exactly
  the seats recorded in `RunState::quota` for the judge/vote nodes (never a
  healthy seat), and if the re-tally restores the quorum the run picks up and
  finishes; for a seat that ranks again its `QuotaLoss` is dropped so `tally`
  counts it present. If the quota is still out, the seat fails again, the run
  stays `Stalled` and the marker is persisted (the normal end-of-execute save
  sits below the `Stalled` return), so it stays resumable for a later retry.

### The TUI: pure state, one render function, one terminal function

`src/tui.rs` keeps `App` as pure state with pure transitions, `draw` as the only
function that knows ratatui, and `run` as the only one that touches the real
terminal. That is what makes selection clamping, filter cycling and modal-help
behaviour unit-testable, and it is why the frame tests can use
`ratatui::backend::TestBackend` instead of a PTY.

Three things that are load-bearing:

- **`disable_raw_mode` runs LAST in `TerminalGuard::drop`.** On Windows the
  console-mode restore performed while leaving the alternate screen replays a
  snapshot taken *after* raw mode was enabled, so disabling raw mode first lets
  that restore put the cooked bits back to their raw values and strands the
  whole console after magi exits. Learned in yukimemi/shoka; do not "tidy" the
  order.
- **Quit is checked before anything modal.** A help overlay that swallows
  `Ctrl-C` is how a TUI earns a reputation for trapping people;
  `help_is_modal_but_never_swallows_a_quit` pins it, and it was a real bug in
  the first draft.
- **The report pane renders `report::run`'s own ANSI** through `ansi-to-tui`
  rather than reimplementing the report against ratatui spans. One
  implementation of the report, one place to change it. Colour therefore has to
  be *on* for the TUI, which is why `main` only disables it for non-terminals
  and explicit `--no-color`.

The deck is read-only. Adding a key that mutates a run means adding a
confirmation flow and an undo story; `magi fold` already exists for cleanup.

### A question always has someone waiting on it

`magi ask` runs inside the agent's own shell tool, which kills a long wait, and
an agent can background the call and exit. The question then outlives the
only process that would read the owner's reply. `src/waiter.rs` is the
guarantee that something is always on the other end; it runs inside `magi
serve` as its own task (not a step of the queue loop, which is busy for the
whole of a run).

- **The lease is a sidecar, not a field.** `<questions>/<id>.lease` holds
  `{kind, pid, beat_at}`; the asker rewrites it every poll, the daemon while a
  delivery runs. It is not `*.json`, so `list` never sees it, and it is not in
  the question so the beat cannot race the phone's answer with a lost update.
  Fresh means a beat within `ask::LEASE_TTL` (90 s), deliberately longer than
  the gap between two `magi ask --wait` calls. **No pid checks** - pids are
  reused and mean different things per platform. The record's own `waiter`
  field is a note of who was last known to wait; the lease says whether they
  still do (`QuestionView::holder`).
- **Every change to a question that can still be answered goes through
  `Questions::update`** (a read-modify-write under a short lock file), never
  `get` + `put`: the answer, the say, the asker's bookkeeping and the
  waiter's would otherwise overwrite each other.
- **Never start a fresh consultant.** The waiter resumes the *same seat's*
  session (`agent::has_session`, seat state from the run's `RunState`, or
  `<home>/conduct/seat.json` for the conductor). If the session cannot be
  resumed - no session, cwd gone, run over, agent left the roster - nothing
  runs; a notice says why and the record is kept. Never assume a resume worked:
  a failed turn leaves the word undelivered, and it is tried again after
  `RETRY_AFTER`.
- **Never a second agent while the first is blocked.** A fresh lease means
  do nothing, and a seat still listed in `RunState::active` (or the
  conductor's `busy` marker) is not resumed either: the CLI may outlive the
  `magi ask` that was killed.
- **An answer that carries an action is the daemon's, not the waiter's.**
  `waiter::decide_owned` returns `Idle` for it whenever the task exists
  (no task: delivered as before), because resuming the dead seat while the
  daemon requeues or resumes the same run would run it twice.
  `daemon::apply_choice_actions` applies it once - idempotence is
  `Task::actions_applied` under `queue.claim`, never `answer_delivered` - and
  also when a live agent consumed the answer and then died. It defers while the
  asker's lease is fresh or its seat is still active; that check is not atomic
  with the write, but the asker only prints, so the bound is one poll
  (`ask::LEASE_TTL` at most) and never a double application.
  `daemon::action_standing` tells "the daemon will not act" apart: `Applied`
  and `Stale` (a question about an earlier attempt, judged before `Busy`) stop
  the waiter's delivery for good; only `Busy` (running / blocked / done) still
  defers to it. The waiter never delivers an action answer without the task's
  claim: if `queue.claim` fails it steps aside and the next tick re-judges.
- **Delivery is tracked in the record** (`delivered_turns`, `answer_delivered`),
  set by the asker as it prints and by the waiter after a resumed turn. Where
  the outcome is unknown the word stays undelivered: a repeat is better than a
  drop. Only questions with `cwd` set (filed by `magi ask`) are the waiter's;
  land's approval gate and release notices are not.
- **A park drops a delivery in flight and writes nothing**; the lease just ages
  out and a restarted waiter reads the same state from disk. `answer_timeout`
  is obeyed: an unanswered question with no fresh lease is abandoned in the
  asker's own words, but a word the owner gave in time is delivered first.
- `waiter` writes no `RunState`: it clones the seat, so a resumed turn's
  `turns` count is not persisted (only ever needs to be non-zero).

### The queue: data with pure transitions, I/O in one place

`src/queue.rs` splits `Task` (data plus *pure* state changes) from `Queue`
(every filesystem call, constructed with its root). That is not decoration:

- `Queue::at(root)` is why the queue tests run in parallel against temp
  directories with no process-global state. The first version used
  `run::set_home`, whose `OnceLock` means **the first call in the process
  wins** — three tests silently shared one home and clobbered each other. If
  you find yourself reaching for `set_home` in a unit test, parameterise the
  root instead.
- `Task::fail` decides whether an attempt was the last one, with no disk in
  the way, so the retry policy is asserted directly.

**A quota stall is refunded, and only a quota stall.** `Task::stall`
decrements `attempts`. A rate limit is a property of the machine, not of the
task, and a quota window closing overnight must not leave a backlog of `held`
tasks that were never actually judged. Do not "simplify" this into `fail`.

But the refund is keyed on `Verdict::quota_hit`, not on the `Stalled` status,
and that distinction is load-bearing. A quorum can collapse for a reason that
has nothing to do with quota: run e633 stalled with `quota: []` because two
judges answered with the wrong JSON shape, twice each, nudge included. That is
ordinary flakiness, it can recur every time, and refunding it removes the
bound from the retry loop — each attempt paying for another hour-long
implement wave before it reaches the same judges. `max_attempts` exists so
that cannot happen. Refund the machine's failures; charge the task for its
own.

### Two gates guard the merge, and both are on

`graph.land` and `graph.land_approval` both default to `true`, and neither is
redundant:

- `land` decides whether magi keeps watching the pull request after opening it.
  It only engages for `merge = "pr"`.
- `land_approval` decides whether a human sees the panel before the merge.
  Silence is a hold, and nothing but the word `merge` merges.

An unattended merge requires flipping both, which is two deliberate choices.
Do not "simplify" them into one flag: the useful middle state - magi does the
watching and the fixing, a human owns the irreversible step - is exactly the
default, and one flag cannot express it.

**A third, conditional reason to hold: a contested hand-off.** Both gates
stay on by default, and `land_approval = false` still means an unattended
merge for an ordinary run. The exception is a run whose review loop handed
off over a finding the panel did not settle: the last round ended with a
Major-or-above finding open *and* a reviewer's final vote (the revote where
reconsideration answered, the initial vote otherwise) was reject. Findings
raised in the budget's last round are never fixed or re-reviewed, so that is
the one case an unattended merge would land over an explicit objection. Then
`land` asks, with `approval_gate` unchanged (silence is a hold, only `merge`
merges, a pending question parks the run), and the question says why it was
filed and lists the findings and the rejecters. The PR is still opened; only
the merge waits.

- The decision is one function, `ReviewRound::contested_handoff`, and its
  result is a snapshot stored on `RunState::contested_handoff` when
  `stop_reviewing` hands off (a reentry keeps the first record). `land` only
  reads it, through `land::contested_to_ask`.
- `graph.hold_contested_merge` (default on) is evaluated at land time, so
  turning it off restores the unattended merge at once. With `land_approval`
  on it changes nothing.
- A question already filed before the record existed is reused as it is.

**Every land decision is about the head magi observed, never an older one.**
After a push (a fix round or a rebase) `land` remembers the pushed SHA and
decides nothing until the pull request points at it *and* the rollup's commit
(`commits`, last entry) is that same head; `bound_head` returns `None`
otherwise and the loop re-polls, bounded by `WAIT_CEILING`, then stops naming
both SHAs - never a merge. The merge itself is `gh pr merge
--match-head-commit <observed sha>`, so a push between the look and the merge
is refused by the forge, and a refusal after which the head differs goes back
to observing without failing the run. The non-required-red policy is
unchanged. Known limit: the awaited SHA is memory only, so a crash right after
a push can bind the old head once on resume; `--match-head-commit` still stops
that merge. `gh` truncates `commits` near 100 entries, so a very long pull
request waits out the ceiling and stops (the safe direction).

**A conflict is a rebase, not a fix round.** `Step::Rebase` is decided before
the checks, because every check on a branch that cannot land is an answer
about a state that cannot land. It is bounded by the same budget as a fix and
spends none of it, since the change is not what is wrong. The rebase happens
in a throwaway worktree - this repository is jj-colocated, so a rebase in the
primary tree would move a detached `HEAD` under the operator - and pushes with
`--force-with-lease`, so a person's push to the same branch fails the step
instead of being lost. A rebase that conflicts is handed to the fixer seat
(`rebase::rebase_with_fixer`, shared by `graph::Runner::sync_to_base` and
land): the conflict is left standing in the throwaway worktree and the agent
resolves it and runs `git rebase --continue`. magi never resolves a conflict
itself. The budget is `graph.review_rounds`, counted in
`RunState::rebase_fixes` and saved *before* each call (so a park or crash
cannot hand a round back), shared by both callers, and touching neither the
task's attempts nor land's own rebase budget. A round is judged by what git
says - rebase no longer in progress, nothing unmerged, no markers left in a
path that conflicted, the base an ancestor of the result - never by the
fixer's report; whether the tree builds is left to the review and gate that
follow. When the rounds are spent, the fixer hit its quota, or it could not
finish, the branch ref is restored, the worktree removed and the old
behaviour applies: `base_sync.conflict` (or land's `stop`) carries what was
tried - rounds spent and the paths still conflicted - for the conductor to
quote, and a person decides.

**A pull request is a hand-off, not a failure.** `settle` takes `left_pr`, and
a `Blocked` run that opened one holds its task instead of requeueing it. Run
01c2 spent two and a half hours on a competition, opened a green pull request,
and had the whole thing re-competed four seconds later because `land` read
`checks: unknown` on a pull request GitHub had not yet attached workflow runs
to. Two rules came out of that, and neither may be dropped:

- **Unreadable is not absent.** `land::CHECKS_GRACE` waits three minutes before
  believing a pull request has no CI. Refusing to merge without a signal is
  right; concluding there will never be one four seconds in is not.
- **Work that exists is never re-competed.** The artifact is on a branch
  waiting for CI or a person. A retry races a second branch against the open
  pull request and bills the whole roster again.

**A re-ask is not the original job.** `graph::retry_budget` gives a nudge a
quarter of the node's timeout, because a seat that already did the thinking
only has to restate it. Two judges restated a ranking in 41 and 133 seconds
while a third sat on a resumed session for eleven minutes, on the full 1200s
budget it had inherited - one stuck nudge nearly doubled a judging round whose
other seats had long finished. A retry that re-sends the whole prompt, because
the seat kept no context, keeps the whole budget: that one really is the job
again.

### Known follow-ups from the takeover review (PR #322)

- **R3-1-1 (race).** `Runner::execute_graph` loads `released_to`, then later
  saves the whole in-memory `RunState`; a takeover that writes the release mark
  in between is overwritten by the resume's `driver_pid` save, so the takeover
  can delete a clean worktree under a run that just started driving. Intended
  fix: compare-and-set on `released_to`, or a re-read under a lock.
- **R3-1-3.** `Released::restore` runs an unconditional `git branch -f` unless
  the current tip equals the saved tip, so a branch a third party moved is
  overwritten. `Released` cannot tell a legitimate post-sync tip from a foreign
  one; intended fix: refuse (or lease-check) when the branch moved to a third
  value.
- A refused handover keeps `Task::review_branch` (`Task::hold_for_handover`),
  and `Task::release` keeps it only for a machine hold; releasing without
  cleaning the old worktree re-holds the task without spending an attempt.

### Handing the address over: release, then spawn

`POST /api/upgrade` ends the process it is serving from, and the order of the
last two steps is the whole thing:

1. `upgrade_and_restart` replaces the binary and signals `HANDOVER`.
2. `serve`'s `select!` wakes on it, parks and drains the loop, **drops the
   listener by returning**, and only then calls `spawn_successor`.

The first attempt did it the other way round — spawn, sleep 200 ms, `exit(0)`
— and the successor died on "address already in use" with its stdio sent to
null, so the deck never came back and there was no terminal to say why. Note
that the successor is whatever binary was just installed, so it cannot be
assumed to carry `bind_waiting`: correctness has to come from the ordering,
and the retry is only belt to that braces.

Two more rules here:

- **Refuse when the loop is foreign.** Replacing this binary would leave
  somebody else's `magi serve` running an old one against the same claims.
- **Answer 202 before restarting.** The reply has to leave while this process
  can still send one; the phone learns the deck is back by reconnecting.

### A run is interrupted at a node boundary, never mid-node

`graph::Pause` is checked between nodes in `execute`, and that is the only
place it may be checked. The cheapness of a park is entirely a property of the
boundary: every node saves the run's state before the next one starts, and
every node skips what is already on disk — `prep` returns early once
candidates exist, `implement` asks only the seats with nothing recorded,
`judge` returns early once judgements exist. A park therefore resumes into
exactly the node it stopped before and throws nothing away. A check *inside* a
node would abandon the wave in flight, which for an implement wave is an hour
of paid calls.

Two rules follow, and neither is optional:

- **A parked run is not a failed one.** `Verdict::parked` refunds the attempt.
  It is the operator asking for the process back — to replace the binary,
  usually — and a few upgrades must not exhaust a budget meant for agents that
  misbehaved.
- **An unfinished run is resumed, never re-competed.** `attempt` looks for a
  non-terminal run of the task and calls `Runner::resume` on it. When it did
  not, run 01c2 was blocked and the loop immediately started 3cbf on the same
  task, paying three implementers to redo two and a half hours of work that
  was already on a branch.

### Integration tests that mint runs must take `home_lock`

`run::set_home` is a process-global `OnceLock`, so every test in one
integration binary shares a magi home. Two that mint runs concurrently clobber
each other: a park test and a resume test in one file failed with "no
candidate produced a change" because the other test's fixture had taken the
home. `tests/common::home_lock()` serializes them.

This used to be a convention, not something the compiler checked, and that
gap bit twice: `graph_review_only`'s own test always took the lock and still
failed under the full suite, because `graph_split`, `graph_unanimous` and
`graph_uncontested` built fixtures without ever calling `home_lock`. The
tests that forgot stayed green — they never contended for anything — and the
one that paid for it was whichever unrelated test happened to be running
concurrently. `tests/common::fixture` (and every `fixture_with_*` variant)
now takes `&HomeGuard`, the value `home_lock().await` returns, as its first
argument, so a test that never called `home_lock()` fails to compile instead
of occasionally taking someone else's home. Do not add a `fixture*` helper
that can be called without one.

### Autonomy is bounded, and the bound is the point

`src/daemon.rs` runs one competition at a time — no `--jobs`. The graph is
already parallel inside (candidates times judges), and the scarce resource is
the agent CLIs' quota, not local CPU.

- Every attempt is counted, and a task that burns its attempts is `held` for a
  human rather than retried until the money runs out.
- A setup error that never minted a run still spends an attempt. Otherwise a
  task naming a nonexistent repository is retried at every poll, forever.
- A crash mid-run leaves the task `Running` with its run id recorded. That is
  deliberate legibility: the operator can see what was in flight. Do not add a
  startup sweep that "cleans" those into `Queued` without also proving the run
  is dead.
- Ctrl-C does not abandon a run in flight. Killing the graph mid-node leaves
  worktrees, branches and agent sessions behind, and throws away agent calls
  already paid for.

### Agents are users of this CLI

`agent::invoke` exports `MAGI_RUN` and `MAGI_NODE`, and `magi task add` reads
them to attribute a filed task to the seat that filed it. Nothing else may set
those variables, which is what makes "most of the backlog was filed by agents" a
measurement rather than a claim. `Invocation` carries `run`/`node` purely for
this; they must not influence behaviour.

### Panels: the sandbox is the whole argument

Agent-authored HTML is rendered in the operator's browser, which the rest of
this UI refuses to do. Three things make that acceptable, and none of them is
optional:

1. **`<iframe sandbox>` with no tokens.** One function in `app.js` builds it.
   `allow-scripts` would hand a scriptable document to agent HTML;
   `allow-same-origin` would give it the operator's origin. Neither is ever
   added, and a comment above the function says so.
2. **`PANEL_CSP`, asserted as a whole string** by a test, so weakening one
   directive fails it. `img-src 'self' data:` is what lets a panel show its
   own attachments while every external load is refused.
3. **Asset names validated twice** - on write and on read - against
   `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$` with no `..` anywhere. `:` is excluded
   deliberately: on Windows `Path::join` with a drive-absolute name discards
   the prefix and would serve any file on the disk.

**The panel's URL ends in a filename.** A document served at `.../panel`
resolves `shot.png` to `.../shot.png`, which is not the asset route, so panels
written exactly as the prompt instructs showed broken images. `base-uri 'none'`
means a `<base>` tag cannot fix it from inside, which is why the route does.

**Measure before you loosen a policy.** That broken image was diagnosed as
"a tokenless sandbox has an opaque origin, so `img-src 'self'` can never
match", and the fix was going to be widening `img-src` to a dynamic host. The
theory was wrong: `'self'` matches fine in a sandboxed frame, and the real
cause was a corrupt fixture PNG. Chrome's `Log.entryAdded` prints CSP
violations verbatim - read it instead of reasoning about it.

### Asking back: a question is a conversation, not a form

The owner is not limited to answering or letting a question time out. They can
talk back - `POST /api/questions/{id}/say`, `magi answer <id> --say "..."` -
and the agent that asked keeps the conversation going with
`magi ask --thread <id>`, which appends its reply, re-arms
`answer_timeout`, and waits again. No second agent is ever started: the
process blocked in `magi ask` is returned to with exit code 0 and the
owner's words on stdout, because the alternative - a fresh consultant that
never saw the original task - would have to be caught up on everything the
first agent already knows.

- **`QuestionStatus` still has three values and means what it always meant.**
  A round trip never touches `status`; it stays `Open` from the first "why not
  Postgres?" to the eventual `Answer`. `Question::waiting_on_agent()` is the
  one place "the ball is in the agent's court" is readable at all, and it is
  why `count_open()`/`open_for()` do not need to change: ten turns of back and
  forth are still one open question.
- **`ask::SCHEMA` bumped to 2 for `Question::thread`, and the bump does not
  mean what it used to.** Every field added since schema 1 (`panel`, `assets`,
  now `thread`) carries `#[serde(default)]`, so `read_path` refuses a file only
  when its `schema` is *greater* than this build's - a strict equality check
  would turn this bump into an upgrade that stops reading yesterday's
  questions.
- **The quiet window is a judgement call, not a setting.** An agent's reply
  pages the operator only when it has been more than five minutes since their
  own last word (`REPLY_QUIET_WINDOW` in `src/ask.rs`) - still reading the
  card, no second buzz; walked away, they still hear about it. It is a
  constant, not a `magi.toml` key: the operator cannot express "am I still
  looking at the phone" as a per-repository setting, and neither can this
  build.
- **`magi ask --thread` replaces `--choice` wholesale, same as everywhere
  else this codebase replaces rather than merges.** Asking back is normally
  exactly the moment the right choices change; a caller that wants the old set
  kept just repeats it.
- **`src/chat.rs` is a different product and this feature does not touch it.**
  The wire shape (`who`/`body`/`at`, `"operator"`/`"agent"`) matches
  `chat::Turn` on purpose - it lets the phone render both with one component -
  but `ask::Turn` and `ask::Who` are their own types with no dependency on
  `chat`.
- **No single `magi ask` call blocks for the whole `answer_timeout`, on
  purpose.** `answer_timeout` defaults to a day; the shell tool an agent CLI
  runs `magi ask` inside caps out around ten minutes, so a wait that long is
  killed either way, taking the `magi ask` child - the only thing that would
  have read the owner's answer - down with it. Run 20260908-205802-c9eb is
  what that looked like: seat `impl-A` asked, the tool killed the wait, the
  seat backgrounded the blocking call and exited `completed`, and the
  owner's eventual answer on the web UI had nobody left to notice it. So a
  wait is sliced to `WAIT_SLICE` (`src/ask.rs`, four minutes) and hands
  control back with [`Wait::Pending`] - exit 0, same as `Wait::Replied` -
  when nothing has happened yet. `magi ask --wait <id>` (`ask::resume_wait`)
  is how the caller picks that same wait back up: no new turn, no
  re-notification, and its budget is computed from
  [`Question::asked_at`] plus `answer_timeout`, never re-armed to a fresh
  window - stacking `--wait` calls can spend that deadline, never extend it.
  `--thread` is unrelated and keeps re-arming a fresh `answer_timeout` on
  every reply, exactly as before.

### A conductor's question has a deputy, and the conductor still never waits

`src/deputy.rs` runs one short-lived seat per open conductor question
(`node == conduct::NODE`), as its own task inside `magi serve` beside the
waiter. It exists because a free-text reply to a conductor question used to
reach nobody: the conductor is non-blocking, and `Conductor::worth_a_look` does
not move on a say.

- **The seat is `deputy-<question id>`, never the conductor's.** `conduct/seat.json`
  is rewritten by every cycle for the whole queue, and sessions are keyed by seat
  name, never by agent id. `waiter` leaves any question with a `deputy` alone
  (`q.deputy.is_some()`): resuming the conductor's seat there would fork it.
- **`Question::deputy` is persisted and written only through `Questions::update`**
  (the owner's say and answer write the same file). It holds the brief, the agent,
  the `SeatState` and `starts`. A restarted daemon resumes the seat only when
  `agent::has_session` says so; otherwise a fresh seat gets the whole context
  again. `ask::SCHEMA` is 5 for it, `#[serde(default)]` as ever.
- **Bounded twice.** `daemon.max_deputies` (default 2) at once, and
  `deputy::MAX_STARTS` per question, never reset by a restart. The deadline is
  the question's `answer_timeout` from `last_activity`: the waiter retires the
  question, and `daemon::resolve_blockers` then moves a task blocked on an
  abandoned conductor question to a machine hold.
- **An unread say extends the deadline only for a deputy that can read it**: one with a fresh lease, or one that can still start (`deputy::can_start`: `daemon.max_deputies > 0` and a readable config) with starts left. Past the deadline with none of those, the waiter retires the question. `QuestionView::deputies_enabled` tells the UI, which then says nobody listens and only a choice resolves it.
- **A say is not a decision.** The deputy answers it with `magi ask --thread`
  (repeating the choices, which a reply replaces). `magi ask --settle` records an
  answer only for `MAGI_NODE=deputy` with the seat recorded on the question, an
  offered label, and a verbatim quote of something the owner said, and never on
  an `operator_held` task. Applying the outcome stays with the daemon's existing
  answer path; the deputy applies nothing. It runs with `allow_write: true` because `magi ask` writes the question record and a read-only sandbox (codex) refuses that; like opencode/omp seats, its read-only-ness rests on the prompt.
- **A fresh seat takes a short handover turn first**, so its session id is persisted before the hours-long `magi ask --wait` turn (`agent::invoke` learns it only on return). The claim file covers only the start decision (60 s stale), the lease covers the turn, and a spent deputy (`MAX_STARTS`) no longer shields an expired question from the waiter.
- **The UI says who listens.** `web::holder_of` returns `deputy` only for a fresh
  lease on a question with a deputy, and `nobody` for a conductor question that
  has no one; `app.js` says "Waiting for the agent" only for `asker` / `deputy` /
  `daemon`.
- **A merge approval (`land::APPROVAL_NODE`) has a deputy too, and stays land's.**
  `deputy::kind_of` picks the kind; a land deputy gets `land::deputy_brief` (PR,
  run, base/winner, contested findings, "merge is irreversible, silence is a
  hold"; a missing run record is named as missing, never reconstructed). It has
  **no `cwd`**, so the waiter's `decide` stays `Idle` for it - the `q.deputy`
  skip rule only ever matters for conductor questions. Its deadline is
  `deputy::deadline`: `asked_at + answer_timeout`, never moved by a reply, the
  same instant `daemon::land_resume_state` abandons the question; land is the
  only thing that retires an approval. `--settle` on an approval accepts
  `merge` / `hold` only when the owner's whole message is that word
  (`settle_by_deputy`); anything else is answered back with `--thread`. The
  operator-held check resolves the task through the run id. `holder_of` says
  `nobody` for an approval with no live deputy, and `app.js` says so.
- The mock agent in `tests/common/mod.rs` greps `prompt::DEPUTY_HEADING`; reword
  the heading and update both.

### The web UI: one binary, no authentication, and no lying empty states

`src/web.rs` serves `assets/ui/{index.html,app.css,app.js}` through
`include_str!`. **There is no `--assets-dir` and no filesystem fallback**, and
the front end has no build step, no CDN and no remote font, because
`cargo install magi-cli` has to yield a working phone UI with nothing else
fetched.

- The default bind is the Tailscale address, not `0.0.0.0`. There is no
  authentication: the tailnet is the security boundary, and that trade is only
  honest while the default cannot accidentally face the internet.
- `report::set_color(false)` is called once in `serve`, never per request — the
  flag is a process-global `AtomicBool` and a request-time toggle would race a
  concurrent render and leak escape codes into a browser.
- **An unreadable run must be counted, not hidden.** `/api/health` reports
  `runs_unreadable`, and the UI says so. The first live run of the server
  printed "Nothing has run yet" with six runs on disk: they were schema 1
  against a build speaking schema 2, and the list quietly dropped every one.
  The terminal deck was fixed for the same failure the same day. Any new view
  over runs owes the operator this number.
- **A stalled run must never render as a decided one.** A collapsed panel still
  records a winner label, so the verdict marker is gated on `tally.met_quorum`
  and shows "provisional" when it is false.
- **Never tell the phone to open a terminal.** The delete control used to grey
  itself out with "Candidates must be folded before deleting. Run `magi fold`
  first." — in the one product whose whole argument is that an operator does
  not need a terminal. If a guard names a command, the command belongs on the
  screen: `POST /api/runs/{id}/fold` and `POST /api/runs/{id}/resume` exist
  for exactly that reason. The same applies to any future refusal.
- **Fold and resume are opposites and must read as opposites.** A fold throws
  away the worktrees a resume would continue from, so they share one row, the
  resume is offered first, and the fold's confirmation says the run can no
  longer be resumed. A resume that has nothing left to continue from is
  disabled with that sentence rather than left to fail on tap.
- **A refresh must never navigate.** `loadChat` opened by assigning
  `state.chatDetail`, which made every refresh a navigation: with a turn in
  flight, `tickWait`'s ten-second insurance refreshes the *waiting*
  conversation whatever the operator is reading, so the transcript on screen
  was replaced by a different one every ten seconds while the address bar went
  on naming the chosen conversation. Only `applyRoute` and an explicitly
  started interview choose what is displayed. A loader that finds its subject
  off screen settles whatever bookkeeping it owes and then returns.
- **Anything that spends agent calls answers 202 and runs in the background.**
  `say` and `resume` both take minutes; a held connection on a phone is a coin
  flip, and the change stream is how every other moving part reports itself.

### The shared build cache is only shared if both sides name the same path

`magi.toml` renders `CARGO_TARGET_DIR` from `{{ vars.cache }}`, whose default
is `/tmp`. The verification commands run under a POSIX shell, which resolves
`/tmp` to its own temp directory; magi measures the string literally. On
Windows those are two different places, so the janitor pruned and the health
view sized a path that never existed while 17 GB of build output sat in the
shell's `/tmp`. `magi cache show` now says so instead of printing `0 bytes`.

**Set `MAGI_CACHE` to a real absolute path on any machine whose shell rewrites
`/tmp`** (this one: `C:/Users/yukimemi/AppData/Local/Temp`, set as a user
environment variable). Forward slashes, so the same string works in the shell
command and in `std::fs`.

### The disk is a resource the graph can exhaust

A run costs a candidate worktree plus a reviewer worktree per reviewer, each
with its own multi-GB `target/`. Two failures came out of that, both silent:

- **A save that runs out of disk leaves `<home>/runs/<id>/run.json.tmp` and
  nothing else.** `run::list_ids` therefore keys on the directory's *name*
  ([`run::is_run_id`]), never on a readable `run.json`: filtering on the state
  file made run `88c0` invisible to `magi list`, to `runs_unreadable`, and to
  every phone route, so nothing could report it and nothing could clear it.
- **An agent that cannot write files reports it in prose and nowhere else.**
  Run `4043`'s implementer said the disk had 8.9 MB free; the run was recorded
  as "blocked with one finding open". `daemon::disk_gate` holds a task before
  minting a run instead, which spends no attempt and leaves it in the list.

Housekeeping is best-effort by construction: one run's fold failure is a
`tracing::warn` and the pass continues (`clean::fold_due`). A `?` there stopped
automatic folding permanently at the first run git refused to let go of.

**A different schema number is not "unreadable", and conflating the two once
stopped the janitor fleet-wide.** `clean::fold_due` used to require a full
`RunState` to pass the same schema check `RunState::load` enforces for
`--resume`, and treated any mismatch as unreadable - silently skipped, not
counted, not logged. The moment magi's own `run::SCHEMA` bumped, every run
still on disk failed that check at once: on one operator's machine this was
90 of 93 runs, folding 0 per pass, for months, with nothing anywhere saying
so (`Housekeeping::unreadable` was defined but never incremented). The fix
narrows "unreadable" to what `RunState::load` cannot do at all - `run.json`
failing to parse - because folding only ever reads worktree paths, branch
names and a tally winner off the struct, and every schema bump so far has
only added a field or a variant, never repurposed one; an old record's values
are exactly as good for that purpose as a current one's. `RunState::load`
itself keeps the strict check, because *its* callers (`--resume`, the review
loop, the tally) do recompute against a field's current meaning and must not
guess at a value an older schema never had a chance to mean correctly.

### Never take a port you did not check

A UI verification fixture was started on **8791** in a temp directory, and that
is the port `nagi`'s market maker listens on. The fixture won the bind, the
agent that started it then died without cleaning up, and the trading service
sat unable to reclaim its port until someone noticed. Worse, the first
diagnosis was wrong: `Get-NetTCPConnection` showed `8791 python` and that was
read as "nagi is fine" — the fixture *was* the python process.

So, for anything that listens on this machine:

- **Bind port 0** and let the OS choose, or check the port is free first.
- The operator's occupied set today is `8080` kanade-backend, `8188` yaiba,
  `8788` / `8789` / `8791` nagi, `4222` / `8222` nats, `6123` glazewm,
  `6124` zebar, `7878` magi. Treat it as a floor, not a list: check.
- **Identify a listener by its command line, never by its process name.**
  `Get-CimInstance Win32_Process -Filter "ProcessId=<pid>"` and read
  `CommandLine`. Two unrelated services are both "python".
- A brief that tells a subagent to stand up a server MUST give it the port
  policy. This one only said "in a temp directory outside the repo", and the
  agent had no way to know 8791 mattered.

### Landing a run's winner by hand

A candidate branch holds one commit, subject `magi: candidate A (uncommitted
work)`. `gh pr merge --squash` prefers that commit's message over the PR title
when there is only one commit, so `main` ends up reading
`magi: candidate A (uncommitted work) (#16)` — which says nothing about what
landed. Pass the subject explicitly:

```sh
gh pr merge <n> --squash --delete-branch --subject "feat: what it actually did"
```

Also **rebase before opening the PR**. A run branches from the commit it
started on, and anything merged in the meantime shows up in its diff as a
deletion — the first autonomous run appeared to delete a test that had landed
while it was thinking. `magi run --merge pr` builds its title from the first
line of the body and is unaffected; this is only the hand-driven path.

**A non-zero `gh pr merge` does not mean the merge failed.** In this
repository it usually means the opposite. jj keeps git HEAD detached, so
`--delete-branch` finishes with

```
could not determine current branch: failed to run git: not on any branch
```

*after* GitHub has already merged. Every hand-driven merge in this repo prints
it. Check `gh pr view <n> --json state` — or `git log origin/main` — before
concluding anything, and never re-run the merge on the strength of the exit
code. `land::merged_after_all` is that rule for the unattended path: run ec12
landed pull request 28 and recorded `ok: false`, and its task was held waiting
for a merge that was already in `main`.

**When magi never got as far as opening a pull request at all** - `gh pr
create` itself failing (run 2963's title was over GitHub's 256-character
GraphQL limit for `createPullRequest`), a dead token, `gh` unreachable - there
is no PR for `land::merged_after_all` to recover through, because none of
magi's own machinery ever ran against one. If a human (or an agent acting for
one) then opens and merges a *different* pull request from the same branch by
hand, the run's own record is stuck exactly where `merge` left it: `status`
never becomes `Merged`, `merge` keeps recording the create failure, and the
run sits in the web UI's "In flight" section (and the terminal deck) forever,
usually with a `superseded_by` pointing at whatever later run actually got
the task done. `magi task done` closes the task; it never touches the run.

`magi fold --merged <pr-url>` is the fix: it reads the pull request back with
`land::lifecycle` (refusing outright if it is not actually merged - never
guess from a URL alone), then feeds it through `land::land` exactly as the
automatic post-merge loop would have, which rewrites `status` and `merge` to
match reality before the usual `fold` cleanup runs. **This path never calls
`bump::after_merge`** (`src/bump.rs`) - that call is made only from
`graph::Runner::run_land`, which a manually-corrected run never passes
through - so a release version bump the change might have earned is not
filed automatically even after the correction; request one by hand if the
change warrants it. Fixing that gap is future work, not something this
correction path attempts.

### Running magi on magi

`magi.toml` in this repo sets `e2e = cargo test` and `gate = cargo make check`,
both with a **shared** `CARGO_TARGET_DIR` under `{{ vars.cache }}`. Without it
every review round rebuilds this crate from scratch in the winner's worktree,
which dwarfs the agent latency it is measuring. `magi fold --all` afterwards:
`candidates + judges + reviewers` worktrees exist at peak.

**Never build by hand into the directory `verify` uses.** Two different source
trees alternating through one `CARGO_TARGET_DIR` will link a test against the
other tree's stale `libmagi`, and the compiler then reports missing fields on
types that plainly have them — `no field met_quorum on type &Tally` for a struct
whose declaration is right there. That cost half an hour of chasing a phantom
rebase. Give every manual build its own directory:

```sh
CARGO_TARGET_DIR=/tmp/magi-dev cargo make check       # in the main worktree
CARGO_TARGET_DIR=/tmp/magi-<run> cargo make check     # in a candidate's
```

### The local toolchain may not be the one CI uses

`rust-toolchain.toml` pins `stable`, but a `RUSTUP_TOOLCHAIN` environment
variable overrides it silently, and this machine has had it set to an older
release — so a clippy lint that CI fails on is invisible locally. Verify with
the channel CI actually runs:

```sh
RUSTUP_TOOLCHAIN=stable cargo make check
```

### Bare `magi` must not raise a screen in a pipe

`main` resolves an absent subcommand to `Tui` only when `stdout` is a terminal,
and to `Show` otherwise. Without that, `magi | head` and any CI invocation would
enter the alternate screen and block on input forever.

### Tests must not touch the operator's history

`run::set_home` (and `MAGI_HOME`) exist so tests never write into
`<data_local>/magi`. `report::set_color` exists so rendering is assertable —
that is also why there is no colour crate: the ones available decide for you
whether the stream supports colour, which makes the output untestable.

### Changes to this repo go through the graph

A feature or a fix here is written as a task file and put through the graph —
`magi run --file <path>` — not typed into `src/` by whichever agent happens to
be open. In rough order of why:

- **It is the only honest test.** Whether blind competition yields a mergeable
  result on a real Rust repository is not knowable from the outside; the suite
  can prove that the graph moves bytes correctly and nothing more.
- **It surfaces operational defects nothing else reaches.** The first attempt to
  run magi on magi found that jj keeps git's HEAD detached at the working-copy
  commit, so magi refuses to start on most repositories in this fleet.
  `base = "main"` in this repo's `magi.toml` is the workaround, and no test saw
  any part of it.
- **It accumulates a record.** The win rates and reviewer precision behind
  `magi stats` only mean something over a workload whose distribution of tasks
  we choose ourselves — which is this repository.

### What stays with the human

The graph implements intent; it does not decide what magi is for. These do not
go into a task file:

- **The rules and conventions themselves**, this section included. A candidate
  that could rewrite the standard it is judged against is not competing.
- **The task statement.** A vague task buys three vague candidates and a
  coin-toss tally. Write the mechanical constraints exactly and leave the design
  open — that gap is where blind judging does its work.
- **Visual and UX judgement.** No judge sees a rendered SVG or a running TUI.
  Someone has to look at the real thing on a real terminal.
- **Destructive or irreversible operations.** Three candidates run unattended
  and in parallel, and no node stops to ask. Anything that outlives deleting a
  worktree is not something to hand to three of them at once.

### Every friction with magi becomes a task

- If using magi is annoying, write the task file and run it, however small the
  fix looks. Left alone, the tool stays as inconvenient as the day it was
  written, because the one person who notices has memorised the workaround.
- **One change per competition.** Bundling unrelated fixes into one task makes
  the diff unjudgeable and the statistics meaningless.
- The examples already collected, which are the queue to work from:
  - Per-node durations are absent from the report, so the numbers had to be
    computed by hand out of `run.json`'s `events` with `jq`.
  - For the ten minutes the implementation nodes ran, `magi show` printed
    `0 files, 0 commits, 0s` for all three candidates. Telling a live agent from
    a dead one meant a human checking file mtimes in the worktrees and the
    absence of `artifacts/*.out`.
  - An opencode judge dropped out on a permission refusal, and the report showed
    it only as one ranking fewer.

### The notification centre is not the question queue

`src/notices.rs` backs the web header's bell. A notice is something to *know*
(a failed release bump, a blocked run, a held task, a disk hold), never
something to answer, so it does not reuse `ask::Questions`.

- **Dedupe key = kind + subject, never prose** (`release-bump:<run>`,
  `task:<id>`, `disk:<repo>`). The file name is derived from the key by a
  stable FNV-1a (`notices::id_of`) - not `DefaultHasher` - so processes and
  builds agree on it and no lock is needed. Keep producer messages free of
  anything that varies between retries: a changed message relights a read
  notice, so a number in the wording turns a retry loop into a flood.
- **Dismiss is a tombstone.** An identical re-raise does not resurrect it; a
  changed message or a higher severity does. Consequence accepted: a real
  recurrence with identical wording stays hidden until the 200-notice cap
  prunes the tombstone.
- **`notifications_rev` lives in three places that must stay aligned**: the
  `events` tuple, `HealthView`, and `applyRevisions_` / the initial load in
  `app.js`. Miss one and the badge stops updating live (health is the
  fallback when the stream is down).
- **Producers go through `notices::raise` / `raise_in`** and are best-effort:
  a failed write is a `tracing::warn`, never a failed run. `raise` is a no-op
  in a unit test that never pinned a home (`run::try_home`).

- **One cause pages once.** A `Notice` carries `subjects` (the task / run ids it
  is about) and `covered_by`. When an open question whose `run` equals a
  subject exists and is about the same kind of cause (a conduct / triage question
  covers that task's hold and handover notices, a question from inside a run
  covers that run's ended / stopped notices, and the two never cross) - at raise time (`notices::raise_in`) or is filed afterwards
  (`Questions::put`, first write only, via `notices::quiet_for`) - the notice is
  still written, but already read and pointing at the question. Dedupe is
  expressed as read state, never as a tombstone, so a recurrence with a changed
  message or higher severity pages again; `Task::hold_reason` is untouched, so
  `magi task show` keeps the reason. The release / land question
  (`bump::NOTICE_NODE`) never covers anything.
- **`Notice::since` identifies the cause.** A task hold's notice carries
  `Task::held_at` (stamped on entering `Held`, kept by a re-hold, cleared by
  `release`; `queue::SCHEMA` 9). `notices::covers` ignores a question filed
  before `since`: it is about an older cause, and silencing a new hold or
  handover failure behind it would hide the failure. A question filed after the
  hold still covers it however late. Missing a duplicate costs one extra page;
  hiding a failure costs the failure, so the rule errs towards paging. A notice
  with no `since` keeps the old broad rule. `Notices::cover` re-checks under
  the lock. A later `since` on the same key relights the notice - the one
  exception to the tombstone rule: hold, release, hold again pages again.
- **A notice also fires `[notify]`, decided in one place.** `Notice::raise_again`
  returns whether the raise pages (relit and not covered; a brand-new
  uncovered notice pages too); `notices::raise_in_with` sends a `Page` through
  an injected closure. The real sender runs on its own thread and runtime and
  is a no-op until `notices::install_pager()` (called in `async_main`), so a
  test that pins a home never reaches the operator's config. A producer with a
  repo config passes it via `notices::raise_with`; every other one gets the
  machine layer's `[notify]`. `land::announce_red_merge` must not call
  `ask::notify_text` itself: the notice already pages, and a second call would
  page a merged-red twice (and ignore `covered_by`).
- **A refused handover is one page.** `daemon` no longer raises a separate
  `handover:<id>`; the hold's own `task:<id>` notice (from `Queue::put`) is the
  page, and the release guidance rides in the hold reason with fixed wording.
  `covers` still matches a `handover:` key for records written earlier. A page
  already delivered cannot be retracted when a question arrives later.

### Duplicate-work claims are gathered from several places

`src/dupes.rs` refuses (`--force` overrides) a `magi task add`, `magi run` or
`magi review`, and a web edit that changes a task's instruction, when it names
a branch, a commit SHA or a pull request that unfinished work already owns.
Only concrete identifiers match, never text similarity. What counts as
"owned" is spread over `Task::review_branch`, `Task::runs`, and each run's
`candidates[].branch` / `pr` / `base_commit` in `run.json`, none of which a
compiler ties together, so a new field that names a branch or PR belongs in
`dupes::task_claims` / `run_claims`. `run.json` is read through a tolerant
view, not `RunState::load`, so a schema bump cannot silently drop claims. The
daemon's retries, requeues, review requests and resumes never go through this
check: they would collide with themselves. A PR number the text names that no
record explains is asked of the forge (`gh pr view`, 5 s cap, any failure is
"unknown" and ignored); tests inject that lookup via `dupes::check_with`.

### A run record's `pr.state` is the last value *its own* land loop polled

A run handed over (`released_to`) or left blocked while another run landed the
same pull request never hears about it, so its record said `open` forever (7 of
9 such records on one machine were merged on the forge). Three layers, cheapest
first, none of them optional:

- **Source.** `land::land` (merge, already-merged, closed) calls
  `land::write_pr_state_through`, which rewrites `pr.state` on every *other
  terminal* run of the same repository naming the same url / number (never a
  non-terminal run, never one a live daemon claims, only a record still saying
  `open`, only `merged` / `closed`), through `RunState::save_under`, best effort.
  `magi fold --merged` goes through `land`, so it writes through as well.
- **Repair.** `magi fold --repair-prs` (idempotent) asks the forge about each
  distinct pull request a terminal run still records as open and rewrites the
  merged / closed ones; an open one or an unreadable forge changes nothing. The
  janitor (`clean::housekeep`) runs the same pass, capped at 5 lookups a pass.
- **Guard.** `dupes::Staleness`: a recorded-open PR on a *terminal* run claims
  only if its run was not released to a readable successor and the forge (one
  lookup per number, at most 5, none after the first failure, 5 s each) does
  not say merged / closed. Unreadable keeps the claim - claiming too much is
  the cheap error. Records written before the repair stay safe through this.

The web run page shows a finished run's recorded-open PR as "last seen open"
(`landView` in `app.js`), never as live landing state.

Remaining gap: the write-through only reaches runs under the same magi home, and
a run that was *not* terminal when the PR landed keeps `open` until it ends and
the repair / janitor pass reaches it.

### Task attachments live beside the task file, outside the worktree

`magi task add|edit --attach <PATH>` copies a file into `<queue>/<id>.attachments/`
(`Queue::attach`), never a `*.json`, so `Queue::list` and `revision` do not see
it; `Queue::remove` deletes the directory. Names must pass
`ask::valid_asset_name` and are numbered, never overwritten. `queue::SCHEMA` is
bumped to 7 for `Task::attachments` (the existing convention: schema 6 was a
field-only bump too); `run::SCHEMA` is not, since `RunState::attachments` is
additive and `migrate_schema` would otherwise need another arm. The daemon
refreshes `RunState::attachments` from the task on every start and resume, and
the implementer prompt lists the absolute paths under `# Attachments`. `task
done` does not delete them.

### A merge must not take open findings with it

`src/followup.rs` files the findings a run's last review round left open as
queue tasks once the pull request is confirmed merged. The merge itself is
never blocked or changed by it (`land_approval`, `review_rounds` untouched);
`[graph] file_followups` (default on) switches it off.

- **What is filed.** Read from the last `ReviewRound` directly, never from
  `open_findings()` (empty on a clean round) or the fixer's report: every
  Major-or-above finding, plus every finding of a seat whose *final* vote
  (`final_votes()`) was reject - a vote has no per-finding reasoning, so that
  seat's Minor/Nit come with it. **Other Minor/Nit are not filed**; they are
  listed in the pull-request comment only.
- **Grouping is deterministic, never an agent call.** Same file with lines
  within 5 of each other, or, with no file, the same normalized title. One
  task per group, carrying every finding's full text.
- **Idempotent three ways.** The task id is derived from run id + the group's
  finding ids and written with `Queue::create_new` (never `put`, which would
  rewind a task that has since run); the queue is scanned for tasks whose
  `FollowUp::run` is this run; and `RunState::followups` remembers what was
  filed, so a completed or deleted follow-up is not recreated. Tasks found by
  the queue scan are written back into `RunState::followups`, so a crash
  between filing and the state save loses no record.
- **Attribution.** `Source::Agent { run, node: "followup" }` plus
  `Task::followup` (`queue::SCHEMA` 8). Anything counting agent-filed tasks
  from `MAGI_RUN` must look at `node`. Filed `solo`, through the normal
  attempts accounting. `dupes::check` is deliberately **not** run: the
  instruction names the merged pull request, which it would refuse.
- **Depth cap.** `Task::followup.generation` (ordinary task 0); a task of
  generation `MAX_FOLLOWUP_GENERATION` (2) files nothing and the event says so.
  The daemon records the task's generation in `RunState::followup_generation`
  when it starts or resumes the run, so deleting the parent later cannot reset
  it; a run with an origin task whose depth was never recorded and whose task
  is gone is treated as at the cap (nothing filed), never as generation 0.
- **Best-effort.** `followup::after_merge` returns `()`, records failures as
  run events and never touches `status`. It is called from
  `Runner::run_land` after `bump::after_merge`, independent of it, and from
  `land::correct_merge`, so `magi fold --merged` and the janitor's external
  merge reconciliation file follow-ups too.
- **Visibility.** A `filed N follow-up task(s): ids ...` event, a "follow-ups"
  section in `magi show`, task links on the web run page, and one English
  comment on the pull request (marker `magi-followup run=<id>`), posted only
  when something is filed, and retried by a re-entered `land` if it failed.

### origin/main is kept fresh by its own task, and only refs/remotes move

`daemon::fetch_loop` runs `clean::fetch_origins` every `[repos] fetch_interval`
seconds (default 600, `0` disables) over the checkouts `magi repos` lists. It
is `git fetch origin` and nothing else, so only `refs/remotes/origin/*`
changes: never a branch, HEAD, index or working tree (primary checkouts are
detached and often dirty). It is its own task rather than part of the janitor
because the janitor only runs on idle ticks, so `origin/main` would go stale
during a long run, and a slow remote must not hold up the poll loop. Failures
and the 30 s per-repo timeout are warnings; `talk::briefing` tells the Chat to
read code from `origin/main`.

### The web UI is checked in a real browser (`tests/web_render.rs`)

`web.rs`'s own tests cannot see a layout: a Queue row that lost its title, or
two labels on top of each other, only exists once CSS has run.
`tests/web_render.rs` starts the real router on `127.0.0.1:0` over a seeded temp
home (tasks in every state, runs, chats), opens Queue / Runs / Chat in headless
Chrome at 1280px and 390px (mobile), and asserts structure, not pixels: every
visible row has a visible non-empty `.card-title`, titles do not overlap the
chips beside them, nothing spills out of the list pane, the sticky header does
not cover the `<h1>` (`elementFromPoint` at its centre), and the console is
clean (exceptions, `console.error`, `Log.entryAdded` errors such as CSP
violations; only a favicon request is tolerated).

```sh
RUSTUP_TOOLCHAIN=stable cargo test --locked --test web_render -- --nocapture
MAGI_CHROME=/path/to/chrome cargo test --test web_render   # name a binary explicitly
```

- **No Chrome, no failure — locally.** Without `MAGI_CHROME`, a Chrome/Chromium
  on `PATH`, or a default macOS/Windows install, the test prints `SKIP` and
  passes. With `CI` set a missing browser is a **failure**: a check that quietly
  stops running is worse than a red one. `ubuntu-latest` ships Chrome;
  `ci.yml` is kata-managed and is not edited for this. A Chrome that is found but
  will not start is a failure, never a skip.
- **No dependency for the shipped binary.** The DevTools client is
  `tests/web_render/cdp.rs` over the dev-only `tokio-tungstenite`; Chrome gets
  `--no-sandbox` (CI containers run as root) and `--remote-debugging-port=0`.
  The server binds port 0 too. Never hard-code a port in this test (see "Never
  take a port you did not check").
- **Known failures are per assertion.** `KNOWN_FAILURES` lists `(page, check,
  reason)` for layouts another task is already fixing. A hit is reported, not
  failed; an entry that no longer fires prints `NOTE: ... remove it`. Do not
  add page-wide exclusions or `#[ignore]`; when you fix the layout, delete the
  entry.
- The Runs page keeps finished runs in a collapsed section that renders no rows
  until opened, so the harness seeds one in-flight run and only requires that
  one (`Case::expect`).
