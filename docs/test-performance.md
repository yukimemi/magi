# Test performance evidence and PR description draft

The suite has two reported problems: long worker-retention tests and graph
handover binaries, plus cold test builds reported at 8–25 minutes. The changes
retain every test and assertion. Worker retention now constructs terminal disk
records from a real published record instead of repeating durable reservation
publication and pruning hundreds of times. The retirement concurrency test
still requires the task update to finish before releasing the blocked proof;
its larger watchdog removes dependence on scheduler load, not the ordering
assertion.

The initial- and later-turn handover scenes now share one integration binary,
so Cargo compiles the large common fixture and links the executable once. All
17 cases remain independently discoverable by libtest and nextest. Names of
the seven later-turn cases acquire a `later::` prefix; the process-isolation
macro passes that full path to its child. The libtest child cap is reduced to at most four, matching the existing
nextest e2e group: combining the scenes with the previous half-CPU cap measured
31.533 s versus 27.986 s for the two original binaries. Larger concurrent
subprocess batches increased contention. Nextest groups, home locks, and
Windows shard configuration remain in place.
A test-only `sysinfo` optimization targets the process-table walks performed
when every mock CLI is cleaned up. The shell mock derives the artifacts directory with parameter expansion rather
than spawning `dirname` twice per ordinary invocation (and again for hang and
rebase cases). Attribution, context, and hang assertions still read those logs.

## Measurement method

`tools/test_timings.py` runs each supplied stable libtest executable normally
for binary wall time, then runs its listed tests individually with `--exact`
for attribution. Individual durations include process isolation and fixture
construction; their sum is not a parallel binary wall time. It fails if a test
fails or an exact selection does not report one passed test. It does not change
thread counts, group caps, or environment isolation.

Build executable paths come from Cargo's `--no-run` output. For example, use
`RUSTUP_TOOLCHAIN=stable cargo test --locked --test graph_seat_handover --no-run
--timings`, then pass `--binary graph_seat_handover=<executable>` and
`--output <report.json>` to the Python script. The executable path is an input
only and is excluded from the report. Use a quiescent shared cache for timing;
never create a separate target directory or purge the shared cache to force a
cold build.

The handover "before" snapshot is the start of review round 1, already including
the initial worker/retention/profile changes. It is not an upstream baseline
for those changes. Full per-test records are in
`test-performance/handover-before.json` and `test-performance/handover-after.json`.
A first after sample overlapped unit-test compilation and took 30.266 s;
it is retained as `test-performance/handover-overlapping-build.json` to show the
load sensitivity and is excluded from the comparison. A second sample without
concurrent compilation took 31.533 s before reducing the libtest cap; it is
retained as `test-performance/handover-before-cap.json`. Reducing that cap
alone measured 26.952 s (`test-performance/handover-after-capped.json`);
optimizing `sysinfo` was measured next, separately.

## Focused execution results

| Scope | Before | After |
| --- | ---: | ---: |
| Initial handover binary (10 cases) | 13.824 s | combined below |
| Later handover binary (7 cases) | 14.162 s | combined below |
| All 17 handover cases, binary wall total | 27.986 s | 22.211 s |
| Worker retention unit test, exact selection | 10.530 s | 2.460 s |

The handover total decreased 20.6% in these samples. This is a
single-machine observation; individual test times fluctuate. Both worker
measurements used the round-1 test profile, changing only the worker test's
fixture between measurements (`test-performance/worker.json`). These are
focused test totals, not full unit/integration suite totals.

The longest handover cases before this revision were the two-fixture
single-agent retry case (6.808 s), voting timeout (5.769 s), deliberation
handover (4.469 s), implementation timeout (4.409 s), and vote quota handover
(4.154 s). Real timeout cases retain their 2-second agent cap, 20-second caller
budget, and proof that the intended child reached the hang. Lowering these
would risk mistaking slow process startup for the intended hang.

The focused retention concurrency test also passed (0.93 s). `cargo fmt
--check` and `git diff --check` passed. Full verification belongs to the parent
run and was not rerun by this child.

## Build evidence and remaining required measurements

The first focused build of the two handover targets took 81.31 s in an existing
shared cache. Cargo's stable `--timings` report is summarized without machine
paths in `test-performance/focused-build.json`:

| Unit | Elapsed seconds |
| --- | ---: |
| magi library | 48.93 (frontend 33.86, codegen 15.07) |
| aws-lc-sys build script | 16.40 |
| axum | 12.27 |
| magi normal binary | 6.44 |
| initial handover test binary | 4.49 |
| later handover test binary | 4.48 |

These units overlap; their sum is not build wall time. Consolidation removes
one fixture compilation and link unit. Rebuilding just the combined test target
after its source changed took 6.21 s; this warm incremental number is not
comparable to the first build and does not establish a cold-build improvement.

With the test-only `sysinfo` override, a focused rebuild took 54.80 s:
`sysinfo` itself took 0.83 s, the library 49.40 s, the normal binary 4.83 s,
and the combined handover binary 2.32 s (overlapping units;
`test-performance/focused-build-sysinfo.json`). This is another partially
cached build, not a cold comparison.

Line-table debug information remains a build-size experiment. No measured
cold-build speedup is claimed. Release settings are unchanged. All four dev
dependencies are used: `pretty_assertions` in library tests, `tempfile` throughout
the fixtures, and `futures-util` plus `tokio-tungstenite` in browser tests.
Removing them would require replacing existing coverage machinery. Disabling
the CLI test harness would lose the tests in `src/main.rs`; consolidation avoids
one duplicate integration build without doing that.

Required before/after totals for the **entire** lib unit suite, **entire**
integration suite, and **cold all-target build** remain unmeasured in this child
seat. Prior run reports (218 s and 1620 s for lib tests, about 80 s for handover
cases, and 8–25 minutes cold builds) are historical observations, not controlled
baselines. This run explicitly assigns the full suite and gate to its parent
and prohibits child full-suite reruns, cache deletion, or private target dirs.
The parent must attach comparable base/candidate totals and its `cargo make
check` result to the actual PR description before claiming the full performance
acceptance criterion is met. The focused results here cannot substitute for
those totals.
