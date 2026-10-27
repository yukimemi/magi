# Recovering exhausted base synchronization

A saved base-sync stop is terminal `Blocked`, even if an older launcher saved
it as `Voting`. Resume without a new explicit owner approval does not grant
another rebase. The shared resume admission policy suppresses that offer, and queue settlement holds the
existing task instead of starting a replacement competition.

After inspecting retained changes and current ownership, an owner can authorize
one additional synchronization for movement exhaustion only:

```sh
magi run --resume <saved-run> --recover-base
```

This uses the existing owning task, owner lock, driver lease, and worker path.
A live or unknown driver, transferred ownership, dirty retained tree, or a
non-latest run refuses authorization. Genuine rebase conflicts, failed branch
publication, and unknown legacy stops cannot use this recovery. Legacy
exhaustion requires the exact four-rebase stop message and counter.

Authorization records the task, run, stopped candidate HEAD, base tip and time.
Consumption is saved before Git runs and reserves one additional attempt;
ordinary resume after an interrupted attempt cannot spend it again. A new
explicit `--recover-base` invocation is a new authorization. An explicit queue
release after the stop also authorizes recovery through the daemon; its approval
generation can authorize only once, including after another base movement. A
request already pending must be resumed instead of authorized again. The cumulative attempt
counter is never reset. Recovery requires a successful base fetch, rebases onto
that fetched SHA and retains existing publication leases. If the base moves
again, the run stops again rather than chasing it.

Previous review and gate evidence is archived with the consumed authorization,
including failed or timed-out gates. Fresh required review, E2E and gate work
must complete on the recovered head. Merge approval remains head-specific and
is not granted by recovery. Saved run configuration still applies: changing a
repository timeout does not update an existing run. A previously timed-out gate
is not success; if the saved timeout is insufficient, resolve that configuration
through an independently supported configuration/review workflow before landing.

Unchanged stops retain their time and notification identity across driver
passes. No task, branch or worktree must be deleted to use this transition.
