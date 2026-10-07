//! `--help` is coloured by the Evangelion theme, and every way of asking for
//! plain text still gets plain text.

use std::process::Command;

fn help(args: &[&str], envs: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_magi"));
    cmd.args(args)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CLICOLOR");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run magi");
    assert!(out.status.success(), "{args:?} failed");
    String::from_utf8(out.stdout).expect("utf-8 help")
}

#[test]
fn help_is_styled_only_when_colour_is_wanted() {
    assert!(help(&["--help"], &[("CLICOLOR_FORCE", "1")]).contains("\x1b["));

    assert!(!help(&["--help"], &[]).contains('\x1b'));
    let forced = [("CLICOLOR_FORCE", "1")];
    assert!(!help(&["--no-color", "--help"], &forced).contains('\x1b'));
    assert!(!help(&["run", "--no-color", "--help"], &forced).contains('\x1b'));
    assert!(!help(&["--help"], &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")]).contains('\x1b'));
}
