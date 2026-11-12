//! A sandboxed macOS program's SSH (#19): it cannot read the user's
//! `~/.ssh`, so `ssh` runs with a configuration of the programs' own, in the
//! shared folder ([`crate::servers::shared_dir_in`]): their own known
//! hosts (a host trusted in one program is trusted in the others of its
//! App Group), no agent and no key files of the user's (the programs' own
//! login keys are added with `-i`). Moved from AnnotaryCat
//! (`settings::prepare_sandbox_ssh`), for every program built on Homology.

use std::io::Write;
use std::path::{Path, PathBuf};

use homology_sys as sys;

use crate::{SshError, SshResult};

/// Whether this is a sandboxed macOS program: macOS sets
/// `APP_SANDBOX_CONTAINER_ID` in one.
pub fn active() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some()
}

/// The folder of the programs' SSH configuration in the shared folder `shared`.
fn folder(shared: &Path) -> PathBuf {
    shared.join("ssh-config")
}

/// The programs' own known hosts in the shared folder `shared`.
pub fn known_hosts(shared: &Path) -> PathBuf {
    folder(shared).join("known_hosts")
}

/// The programs' SSH configuration in the shared folder `shared`, made the
/// first time (in a folder only the user can read), for
/// [`Context::set_ssh_config`](crate::Context::set_ssh_config).
/// `sandbox_ssh_failed` when it cannot be made, or its path cannot be
/// written in an `ssh_config`.
pub fn ssh_config(shared: &Path) -> SshResult<PathBuf> {
    let folder = folder(shared);
    let config = folder.join("config");
    let failed = |error: std::io::Error| SshError::new("sandbox_ssh_failed", format!("{}: {error}", config.display()));
    sys::private_dir(&folder).map_err(failed)?;
    // Other programs of the group make it too.
    let _lock = sys::LockFile::take(&config).map_err(failed)?;
    if !config.exists() {
        sys::atomic_write_bytes(&config, text(&known_hosts(shared))?.as_bytes()).map_err(failed)?;
    }
    Ok(config)
}

/// The configuration naming `known_hosts`: no agent, no key files, no
/// global known hosts. A path `ssh_config` cannot quote is refused.
fn text(known_hosts: &Path) -> SshResult<String> {
    let refused = || SshError::new("sandbox_ssh_failed", "the SSH configuration's path cannot be written in it");
    let file = known_hosts.to_str().ok_or_else(refused)?;
    if file.contains(['\n', '\r', '\0', '%']) {
        return Err(refused());
    }
    let file = file.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!(
        "# The sandboxed programs' own SSH, apart from the terminal's (Homology).\nHost *\n    UserKnownHostsFile \"{file}\"\n    GlobalKnownHostsFile /dev/null\n    IdentityAgent none\n    IdentityFile none\n    IdentitiesOnly yes\n"
    ))
}

/// Add the hosts of the known-hosts file `from` (a program's own, from
/// before the shared one) that `to` lacks, line for line, under `to`'s
/// lock; how many. `from` is left as it was, so adding again adds nothing.
pub fn adopt_known_hosts(from: &Path, to: &Path) -> SshResult<usize> {
    let Ok(earlier) = std::fs::read_to_string(from) else { return Ok(0) };
    let failed =
        |error: std::io::Error| SshError::new("known_hosts_write_failed", format!("{}: {error}", to.display()));
    if let Some(parent) = to.parent() {
        sys::private_dir(parent).map_err(failed)?;
    }
    let _lock = sys::LockFile::take(to).map_err(failed)?;
    let now = std::fs::read_to_string(to).unwrap_or_default();
    let known: std::collections::HashSet<&str> = now.lines().map(str::trim).collect();
    let new: Vec<&str> = earlier
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && !known.contains(line))
        .collect();
    if new.is_empty() {
        return Ok(0);
    }
    let mut text = String::new();
    if !now.is_empty() && !now.ends_with('\n') {
        text.push('\n');
    }
    for line in &new {
        text.push_str(line);
        text.push('\n');
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(to).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.sync_all().map_err(failed)?;
    Ok(new.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_programs_ssh_configuration_is_made_once_with_their_known_hosts() {
        let shared = tempfile::tempdir().unwrap();
        let config = ssh_config(shared.path()).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        let hosts = known_hosts(shared.path());
        assert!(text.contains(&format!("UserKnownHostsFile \"{}\"", hosts.display())), "{text}");
        assert!(text.contains("IdentityAgent none") && text.contains("IdentityFile none"));
        // Changed by the user: kept, not made again.
        std::fs::write(&config, "Host gpu\n    ProxyJump bastion\n").unwrap();
        assert_eq!(ssh_config(shared.path()).unwrap(), config);
        assert!(std::fs::read_to_string(&config).unwrap().contains("ProxyJump"));
        assert!(text_refuses("/a\nb"));
    }

    /// Whether a configuration naming `path` is refused.
    fn text_refuses(path: &str) -> bool {
        text(Path::new(path)).is_err()
    }

    #[test]
    fn earlier_known_hosts_are_added_once() {
        let folder = tempfile::tempdir().unwrap();
        let (from, to) = (folder.path().join("old"), folder.path().join("shared").join("known_hosts"));
        std::fs::write(&from, "# comment\ngpu ssh-ed25519 AAAA1\n\nlab ssh-ed25519 AAAA2\n").unwrap();
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::write(&to, "lab ssh-ed25519 AAAA2").unwrap();
        assert_eq!(adopt_known_hosts(&from, &to).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "lab ssh-ed25519 AAAA2\ngpu ssh-ed25519 AAAA1\n");
        assert_eq!(adopt_known_hosts(&from, &to).unwrap(), 0, "added again: nothing");
        assert_eq!(adopt_known_hosts(&folder.path().join("none"), &to).unwrap(), 0);
        assert!(from.is_file());
    }
}
