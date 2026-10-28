//! Isolation settings for real CLI integration subprocesses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub(super) const SEAT_ENV: [&str; 3] = ["MAGI_RUN", "MAGI_NODE", "MAGI_SEAT"];
// These resets apply to spawned CLIs. Library-embedding test hosts retain their
// explicit MAGI_WORKER_EXE when copying child isolation settings back to a host.
pub(super) const WORKER_ENV: [&str; 4] = magi::worker::INVOCATION_ENV;

/// Cargo's unqualified `debug/magi` can be relinked by another toolchain using
/// the shared target directory while these tests run. Use the hashed artifact
/// linked against this test's library instead, including for worker launches.
pub fn magi_executable() -> &'static Path {
    static EXE: LazyLock<PathBuf> = LazyLock::new(|| {
        let test = std::env::current_exe().expect("test executable");
        matching_cli(&test).unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_magi")))
    });
    &EXE
}

/// Nextest archives may omit Cargo fingerprints; in that case the caller uses
/// Cargo's supplied executable. A local build with fingerprints must find an
/// exact library match, rather than silently run a different schema version.
pub(super) fn matching_cli(test: &Path) -> Option<PathBuf> {
    let (name, hash) = test.file_stem()?.to_str()?.rsplit_once('-')?;
    let profile = test.parent()?.parent()?;
    let fingerprints = profile.join(".fingerprint");
    let test_fingerprint = fingerprints
        .join(format!("{}-{hash}", env!("CARGO_PKG_NAME")))
        .join(format!("test-integration-test-{name}.json"));
    let bytes = std::fs::read(&test_fingerprint).ok()?;
    let test: serde_json::Value = serde_json::from_slice(&bytes).expect("test build fingerprint");
    let library = |value: &serde_json::Value| {
        value["deps"]
            .as_array()
            .and_then(|deps| deps.iter().find(|dep| dep[1] == "magi").cloned())
    };
    let dependency = library(&test).expect("test links magi library");
    for entry in std::fs::read_dir(&fingerprints).expect("Cargo fingerprints") {
        let entry = entry.expect("Cargo fingerprint entry");
        let Ok(bytes) = std::fs::read(entry.path().join("bin-magi.json")) else {
            continue;
        };
        let Ok(binary) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if library(&binary).as_ref() != Some(&dependency) || binary["rustc"] != test["rustc"] {
            continue;
        }
        let directory = entry.file_name();
        let Some(hash) = directory
            .to_str()
            .and_then(|name| name.rsplit_once('-').map(|(_, hash)| hash))
        else {
            continue;
        };
        let executable = profile
            .join("deps")
            .join(format!("magi-{hash}{}", std::env::consts::EXE_SUFFIX));
        if executable.is_file() {
            return Some(executable);
        }
    }
    panic!("no CLI artifact links this test's magi library; rebuild the magi binary");
}

/// Start the real CLI in a caller-owned temporary home. Keep that directory
/// alive until the child exits. Callers may override the cwd and environment.
pub fn magi_cli(home: &Path) -> std::process::Command {
    let host_home = home.join("host-home");
    std::fs::create_dir_all(&host_home).expect("create isolated CLI home");
    let mut command = std::process::Command::new(magi_executable());
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
    // A fresh CLI belongs to this fixture, not the web server that launched
    // the test. In particular, a retained restart port overrides `--port 0`
    // and makes independent web tests contend for the operator's listener.
    // Tests of an actual successor may explicitly set these after this call.
    command
        .env_remove("MAGI_WEB_RESTART_PORT")
        .env_remove("MAGI_WEB_RESUME_LOOP");
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
