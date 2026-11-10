//! The caller's side of Homology: what a program needs to work through a
//! runner on this computer or on the user's own SSH hosts.
//!
//! - [`bundle`]: the runners the program carries for hosts (static Linux
//!   builds), and the one for this computer.
//! - [`deploy`]: a runner on an SSH host, placed once per version in the
//!   host's cache, checked by its hash, talked to over `ssh … serve`.
//! - [`Pool`]: one session per host, kept between requests.
//!
//! A program says which runner it deploys and where it keeps it with a
//! [`Program`]: its own (`annotarycat-runner` with its operations) or
//! Homology's plain `homology-runner`.

pub mod bundle;
pub mod deploy;
mod pool;

pub use pool::Pool;

use homology_runner::RunnerError;

/// A runner program as the caller deploys it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Program {
    /// Its file name: on a host, and with `-linux-<arch>` in the package.
    pub name: &'static str,
    /// The version a host keeps it under (`<cache>/bin/<version>/`).
    pub version: &'static str,
    /// Its folder in a host's cache (`$XDG_CACHE_HOME/<cache>`), e.g.
    /// `homology/runner`: everything the program leaves there.
    pub cache: &'static str,
    /// The prefix of the variables that point at other runners
    /// (`<ENV>_RUNNER_DIR`, `<ENV>_RUNNER`).
    pub env: &'static str,
}

impl Program {
    /// Homology's own runner, with the operations every runner has.
    pub const PLAIN: Program = Program {
        name: "homology-runner",
        version: homology_runner::VERSION,
        cache: "homology/runner",
        env: "HOMOLOGY",
    };
}

/// The host has no runner the program can place (not Linux x86_64 or aarch64).
pub const RUNNER_UNSUPPORTED_HOST: &str = "runner_unsupported_host";
/// The package carries no runner for the host (a build without them).
pub const RUNNER_MISSING: &str = "runner_missing";
/// The runner could not be placed on the host, or arrived damaged.
pub const RUNNER_UPLOAD_FAILED: &str = "runner_upload_failed";

/// A failure of the SSH layer, kept under its own stable code.
pub fn ssh_error(error: homology_ssh::SshError) -> RunnerError {
    RunnerError::new(error.code, error.message)
}
