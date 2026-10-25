//! Isolation settings for real CLI integration subprocesses.

use std::collections::BTreeMap;
use std::path::Path;

pub(super) const SEAT_ENV: [&str; 4] = ["MAGI_RUN", "MAGI_NODE", "MAGI_SEAT", "MAGI_TURN_SCOPE"];
// These resets apply to spawned CLIs. Library-embedding test hosts retain their
// explicit MAGI_WORKER_EXE when copying child isolation settings back to a host.
pub(super) const WORKER_ENV: [&str; 4] = magi::worker::INVOCATION_ENV;

/// Start the real CLI in a caller-owned temporary home. Keep that directory
/// alive until the child exits. Callers may override the cwd and environment.
pub fn magi_cli(home: &Path) -> std::process::Command {
    let host_home = home.join("host-home");
    std::fs::create_dir_all(&host_home).expect("create isolated CLI home");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_magi"));
    command
        .current_dir(home)
        .env("MAGI_HOME", home)
        .env("MAGI_CONFIG_DIR", home.join("config"))
        .env("HOME", &host_home)
        .env("USERPROFILE", &host_home)
        .env("MAGI_NO_AUTOUPDATE", "1");
    for key in SEAT_ENV.into_iter().chain(WORKER_ENV) {
        command.env_remove(key);
    }
    command
}

/// Pass the CLI's isolation settings through an agent launcher or shell mock.
/// The launcher supplies the fixture cwd and fresh seat metadata. Copy only
/// positive defaults here so nested calls retain the identity it just assigned.
pub fn magi_cli_env(home: &Path, binary_key: &str) -> BTreeMap<String, String> {
    let command = magi_cli(home);
    let mut env: BTreeMap<_, _> = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| (key.to_str().unwrap().into(), value.to_str().unwrap().into()))
        })
        .collect();
    env.insert(
        binary_key.into(),
        command.get_program().to_str().unwrap().into(),
    );
    env
}

/// Copy isolation defaults to an outer shell or sandbox command. Apply any
/// test-specific cwd and environment overrides after this call.
pub fn magi_cli_environment(cli: &std::process::Command, outer: &mut std::process::Command) {
    outer.current_dir(cli.get_current_dir().expect("isolated CLI cwd"));
    for (key, value) in cli.get_envs() {
        match value {
            Some(value) => {
                outer.env(key, value);
            }
            None => {
                outer.env_remove(key);
            }
        }
    }
}
