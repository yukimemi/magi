//! Turn-scoped control transports. Only typed operations reach the launcher.
use super::Operation;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;

const MAX_FRAME: usize = 1024 * 1024;

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Request {
    id: u64,
    operation: Operation,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Response {
    id: u64,
    result: std::result::Result<String, String>,
}

/// Control capabilities belong only to the current foreground chat turn.
pub(crate) const ENVIRONMENT: &[&str] = &[
    "MAGI_CHAT_CONTROL_FD",
    "MAGI_CHAT_CONTROL_RESPONSE_FD",
    "MAGI_CHAT_CONTROL_LOCK_FD",
    "MAGI_CHAT_CONTROL_IDENTITIES",
    "MAGI_CHAT_CONTROL_SOCKET",
    "MAGI_CHAT_CONTROL_PIPE",
    "MAGI_CHAT_CONTROL_CAP",
];

pub(crate) fn clear_environment(command: &mut tokio::process::Command) {
    clear_std_environment(command.as_std_mut());
}

pub(crate) fn clear_std_environment(command: &mut std::process::Command) {
    #[cfg(unix)]
    unix::close_inherited(command);
    for name in ENVIRONMENT {
        command.env_remove(name);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn capability_comparison_checks_every_position_and_length() {
        let token = [b'a'; 64];
        assert!(super::constant_eq(&token, &token));
        assert!(!super::constant_eq(&token, &token[..63]));
        for index in 0..token.len() {
            let mut wrong = token;
            wrong[index] ^= 1;
            assert!(!super::constant_eq(&token, &wrong));
        }
    }
}
