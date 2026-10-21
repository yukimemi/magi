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
