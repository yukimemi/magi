//! `examples/smoke.rs` — release-time smoke target.
//!
//! `release.yml` runs `cargo run --release --target <T> --example smoke` on
//! every build matrix entry, to catch the class of bug where `cargo test`
//! passes but the produced binary dies on real startup.
//!
//! magi's regression-prone startup surface is not HTTPS — it never makes a
//! network call — it is **filesystem plus serialization**: resolving the run
//! home, writing `run.json` atomically (a `rename` across a directory that does
//! not exist yet, on three platforms), reading it back through `jiff`'s
//! timestamp deserializer, and parsing the starter config it tells the operator
//! to use. All four are per-platform behaviour that a Linux-only unit test run
//! would not prove for a Windows or macOS artifact.
use std::path::PathBuf;

use magi::config::Config;
use magi::run::RunState;

fn main() {
    let tmp = std::env::temp_dir().join(format!("magi-smoke-{}", std::process::id()));
    magi::run::set_home(tmp.clone());
    assert_eq!(magi::run::runs_root(), tmp.join("runs"));

    // The config `magi init` writes must load through the real path: teravars
    // renders it, so `toml::from_str` would both leave `{{ }}` unrendered and
    // trip `deny_unknown_fields` on `[vars]`.
    let starter = tmp.join("magi.toml");
    std::fs::create_dir_all(&tmp).expect("create smoke dir");
    std::fs::write(&starter, Config::starter_toml()).expect("write starter config");
    let cfg = Config::load(&starter).expect("starter config must load");
    assert_eq!(cfg.graph.candidates, 3);

    // Atomic save into a directory that does not exist yet, then load back
    // through jiff's timestamp deserializer.
    let mut state = RunState::new(
        PathBuf::from("."),
        "main".to_owned(),
        "0000000000000000000000000000000000000000".to_owned(),
        "smoke".to_owned(),
        cfg,
    );
    state.event("smoke", "release smoke target");
    state.save().expect("save run state");

    let back = RunState::load(&state.id).expect("load run state");
    assert_eq!(back.id, state.id);
    assert_eq!(back.instruction, "smoke");
    assert_eq!(back.events.len(), 1);
    assert!(!magi::run::list_ids().is_empty());

    // The report renderer touches every Display/format path the CLI prints.
    magi::report::set_color(false);
    let rendered = magi::report::run(&back);
    assert!(rendered.contains(&state.id));
    assert!(magi::report::stats(&magi::stats::collect(&[back])).contains("1 total"));

    std::fs::remove_dir_all(&tmp).ok();
    eprintln!("smoke: ok");
}
