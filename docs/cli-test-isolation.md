# CLI integration-test isolation

Use `common::magi_cli(home)` to spawn the real binary. The caller keeps its
fixture directory alive until the child and its independent workers exit.
The helper creates a private host home and cwd, sets `MAGI_HOME`,
`MAGI_CONFIG_DIR`, `MAGI_NO_AUTOUPDATE=1`, `HOME` and `USERPROFILE`, and removes
inherited seat and worker identities (`MAGI_RUN`, `MAGI_NODE`, `MAGI_SEAT`,
`MAGI_WORKER_EXE`, `MAGI_WORKER_ID`, `MAGI_WORKER_GENERATION`,
`MAGI_WORKER_HOME`). Tests may explicitly override
these defaults afterward.

Tests embedding the library's scheduler use `MAGI_WORKER_EXE` to name
`CARGO_BIN_EXE_magi`; a test harness executable is not a worker CLI.
`common::isolated` sets this before its runtime starts. Native restart fixtures
kill only the web/scheduler PID, not its descendant tree: killing a process tree
would deliberately kill the workers whose independence the test must measure.

CLI environment resets belong to the spawned command, not to its embedding test
process. A fixture that copies those settings into its own environment must
retain its explicit `MAGI_WORKER_EXE`; otherwise later library dispatch tries
to launch the libtest harness as a worker. Keep the host's worker-binary override
separate from the child CLI's cleared worker identity.

Agent and shell adapters copy the same defaults while preserving the fresh
seat identity assigned by the launcher and its explicit fixture cwd. The
`update_exit` test deliberately re-enables updates with its existing fake `gh`,
private HOME/cache/configuration, token removal, and process-group cleanup.
