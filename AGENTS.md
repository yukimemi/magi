<!-- kata:agents:base:begin -->
## Shared conventions

This file is the agent-agnostic source of truth (per the
[agents.md](https://agents.md) convention). The matching
`CLAUDE.md` and `GEMINI.md` files are thin shims that point back
here so each tool's auto-load behaviour still finds something.
**Edit AGENTS.md, not the shims.**

### Git workflow

- **No direct push to `main`.** Open a PR.
  - Exception: trivial typo / whitespace / docs wording fixes.
- Branch names: `feat/...`, `fix/...`, `chore/...`.
- **PR titles + bodies in English. Commit messages in English.**
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

### PR review cycle

- Every PR runs reviews from **Claude Code**
  (`.github/workflows/claude-review.yml`, kata-managed) and
  **CodeRabbit**. Wait for both bots to post, address their
  comments (push fixes to the PR branch), and merge only after
  feedback is resolved. The claude-review workflow skips
  review-exempt PRs by itself (its job-level `if:` excludes
  `chore/release-*`, `kata-apply/auto`, `apm-bump/auto`, and
  Renovate / Dependabot authors) — a missing Claude review on
  those PRs is expected, not a failure.
- **Any PR that touches the Claude workflow files goes
  unreviewed.** `claude-code-action` requires the workflow file to
  already exist on the default branch **with identical content** —
  otherwise a PR could rewrite the workflow to exfiltrate the
  token. When the content differs it logs "Skipping action due to
  workflow validation" and exits 0 without reviewing: a green
  check with no review attached. This covers two cases, and the
  second is the one that keeps surprising people:
  - the PR that first adopts these templates (the workflow does
    not exist on the default branch yet), and
  - any later PR that **edits** `claude-review.yml` / `claude.yml`,
    e.g. hand-pulling an upstream template fix.

  Not fixable from this side — it is the mechanism that makes the
  token safe to hand to the action at all. Expected: merge on CI +
  owner approval; reviews resume on the next PR that leaves the
  workflows alone. The `kata-apply/auto` branch is already excluded
  by the job-level `if:`, so the daily template-refresh PRs do not
  add noise here.
- **A missing credential fails loudly instead.** If the repo has
  neither `CLAUDE_CODE_OAUTH_TOKEN` nor `ANTHROPIC_API_KEY` set,
  the guard step fails the job — set one and re-run (subscription
  path: `claude setup-token` → `gh secret set`; pay-as-you-go:
  store `ANTHROPIC_API_KEY` and swap the action input to
  `anthropic_api_key`). Distinguishing the two: **red** means no
  credential, **green with no review** means workflow validation.
- **The Claude full review fires once, at PR open** (plus
  `ready_for_review` / `reopened`) — fix pushes do **not** re-trigger
  it (`synchronize` is deliberately off the trigger list; a full
  re-review per push doubled up with the mention-driven re-check
  below and burned tokens for no extra signal). Verification of
  fixes rides the `@claude` thread replies. After a large rework
  that changes the PR's shape, request a fresh full pass
  explicitly: `@claude please re-review the full PR`. CodeRabbit
  still reviews pushes on its own cadence (its app config, not
  this workflow).
- **After opening a PR, immediately enter the review-monitoring
  loop — do not ask the user whether to start it.** Drive the
  cadence with `/loop` — fixed-interval mode (e.g.
  `/loop 60s …`) schedules ticks via `CronCreate`; dynamic mode
  (no interval, `/loop …`) self-paces via `ScheduleWakeup`. The
  agent actively pulls fresh state each tick with
  `gh pr view <N> --json state,reviews,comments,statusCheckRollup`
  and `gh api repos/<owner>/<repo>/pulls/<N>/comments` (the
  latter covers inline review comments, which `gh pr view`
  does not surface) and reacts to new bot feedback. Passive
  watchers (background `gh` polls, file watchers, hooks) cannot
  trigger active follow-up, so they are not a substitute —
  without an active wake-up the agent never re-reads the PR.
- **Default polling interval: 60s.** Claude Code review /
  CodeRabbit typically reply within ~1–5 minutes of a push or
  thread reply, so a 60s tick catches them on the next wake-up
  without burning cache: 60s sits well inside the 5-minute
  prompt-cache TTL, so the conversation context stays cached
  across ticks. Do **not** stretch the interval to 300s — that
  is the worst-of-both window (you pay the cache miss without
  amortizing it). If the PR is idle but a bot re-review is still
  expected (e.g. a CodeRabbit rate-limit refill window), step
  **up** to 1200–1800s instead.
- **Stop the loop entirely when only owner approval is missing.**
  Once review bots are quiet (or quiet-by-exception — version-bump
  skip, Renovate/Dependabot skip), CI is green, and there is no
  other expected follow-up, the *only* remaining action is human
  approval. GitHub already notifies the owner; the agent
  re-entering on every cron tick to find the same "still waiting
  on owner" state burns cache and adds no value. Stop scheduling
  further wake-ups (`CronDelete` in fixed-interval mode; simply
  omit the next `ScheduleWakeup` in dynamic mode) and report the
  wait state to the user. The owner restarts the loop after their
  next push if a fresh bot pass is wanted, or merges directly.
  (A CodeRabbit rate-limit window doesn't qualify on its own — a
  re-review is still expected once the quota refills, so step up
  to 1200–1800s instead and let it ride. Stopping is only correct
  when the owner has explicitly chosen to skip the bot pass per
  the rate-limit exception below.)
- **Reply to reviewers after pushing a fix — in each thread, not
  at the top level.** Every finding lives in its own inline review
  thread; answer *each* one as an in-thread reply, carrying an
  **@-mention** (`@claude` / `@coderabbitai`). Use the review-
  comment *replies* endpoint — `gh api repos/<owner>/<repo>/pulls/<N>/comments/<comment_id>/replies -f body=…`
  (or `-F in_reply_to=<comment_id> -f body=…` on the comments
  endpoint — `body` is required there too) — and
  get each comment's `<comment_id>` from
  `gh api repos/<owner>/<repo>/pulls/<N>/comments`. A single
  top-level `gh pr comment` does **not** count: it leaves every
  inline thread unresolved, the bot can't tie your response to the
  finding it raised, and the per-finding audit trail is lost.
  Reply in-thread even when you're **declining** a suggestion —
  say why; a silent skip reads as overlooked. Note `@claude` also
  triggers the interactive responder
  (`.github/workflows/claude.yml`, kata-managed) — it will
  re-check the fix and reply on the thread. Since fix pushes no
  longer re-trigger the full review, this mention-driven re-check
  is the **only** Claude-side verification of a fix — don't skip
  it for substantive fixes; do skip it for pure FYI notes that
  need no verification.
- A review thread is **settled** the moment the latest bot reply
  is ack-only ("Thank you" / "Understood" / a re-review summary
  with no new findings) or 30 minutes elapse with no actionable
  comment.
- **Merge gate**: review bots quiet AND owner explicit approval.
- Bot-authored PRs (Renovate / Dependabot) skip the bot-review
  gate; CI green + owner approval is enough.
- **Version-bump-only PRs** (a single `chore/release-vX.Y.Z`
  branch whose entire diff is `[workspace.package].version` /
  `[package].version` + the matching inter-crate refs +
  `Cargo.lock`) **also skip the bot-review gate.** There is
  nothing for the bots to find in a version bump, and the
  release pipeline downstream of merge (auto-tag → release.yml)
  is time-sensitive. CI green + owner approval is enough.
- **Treat CodeRabbit rate-limit notices as "quiet" for the
  merge gate.** If CodeRabbit only posts a "Review limit
  reached" quota-exhaustion message (no findings, no inline
  comments), it has produced no review content — there is
  nothing to address. Re-trigger with `@coderabbitai review`
  once the quota refills if you want a real pass; for small or
  time-sensitive PRs, merge on owner approval without waiting.

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
cargo make check                    # fmt --check + clippy + test + lock-check (the pre-push gate)
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

magi drives subscription CLIs (`claude -p`, `opencode run`, `agy -p`) and never
an HTTP API. That is a product decision, not an unfinished one: the CLIs carry
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
CLI conversation. The three mechanics were established empirically, not from
docs, and each is asserted in `src/agent.rs` tests:

| CLI | open | resume | trap |
|---|---|---|---|
| `claude` | `--session-id <uuid>` | `--resume <uuid>` | the uuid must be RFC 4122 v4 or the CLI rejects it (`src/rng.rs` mints it) |
| `opencode` | `--format json` → `sessionID` | `-s <id>` | nothing to resume until a turn reported an id |
| `agy` | `--output-format json` → `conversation_id` | `--conversation <id>` | print mode defaults to a **5 minute** timeout; `--print-timeout` must track the node budget. `--disable-slash-commands` silently disables `--mode`, so magi never passes it |

`agent::has_session` is the single place that decides whether a follow-up prompt
may rely on memory. If it says no, the node re-sends full context. Never assume
a resume worked.

### opencode has no read-only mode

`--auto` gates **every** permission in opencode, reads included. A judge or
reviewer seat spawned without it cannot even open its own prompt file and drops
out with *"the user rejected permission to use this specific tool call"* — which
is exactly what happened on the first live run, silently costing a seat on the
panel. So magi always passes `--auto` for `kind = "opencode"`, and read-only-ness
for those seats rests on the prompt plus the fact that judge worktrees are
deleted after the tally and reviewer worktrees are `reset --hard` to the commit
under review every round. Do not "fix" this by withholding `--auto`.

### teravars renders the whole config file

`Config::load_layers` runs every layer through teravars, so:

- **Comments are templates too.** `# see {{ system.* }}` is a render error, not
  a comment. `Config::starter_toml` learned this the hard way.
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
  `reviewers of a patch`, `Your patch was reviewed`. Rewording a prompt heading
  in `src/prompt.rs` breaks the end-to-end tests — which is the intended alarm,
  but update both sides together.
- **`blind.strip_lines` is consumed twice**: as case-insensitive substrings by
  `blind::strip_attribution`, and as generated `sed` addresses by
  `blind::commit_msg_hook`. Entries must stay plain literals, not regexes.
- **`run::SCHEMA`** must be bumped whenever a `RunState` field changes meaning,
  or a resumed run will half-read someone else's state file.

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
