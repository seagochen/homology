//! The hosts the program logs in to with a login key of its own, as
//! `hosts.json` in the keys directory records them ([`crate::keys`] sets
//! them up). Reading the record needs no `ssh`, so [`crate::ssh`] finds the
//! key to add (`-i`) here without depending on the module that installs
//! keys over `ssh`.

use std::path::{Path, PathBuf};

use homology_sys as sys;
use serde_json::{json, Value};

use crate::target::SshTarget;
use crate::{Context, SshError, SshResult};

/// The file in the keys directory that says which host uses which key.
pub(crate) const HOSTS: &str = "hosts.json";

/// The keys directory; `ssh_keys_unavailable` when the session set none.
pub(crate) fn directory(cx: &Context) -> SshResult<PathBuf> {
    cx.keys_dir()
        .ok_or_else(|| SshError::new("ssh_keys_unavailable", "no configuration directory to keep login keys in"))
}

/// One host the program logs in to with its own key.
#[derive(Clone, Debug, PartialEq)]
pub struct HostKeyLogin {
    /// `user@host` as the target names it, and its port.
    pub host: String,
    pub port: Option<u16>,
    /// The key file's name in the keys directory.
    pub key: String,
    /// The public key's comment, `<name>@<this computer>`.
    pub comment: String,
    pub fingerprint: String,
    pub created_at: String,
}

impl HostKeyLogin {
    /// The login as commands report it (without the key file's name).
    pub fn to_json(&self) -> Value {
        json!({"host": self.host, "port": self.port, "comment": self.comment, "fingerprint": self.fingerprint, "created_at": self.created_at})
    }

    /// A login as `hosts.json` holds it; `None` when it lacks the host or key.
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            host: value.get("host")?.as_str()?.to_string(),
            port: value.get("port").and_then(Value::as_u64).and_then(|port| u16::try_from(port).ok()),
            key: value.get("key")?.as_str()?.to_string(),
            comment: value.get("comment").and_then(Value::as_str).unwrap_or_default().to_string(),
            fingerprint: value.get("fingerprint").and_then(Value::as_str).unwrap_or_default().to_string(),
            created_at: value.get("created_at").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
    }

    /// The login as `hosts.json` holds it: with the key file's name.
    pub(crate) fn stored(&self) -> Value {
        let mut value = self.to_json();
        value["key"] = json!(self.key);
        value
    }

    /// Whether this login is for `target`'s host and port.
    pub(crate) fn matches(&self, target: &SshTarget) -> bool {
        self.host == target.host && self.port == target.port
    }
}

/// The logins in `hosts.json`; none when it is missing or unreadable.
pub(crate) fn read_hosts(directory: &Path) -> Vec<HostKeyLogin> {
    let Ok(text) = std::fs::read_to_string(directory.join(HOSTS)) else { return Vec::new() };
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    value
        .get("hosts")
        .and_then(Value::as_array)
        .map(|hosts| hosts.iter().filter_map(HostKeyLogin::from_json).collect())
        .unwrap_or_default()
}

/// Write `hosts.json` atomically (`ssh_keys_failed` when it cannot be).
pub(crate) fn write_hosts(directory: &Path, hosts: &[HostKeyLogin]) -> SshResult<()> {
    let failed = |error: std::io::Error| {
        SshError::new("ssh_keys_failed", format!("{}: {error}", directory.join(HOSTS).display()))
    };
    let text =
        serde_json::to_string_pretty(&json!({"hosts": hosts.iter().map(HostKeyLogin::stored).collect::<Vec<_>>()}))
            .unwrap_or_default();
    sys::atomic_write_bytes(&directory.join(HOSTS), (text + "\n").as_bytes()).map_err(failed)
}

/// The key to add for `target`, when the program set one up for it.
pub(crate) fn identity(cx: &Context, target: &SshTarget) -> Option<PathBuf> {
    let directory = directory(cx).ok()?;
    let login = read_hosts(&directory).into_iter().find(|login| login.matches(target))?;
    Some(directory.join(login.key)).filter(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_round_trip_and_match_by_host_and_port() {
        let directory = tempfile::tempdir().unwrap();
        let login = HostKeyLogin {
            host: "me@gpu".into(),
            port: Some(2222),
            key: "id_ed25519_me_gpu_1".into(),
            comment: "tool@pc".into(),
            fingerprint: "SHA256:x".into(),
            created_at: "2026-10-16T00:00:00Z".into(),
        };
        write_hosts(directory.path(), std::slice::from_ref(&login)).unwrap();
        assert_eq!(read_hosts(directory.path()), std::slice::from_ref(&login));
        assert!(login.matches(&SshTarget::parse("ssh://me@gpu:2222/data").unwrap()));
        assert!(!login.matches(&SshTarget::parse("me@gpu:/data").unwrap()));
        assert!(login.to_json().get("key").is_none(), "the key file is not reported");
    }

    /// The program's host list is not written through a link left at the
    /// old temporary name (#3).
    #[cfg(unix)]
    #[test]
    fn the_host_list_is_not_written_through_a_link() {
        let base = tempfile::tempdir().unwrap();
        let (directory, outside) = (base.path().join("keys"), base.path().join("outside.json"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&outside, "ORIGINAL").unwrap();
        std::os::unix::fs::symlink(&outside, directory.join(format!("{HOSTS}.tmp"))).unwrap();
        let dangling = base.path().join("made.json");
        std::os::unix::fs::symlink(&dangling, directory.join(format!(".{HOSTS}.tmp"))).unwrap();

        write_hosts(&directory, &[]).unwrap();

        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "ORIGINAL");
        assert!(!dangling.exists());
        assert!(std::fs::symlink_metadata(directory.join(HOSTS)).unwrap().file_type().is_file());
        assert!(read_hosts(&directory).is_empty());
    }
}
