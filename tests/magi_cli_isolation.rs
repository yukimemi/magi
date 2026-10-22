//! Keep every real CLI invocation behind the isolated test helper.

mod common;

use std::path::PathBuf;

#[test]
fn real_cli_spawns_use_the_shared_helper() {
    // Prefer the runtime path, as the shared build cache may serve another worktree.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let tests = manifest.join("tests");
    let helper = tests.join("common/cli.rs");
    let needle = ["CARGO_BIN_EXE_", "magi"].concat();
    let mut dirs = vec![tests];
    let mut references = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                for (index, line) in source.lines().enumerate() {
                    if line.contains(&needle) {
                        references.push((path.clone(), index + 1));
                    }
                }
            }
        }
    }
    let source = std::fs::read_to_string(&helper).unwrap();
    let start = source.find("pub fn magi_cli(").unwrap();
    let end = start + source[start..].find("\n}").unwrap();
    let helper_line = source[..start].lines().count() + 1;
    let relative_line = source[start..end]
        .lines()
        .position(|line| line.contains(&needle))
        .expect("the helper constructs the real binary command");
    assert_eq!(references, vec![(helper, helper_line + relative_line)]);
}

#[test]
fn cli_defaults_isolate_state_cwd_and_seat_identity_and_allow_overrides() {
    let home = tempfile::tempdir().unwrap();
    let mut command = common::magi_cli(home.path());
    assert_eq!(command.get_current_dir(), Some(home.path()));
    let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
    for (key, value) in [
        ("MAGI_HOME", home.path().as_os_str().to_owned()),
        (
            "MAGI_CONFIG_DIR",
            home.path().join("config").into_os_string(),
        ),
        ("MAGI_NO_AUTOUPDATE", "1".into()),
    ] {
        assert_eq!(env[std::ffi::OsStr::new(key)], Some(value.as_os_str()));
    }
    for key in ["MAGI_RUN", "MAGI_NODE", "MAGI_SEAT", "MAGI_TURN_SCOPE"] {
        assert_eq!(env[std::ffi::OsStr::new(key)], None);
    }
    let cwd = home.path().join("repo");
    command.current_dir(&cwd).env("MAGI_RUN", "fixture-run");
    assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == "MAGI_RUN")
            .unwrap()
            .1,
        Some(std::ffi::OsStr::new("fixture-run"))
    );
}

#[test]
fn outer_commands_receive_cli_cwd_environment_and_removals() {
    let home = tempfile::tempdir().unwrap();
    let cli = common::magi_cli(home.path());
    let mut outer = std::process::Command::new("unused-shell");
    outer
        .current_dir("old-cwd")
        .env("MAGI_RUN", "inherited-run");

    common::magi_cli_environment(&cli, &mut outer);

    assert_eq!(outer.get_current_dir(), cli.get_current_dir());
    assert_eq!(
        outer.get_envs().collect::<Vec<_>>(),
        cli.get_envs().collect::<Vec<_>>()
    );

    let cwd = home.path().join("repo");
    outer.current_dir(&cwd).env("MAGI_RUN", "fixture-run");
    assert_eq!(outer.get_current_dir(), Some(cwd.as_path()));
    assert_eq!(
        outer
            .get_envs()
            .find(|(key, _)| *key == "MAGI_RUN")
            .unwrap()
            .1,
        Some(std::ffi::OsStr::new("fixture-run"))
    );
}
