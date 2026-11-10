//! What one session tells this crate: the program whose names it uses
//! ([`App`]), the SSH configuration a sandboxed program uses, and where
//! its login keys are. The caller owns one and hands it to every call;
//! nothing here is a static.

use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};

/// The program using this crate, by the names it shows on this computer
/// and on hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct App {
    /// Its name in lower case (`homology`): the askpass helper beside it
    /// (`<name>-askpass`), its login keys' comment (`<name>@<computer>`),
    /// its temporary files.
    pub name: &'static str,
    /// The prefix of the environment variables it reads (`HOMOLOGY`):
    /// `<ENV>_SSH`, `<ENV>_SSH_CONFIG`, `<ENV>_SSH_KEYGEN`, `<ENV>_ASKPASS`,
    /// `<ENV>_ASKPASS_PROGRAM`.
    pub env: &'static str,
}

impl App {
    /// Homology's own names, for a program that has none of its own.
    pub const HOMOLOGY: App = App { name: "homology", env: "HOMOLOGY" };

    /// The environment variable `<ENV>_<suffix>`.
    pub fn var(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.env)
    }
}

impl Default for App {
    fn default() -> Self {
        App::HOMOLOGY
    }
}

/// One session's SSH settings (see the module docs).
#[derive(Debug, Default)]
pub struct Context {
    app: App,
    /// An `ssh_config` used instead of `~/.ssh/config` (`ssh -F`): a
    /// sandboxed program's own; `<ENV>_SSH_CONFIG` wins over it.
    ssh_config: RwLock<Option<PathBuf>>,
    /// Where the login keys the program sets up live ([`crate::keys`]);
    /// without it nothing is remembered.
    keys_dir: RwLock<Option<PathBuf>>,
}

impl Context {
    /// A session of `app`'s, with the user's own configuration and no keys
    /// directory yet.
    pub fn new(app: App) -> Self {
        Context { app, ..Context::default() }
    }

    /// The program whose names the session uses.
    pub fn app(&self) -> App {
        self.app
    }

    /// Use an SSH configuration of the program's own (a sandboxed app's
    /// container), prepared before remote operations are enabled.
    pub fn set_ssh_config(&self, path: PathBuf) {
        *self.ssh_config.write().unwrap_or_else(PoisonError::into_inner) = Some(path);
    }

    /// Keep generated keys and the host list in `directory`.
    pub fn set_keys_dir(&self, directory: PathBuf) {
        *self.keys_dir.write().unwrap_or_else(PoisonError::into_inner) = Some(directory);
    }

    /// The `ssh_config` file `ssh` reads instead of the user's, if any.
    pub fn ssh_config(&self) -> Option<PathBuf> {
        std::env::var_os(self.app.var("SSH_CONFIG"))
            .map(PathBuf::from)
            .or_else(|| self.ssh_config.read().unwrap_or_else(PoisonError::into_inner).clone())
    }

    /// Where the login keys the program set up live, if the session said.
    pub fn keys_dir(&self) -> Option<PathBuf> {
        self.keys_dir.read().unwrap_or_else(PoisonError::into_inner).clone()
    }
}
