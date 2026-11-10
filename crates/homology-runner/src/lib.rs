//! The runner: a small program started on the computer whose files and
//! environments are worked on, as a child process here or over the user's
//! own SSH on a host, that speaks JSON lines on its stdin and stdout. It
//! opens no port, stays only as long as its session, and goes through no
//! service of anyone's.
//!
//! - [`protocol`]: the messages and their stable error codes.
//! - [`server`]: the runner's side, with the operations a program adds of
//!   its own ([`server::Runner`]).
//! - [`client`]: the caller's side, over the pipes of any child process.
//! - [`fs`]: a directory's files, for an editor: listed, read, written only
//!   over what was opened, searched.
//! - [`env`]: the Python environments on the runner's computer, found and
//!   probed without running the user's code.
//! - [`check`]: code checked in its environment, never run, with the rules
//!   of the program serving the runner; [`python`]: the checker's Python.
//! - [`sys`]: the few system helpers the rest needs.
//!
//! It depends on nothing but `serde_json`, `sha2` and `hex`: it is uploaded
//! to the user's hosts, so it stays small and static.

pub mod check;
pub mod client;
pub mod env;
pub mod fs;
pub mod protocol;
pub mod python;
pub mod server;
pub mod sys;

use std::path::Path;
use std::process::Command;

pub use client::{Client, Event};
pub use protocol::{RunnerError, RunnerResult, PROTOCOL, VERSION};

/// A JSON object's fields: a request's params.
pub type JsonMap = serde_json::Map<String, serde_json::Value>;

/// The command that serves a session with the runner at `program` on this
/// computer (no console window on Windows).
pub fn local_command(program: &Path) -> Command {
    let mut command = sys::command(program);
    command.arg("serve");
    command
}
