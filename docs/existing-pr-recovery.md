# Recovering an existing PR

An existing-PR recovery has an explicit, persistent purpose:

```sh
magi task recover-pr <task> --pr <url> --head <full-public-SHA> --disposition land
magi task recover-pr <task> --pr <url> --head <full-public-SHA> --disposition review
```

`land` replaces an inherited `merge=none` with `pr`, enables landing and
requires fresh owner approval. `review` replaces the mode with `none` and
finishes after local review. Other task overrides are preserved. The command
prints the replaced merge override. Old selections without a disposition are
held before starting new work; select again explicitly. An existing published
recovery with a matching PR and an approval binding can resume its saved
delivery workflow without a new selection. Its saved validation and all
final-head and owner-approval checks still apply.
Completed tasks can also be selected again through this command.

`--head` is the public update lease. Optional `--source-head <full-SHA>` is the
immutable implementation to adopt, including a previously reviewed fix. It must
exist locally. A descendant of the public lease follows the ancestry path.
For a retained source rebased by the launcher, also pass
`--source-run <full-retained-run-id>`. The command verifies the saved run's
repository, task, PR, original public lease, successful base-sync, retained
candidate ref and final recorded review head. Its recorded rebase base must be
an ancestor of the fetched landing base. It replays every selected public
commit against that base in a private bare object database and requires exact
tree equality at every corresponding source commit. The complete source,
including all later recorded review fixes, is adopted unchanged. Old competing
candidates are not implementation inputs. Fetch the intended
source into the target repository first if necessary. Source retention sidecars
protect historical runs from cleanup; recovery does not move their branches.
A changed public head requires a new selection rather than an unconditional push.

The selection and new run retain full public/source/base SHAs, the ordered
commit/tree correspondence and every later fix SHA. Startup and execution on
resume recompute this proof and reject changes rather than silently updating it.
Older run records may supply the necessary selection, base-sync, retained ref,
rebase event and review evidence without having a new proof field. Missing
records/objects, empty or merge commits, conflicts and modified replays fail
closed with the missing evidence or differing paths. Patch similarity alone
never authorizes adoption. Restore the retained evidence, or explicitly
reconstruct and review a descendant source; do not weaken the lease or overwrite
files from old main to make the check pass.

Delivery requires nonempty `verify.gate` and `verify.e2e` commands and at least
one reviewer/round. Blank commands are rejected. `pre_gate` is optional, not
verification. A missing root `magi.toml` is valid: machine layers or the remote
base's `.magi/config.toml` can provide the commands. Configure the repository's
actual documented checks; no commands are guessed from the language or prompt.

`--config <file>` follows the normal **complete layer replacement** semantics.
Include the desired roster/settings explicitly in that file. Without it, the
existing machine/remote-base layering and task/daemon inheritance rules apply.
At dispatch, one fetched load supplies both the actual configuration and its
ordered inputs. New recovery runs save `recovery_config` beside `config`:
input paths (remote cache paths include the immutable source commit), task
options, and any inherited daemon merge override. The final resolved commands
are in `config.verify`; reporting provenance does not re-render the files.
A resumed run retains its original configuration and source selection, even
when the current settings have changed or their input files are unavailable. To
change validation after a completed review, select a new recovery with the same
source and corrected settings instead of editing saved task/run JSON.

An empty gate is **not configured / not tested**, including historical records
with `gate_ran=true`. It is not a successful verification result. Intentional
review-only workflows can still finish as `Ready` and complete their tasks.
Publication succeeds as `Ready`; it does not mean merged. A selected delivery
is complete only after the selected PR is confirmed merged, with final-head
review, configured e2e/gate evidence and a matching owner-approval binding.
Publication alone, a no-op `MergeOutcome.ok`, missing PR records, or an old
`Ready` result leave delivery held. Parked approval waits keep their existing
resume behavior. A stopped selected recovery requires explicit release or a
new selection; it never automatically starts another competition.

The normal landing path still checks CI on the exact public head, rechecks the
forge before merge, and binds the owner's merge approval and the merge command to that head.
Local review/e2e/gate evidence is checked both before initial publication and immediately
before requesting approval/merge. If a formatter, gate fixer, CI fixer or rebase
changes the head, old evidence cannot authorize a merge: request a fresh recovery
review using the retained new source. Unknown CI never proves a green check.

## Preserved delivery requiring real-broker validation

The inspected target repository tracking base was
`b41c8e31af4e8b40d4ab49632e932b5e0273ab4d`. Both repository layers
(`magi.toml` and `.magi/config.toml`) are absent at that commit. The inspected
machine layer has neither verification commands nor configuration includes. The old run
saved empty e2e/gate/pre-gate arrays and two clean reviewer responses at
`57cace1b9160fe340b2b4757b6153aa7d76e35e9`. That source remains available as
`magi/f0ee/A` in the target repository. It is **not** a descendant of the public
lease. Local Git shows five commits from shared base `0a10a898` to public tip
`3cb6ba20`, replayed onto `0ba31158e9f16c75e142982dc12e8b440ca71cfa`
through `ebac36a384c766fc7402a5855ecb991a89265483`, followed by reviewed fix
`57cace1b9160fe340b2b4757b6153aa7d76e35e9`. Run
`20261018-200357-f0ee` records that successful base-sync and both review heads.
A local isolated replay of all five original commits reproduced every rebased
tree exactly, in order:

| Public history | Rebased history |
| --- | --- |
| `e3c2f51e` | `12f8c46f` |
| `67f5298e` | `787c9e38` |
| `078b8639` | `c5a000ef` |
| `bf0992ac` | `f67327e4` |
| `3cb6ba20` | `ebac36a3` |

The subsequent fix changes three files (43 insertions, 9 deletions): it
consolidates the duplicate features table, replaces the deprecated atomic update
with a saturating compare-exchange loop, and records the accepted measurements
and their limits. The selected source retains that whole fix and newer main;
copying the old public tree would lose both. Use the rebase proof path below;
passing only `--source-head` correctly refuses this history. These are local
observations; public PR status must be queried again before recovery, and source
retention must remain enabled.

For future recoveries, prepare a complete target-specific configuration in
operator-owned storage. The preserved recovery's configuration is already
prepared: reuse it unchanged. The example below includes the existing machine
roster/settings with `include = ["machine.toml"]`
(the include is relative to this file). Verify the included file does not
already define these arrays. The target's `Makefile.toml` defines
`cargo make check`; `.github/workflows/integration.yml` defines the broker
checks and release backend setup. For this retained task's requested auth-switch
and twice-run role checks, the additional settings are:

```toml
include = ["machine.toml"]

[verify]
gate = ["RUSTUP_TOOLCHAIN=stable cargo make check"]
e2e = [
  "nats-server --version",
  "RUSTUP_TOOLCHAIN=stable cargo test --locked -p kanade-shared --test nats_auth_switch -- --ignored",
  "RUSTUP_TOOLCHAIN=stable cargo build --locked -p kanade-agent -p kanade",
  "RUSTUP_TOOLCHAIN=stable cargo build --locked --release -p kanade-backend",
  "KANADE_CONFORMANCE_BACKEND_BIN=\"${CARGO_TARGET_DIR:?set the shared build cache}/release/kanade-backend\" RUSTUP_TOOLCHAIN=stable cargo test --locked -p kanade-agent --test nats_role_conformance -- --ignored --nocapture",
  "KANADE_CONFORMANCE_BACKEND_BIN=\"${CARGO_TARGET_DIR:?set the shared build cache}/release/kanade-backend\" RUSTUP_TOOLCHAIN=stable cargo test --locked -p kanade-agent --test nats_role_conformance -- --ignored --nocapture",
]

[merge]
mode = "pr"
base = "main"

[graph]
land = true
land_approval = true
```

This shell example targets the inspected Unix environment. Confirm the inherited
shared cache is absolute and available to verification commands, `nats-server`
is installed, and the build prerequisites documented by the target repository
are installed. Broker tests launch their own local test brokers; do not deploy
or change running services. Include any additional required checks the target
repository adds before running recovery. These commands were matched to the
repository's files; this change does not claim that target validation ran.

Install the updated CLI and restart any existing queue daemon before selecting
the recovery. An older daemon does not understand the new source/disposition
contract. The command below uses the already prepared operator-owned
configuration under the cache directory; do not rewrite it. Then inspect the
existing public PR read-only:

```sh
gh pr view 1598 --repo kanadehq/kanade --json state,headRefName,headRefOid,baseRefName
```

Only if the open PR still names the expected task branch and public SHA, queue:

```sh
magi task recover-pr 20261011-214217-9462 \
  --pr https://github.com/kanadehq/kanade/pull/1598 \
  --head 3cb6ba206bc4f4d15b3b2b4d91e85012fbd1e659 \
  --source-head 57cace1b9160fe340b2b4757b6153aa7d76e35e9 \
  --source-run 20261018-200357-f0ee \
  --disposition land \
  --config "$HOME/.cache/kanade-work/kanade-recovery.toml"
```

This command queues real work, so it was not run for the preserved task during
this fix. The command and new options are exercised against a fake forge and
real local Git transport in `tests/graph_task_pr.rs`. No live forge mutation was
attempted; do not assume its old head is still current. Inspect the new run's
actual commands, results and final head,
then answer its fresh approval question only after its exact-head CI is green.
Do not resume the obsolete candidates or hand-edit queue/question records.
