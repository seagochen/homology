//! SSH through the system's own `ssh`, in batch mode, so credentials come
//! only from the user's `~/.ssh/config`, agent and keys: nothing here sees
//! or stores a password or key of theirs.
//!
//! - [`target`]: where on a host (`[user@]host:/path`, `ssh://…`), and
//!   quoting for its shell.
//! - [`ssh`]: running a script, or keeping a session's stdin and stdout
//!   (a runner's `serve`), with failures under stable codes.
//! - [`host_key`]: a host `ssh` does not know yet is not accepted blindly:
//!   the key it presents is shown so the user can compare the fingerprint,
//!   and recorded in their known hosts only once they confirm; a key that
//!   changed is replaced the same way, never silently.
//! - [`askpass`], [`keys`]: a host that takes only a password is the one
//!   exception to "never sees a password": the user types it once, askpass
//!   hands it to `ssh` for one run, and a login key of the program's own is
//!   installed with it; the password is then dropped and never stored.
//!
//! - [`servers`]: the servers the user saved, shared by every program on
//!   Homology with the login keys set up for them.
//!
//! What a session sets (the SSH configuration a sandboxed program uses,
//! where its login keys live) is in a [`Context`] the caller owns and
//! passes to each call, with the [`App`] whose names it uses; nothing here
//! is process-wide.

pub mod askpass;
pub mod browse;
mod context;
pub mod host_key;
pub mod keys;
mod logins;
pub mod sandbox;
pub mod servers;
pub mod ssh;
mod target;

pub use context::{App, Context};
pub use ssh::{run, run_with_input, run_with_password, session_command};
pub use target::{quoted_dir, shell_quote, SshTarget};

use std::fmt;

/// A failure talking to a host, with a stable `code`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshError {
    pub code: &'static str,
    pub message: String,
    /// The exit status of the script on the host, when that is what failed
    /// (`remote_failed`): the caller's scripts may give some a meaning.
    pub status: Option<i32>,
}

impl SshError {
    /// A failure with a stable `code` and a message for people.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), status: None }
    }
}

impl fmt::Display for SshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SshError {}

/// What talking to a host returns.
pub type SshResult<T> = Result<T, SshError>;

/// Every code an [`SshError`] carries: stable, for programs to tell
/// failures apart.
pub const CODES: &[&str] = &[
    "invalid_remote",
    "ssh_missing",
    "ssh_failed",
    "ssh_unreachable",
    "ssh_auth",
    "ssh_host_key",
    "ssh_host_unknown",
    "ssh_host_key_mismatch",
    "ssh_host_scan_failed",
    "known_hosts_write_failed",
    "ssh_password_required",
    "ssh_password_wrong",
    "ssh_askpass_failed",
    "ssh_key_rejected",
    "ssh_key_not_found",
    "ssh_keys_unavailable",
    "ssh_keys_failed",
    "remote_failed",
    "remote_directory_missing",
    "remote_directory_denied",
    "invalid_server",
    "server_not_found",
    "server_name_exists",
    "server_changed",
    "server_ambiguous",
    "server_store_read_failed",
    "server_store_write_failed",
    "unsupported_server_schema",
    "sandbox_ssh_failed",
];
