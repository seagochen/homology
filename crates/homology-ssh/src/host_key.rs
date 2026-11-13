//! Host keys: telling a host `ssh` has never seen from one whose key
//! changed, showing the key the host presents, and recording it in the
//! user's known hosts once they have compared its fingerprint.
//!
//! The key is read the way `ssh` itself would meet it: `ssh` connects with
//! the user's configuration (`ProxyJump`, `HostKeyAlias`, `Port`, …) and a
//! scratch `UserKnownHostsFile`, accepting the new key into that scratch
//! file only. What it wrote there, already in the user's format (hashed when
//! `HashKnownHosts` is on, `[host]:port` for another port), is what gets
//! appended to the real file, so the key the user confirmed is the key
//! recorded. Trusting scans again and refuses when the fingerprint differs
//! from the one confirmed, and never adds a key for a host the known hosts
//! already name. A changed key (the host set up again) is replaced only by
//! [`replace`], once the user compared the new fingerprint: the earlier
//! keys are removed and the new one recorded, or nothing changes.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use homology_sys as sys;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::ssh::{classify, config_args, not_started, ssh_program};
use crate::target::SshTarget;
use crate::{Context, SshError, SshResult};

/// The port `ssh` uses when none is given.
const DEFAULT_PORT: u16 = 22;

/// One host key: its type (`ssh-ed25519`, …) and SHA256 fingerprint as
/// `ssh` prints it (`SHA256:` and unpadded base64).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKey {
    pub key_type: String,
    pub fingerprint: String,
}

/// Whether the known hosts trust the key a host presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The known hosts hold the key the host presents.
    Known,
    /// The known hosts do not name the host.
    Unknown,
    /// The known hosts name the host with other keys.
    Changed,
}

impl HostKeyStatus {
    /// The status as commands report it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Known => "known",
            Self::Unknown => "unknown",
            Self::Changed => "changed",
        }
    }
}

/// What the known hosts say about a host, and what it presents now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyReport {
    /// The host as the known hosts name it: `host`, `[host]:port` or the
    /// `HostKeyAlias`.
    pub name: String,
    /// The host `ssh` connects to (after `~/.ssh/config`) and its port.
    pub hostname: String,
    pub port: u16,
    pub status: HostKeyStatus,
    /// The key the host presented.
    pub presented: HostKey,
    /// The keys the known hosts hold for it.
    pub recorded: Vec<HostKey>,
    /// Where a trusted key is written (the first `UserKnownHostsFile`).
    pub known_hosts: PathBuf,
    /// Whether the user's own `ssh` reads the same configuration, so a key
    /// trusted here is trusted in the terminal too: not with a
    /// configuration of the program's own (`<ENV>_SSH_CONFIG`), and not in
    /// a macOS sandbox, whose `ssh` sees the container's `~/.ssh` only.
    pub shared_with_terminal: bool,
}

impl HostKeyReport {
    /// The command that removes the recorded keys, for a changed key.
    pub fn remove_command(&self) -> String {
        let name = if self.name.contains(['[', ' ']) { format!("\"{}\"", self.name) } else { self.name.clone() };
        format!("ssh-keygen -R {name} -f \"{}\"", self.known_hosts.display())
    }

    /// The report as a command answers it.
    pub fn to_json(&self) -> Value {
        let key = |key: &HostKey| json!({"key_type": key.key_type, "fingerprint": key.fingerprint});
        json!({
            "status": self.status.as_str(),
            "name": self.name,
            "hostname": self.hostname,
            "port": self.port,
            "key_type": self.presented.key_type,
            "fingerprint": self.presented.fingerprint,
            "recorded": self.recorded.iter().map(key).collect::<Vec<_>>(),
            "known_hosts": self.known_hosts.to_string_lossy(),
            "shared_with_terminal": self.shared_with_terminal,
            "remove_command": self.remove_command(),
        })
    }
}

/// Compare the key `target` presents with the user's known hosts.
pub fn check(cx: &Context, target: &SshTarget) -> SshResult<HostKeyReport> {
    Ok(inspect(cx, target)?.report)
}

/// Record the key `target` presents in the user's known hosts, provided its
/// fingerprint is `fingerprint` (the one the user was shown) and the known
/// hosts do not name the host yet. Returns the report after writing.
pub fn trust(cx: &Context, target: &SshTarget, fingerprint: &str) -> SshResult<HostKeyReport> {
    let Inspected { report, lines, .. } = inspect(cx, target)?;
    match report.status {
        HostKeyStatus::Known => return Ok(report),
        HostKeyStatus::Changed => return Err(changed(&report)),
        HostKeyStatus::Unknown => {}
    }
    confirmed(&report, fingerprint)?;
    recorded(cx, report, &lines)
}

/// `ssh_host_key`: the known hosts hold another key for `report`'s host.
fn changed(report: &HostKeyReport) -> SshError {
    SshError::new(
        "ssh_host_key",
        format!(
            "the known hosts already hold another key for {}; if the change is expected, remove it with `{}`",
            report.name,
            report.remove_command()
        ),
    )
}

/// `lines`, the key `report`'s host presents, recorded in its known hosts
/// under their lock, looked at again under it (#18): when another program
/// recorded the host meanwhile, the same key is not recorded twice and
/// another is the change it is (`ssh_host_key`).
fn recorded(cx: &Context, report: HostKeyReport, lines: &[String]) -> SshResult<HostKeyReport> {
    let _lock = known_hosts_lock(&report.known_hosts)?;
    let now = match report.known_hosts.is_file() {
        true => lookup(cx, &report.known_hosts, &report.name)?,
        false => Vec::new(),
    };
    if now.is_empty() {
        append_lines(&report.known_hosts, lines)?;
    } else if !now.iter().any(|key| key.fingerprint == report.presented.fingerprint) {
        return Err(changed(&report));
    }
    Ok(HostKeyReport { status: HostKeyStatus::Known, recorded: vec![report.presented.clone()], ..report })
}

/// The lock of the known-hosts file `file`, which other programs (and
/// this one's other threads) write too; its folder made private first
/// when it is not there.
fn known_hosts_lock(file: &Path) -> SshResult<sys::LockFile> {
    let failed =
        |error: std::io::Error| SshError::new("known_hosts_write_failed", format!("{}: {error}", file.display()));
    if let Some(parent) = file.parent().filter(|parent| !parent.as_os_str().is_empty() && !parent.exists()) {
        sys::private_dir(parent).map_err(failed)?;
    }
    sys::LockFile::take(file).map_err(failed)
}

/// Replace the keys the known hosts hold for `target`, whose key changed
/// (the host was set up again), by the key it presents now, provided its
/// fingerprint is `fingerprint`, the one the user compared (#17). Nothing
/// is changed for another key. Returns the report after writing.
pub fn replace(cx: &Context, target: &SshTarget, fingerprint: &str) -> SshResult<HostKeyReport> {
    let Inspected { report, lines, files } = inspect(cx, target)?;
    confirmed(&report, fingerprint)?;
    if report.status == HostKeyStatus::Known {
        return Ok(report);
    }
    replaced(cx, &report, &lines, &files)
}

/// `ssh_host_key_mismatch` unless the key presented is the one the user
/// was shown, `fingerprint`.
fn confirmed(report: &HostKeyReport, fingerprint: &str) -> SshResult<()> {
    if report.presented.fingerprint == fingerprint {
        return Ok(());
    }
    Err(SshError::new(
        "ssh_host_key_mismatch",
        format!(
            "{} now presents {}, not the key that was confirmed; check the new fingerprint",
            report.name, report.presented.fingerprint
        ),
    ))
}

/// The keys of `report`'s host removed from each of `files` (with
/// `ssh-keygen -R`, which knows hashed names), then `lines`, the presented
/// key, recorded in its known hosts; under that file's lock, so that two
/// programs replacing it at once leave it one record (#18).
fn replaced(cx: &Context, report: &HostKeyReport, lines: &[String], files: &[PathBuf]) -> SshResult<HostKeyReport> {
    let _lock = known_hosts_lock(&report.known_hosts)?;
    let program = keygen_program(cx);
    for file in files {
        let output = sys::command(&program)
            .arg("-R")
            .arg(&report.name)
            .arg("-f")
            .arg(file)
            .output()
            .map_err(|error| not_started(&program, None, &error))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(SshError::new("known_hosts_write_failed", format!("{}: {said}", file.display())));
        }
    }
    append_lines(&report.known_hosts, lines)?;
    Ok(HostKeyReport { status: HostKeyStatus::Known, recorded: vec![report.presented.clone()], ..report.clone() })
}

/// What [`inspect`] found: the report, the known-hosts lines `ssh` wrote
/// for the presented key, and the known-hosts files that name the host.
struct Inspected {
    report: HostKeyReport,
    lines: Vec<String>,
    files: Vec<PathBuf>,
}

/// The report of `target`'s key against the user's known hosts.
fn inspect(cx: &Context, target: &SshTarget) -> SshResult<Inspected> {
    let config = Config::parse(&output_text(ssh_command(cx, target, &["-G"]), &ssh_program(cx))?)?;
    let name = config.name();
    let (mut recorded, mut files) = (Vec::new(), Vec::new());
    for file in config.known_hosts.iter().filter(|file| file.is_file()) {
        let found = lookup(cx, file, &name)?;
        if !found.is_empty() {
            files.push(file.clone());
        }
        recorded.extend(found);
    }
    // `ssh` prefers the types it already knows for the host; so does the
    // scan, or a host with an RSA key on record would seem changed when it
    // merely offers ED25519 first.
    let preferred = algorithms_for(&recorded);
    let scanned = match preferred.is_empty() {
        true => scan(cx, target, None)?,
        false => scan(cx, target, Some(&preferred)).or_else(|_| scan(cx, target, None))?,
    };
    let presented = scanned
        .keys
        .first()
        .cloned()
        .ok_or_else(|| SshError::new("ssh_host_scan_failed", "ssh recorded no host key"))?;
    let status = if recorded.is_empty() {
        HostKeyStatus::Unknown
    } else if recorded.iter().any(|key| key.fingerprint == presented.fingerprint) {
        HostKeyStatus::Known
    } else {
        HostKeyStatus::Changed
    };
    let known_hosts = config
        .known_hosts
        .first()
        .cloned()
        .ok_or_else(|| SshError::new("ssh_host_scan_failed", "ssh names no UserKnownHostsFile"))?;
    let shared_with_terminal =
        terminal_shares(cx.ssh_config().is_some(), std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some());
    let report = HostKeyReport {
        name,
        hostname: config.hostname,
        port: config.port,
        status,
        presented,
        recorded,
        known_hosts,
        shared_with_terminal,
    };
    Ok(Inspected { report, lines: scanned.lines, files })
}

/// Whether the terminal's `ssh` uses the known hosts `ssh` uses here. macOS
/// sets `APP_SANDBOX_CONTAINER_ID` in a sandboxed app.
fn terminal_shares(own_config: bool, sandboxed: bool) -> bool {
    !own_config && !sandboxed
}

/// `ssh` with its own configuration file, when there is one
/// ([`Context::ssh_config`]); `extra` comes before the host.
fn ssh_command(cx: &Context, target: &SshTarget, extra: &[&str]) -> Command {
    let mut command = sys::command(ssh_program(cx));
    command.args(config_args(cx)).args(extra);
    if let Some(port) = target.port {
        command.arg("-p").arg(port.to_string());
    }
    command.arg("--").arg(&target.host);
    command
}

/// Run `command` to its end: its stdout, or its failure classified as `ssh`'s.
fn output_text(mut command: Command, program: &str) -> SshResult<String> {
    let output = command.output().map_err(|error| not_started(program, None, &error))?;
    if !output.status.success() {
        return Err(classify(output.status.code(), String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What `ssh -G` says about the host.
#[derive(Debug, PartialEq, Eq)]
struct Config {
    hostname: String,
    port: u16,
    alias: Option<String>,
    known_hosts: Vec<PathBuf>,
}

impl Config {
    /// The settings `ssh -G` printed: the host name (required), the port
    /// (22 when not given), the `HostKeyAlias` and the known-hosts files.
    fn parse(text: &str) -> SshResult<Self> {
        let value = |key: &str| {
            text.lines().find_map(|line| {
                line.split_once(' ').filter(|(name, _)| name.eq_ignore_ascii_case(key)).map(|(_, value)| value.trim())
            })
        };
        let hostname = value("hostname")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| SshError::new("ssh_failed", "ssh -G printed no hostname"))?;
        let port = value("port").and_then(|port| port.parse().ok()).unwrap_or(DEFAULT_PORT);
        let alias = value("hostkeyalias")
            .filter(|alias| !alias.is_empty() && !alias.eq_ignore_ascii_case("none"))
            .map(str::to_string);
        let known_hosts = split_paths(value("userknownhostsfile").unwrap_or("~/.ssh/known_hosts"))
            .into_iter()
            .filter_map(|path| local_path(&path))
            .collect();
        Ok(Self { hostname: hostname.to_string(), port, alias, known_hosts })
    }

    /// The host as `ssh` names it in the known hosts.
    fn name(&self) -> String {
        match &self.alias {
            Some(alias) => alias.clone(),
            None if self.port == DEFAULT_PORT => self.hostname.clone(),
            None => format!("[{}]:{}", self.hostname, self.port),
        }
    }
}

/// `ssh -G` lists the files on one line, separated by spaces, without
/// quoting a path that has one (a Windows home such as `C:\Users\A B`): a
/// new path starts only where a token looks like the start of a path.
fn split_paths(text: &str) -> Vec<String> {
    let starts_path = |token: &str| {
        let bytes = token.as_bytes();
        token.starts_with(['/', '~', '%', '\\'])
            || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    };
    let mut paths: Vec<String> = Vec::new();
    for token in text.split(' ').filter(|token| !token.is_empty()) {
        match paths.last_mut() {
            Some(last) if !starts_path(token) => {
                last.push(' ');
                last.push_str(token);
            }
            _ => paths.push(token.to_string()),
        }
    }
    paths
}

/// A path from `ssh -G` as this program opens it: `~` is the home
/// directory, and on Windows an MSYS or Cygwin `ssh` writes `C:` as `/c`.
fn local_path(path: &str) -> Option<PathBuf> {
    if path.eq_ignore_ascii_case("none") || path.contains('%') {
        return None;
    }
    if let Some(rest) = path.strip_prefix('~') {
        return Some(sys::home_dir()?.join(rest.trim_start_matches(['/', '\\'])));
    }
    if cfg!(windows) {
        let rest = path.strip_prefix("/cygdrive").unwrap_or(path);
        let bytes = rest.as_bytes();
        if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b'/' {
            return Some(PathBuf::from(format!("{}:{}", (bytes[1] as char).to_ascii_uppercase(), &rest[2..])));
        }
    }
    Some(PathBuf::from(path))
}

/// The keys `file` holds for `name` (hashed entries included). Revoked keys and certificate authorities are not host keys.
fn lookup(cx: &Context, file: &Path, name: &str) -> SshResult<Vec<HostKey>> {
    let program = keygen_program(cx);
    let output = sys::command(&program)
        .arg("-F")
        .arg(name)
        .arg("-f")
        .arg(file)
        .output()
        .map_err(|error| not_started(&program, None, &error))?;
    // `ssh-keygen -F` exits 1 when nothing matches.
    Ok(String::from_utf8_lossy(&output.stdout).lines().filter_map(key_of_line).collect())
}

/// The `ssh-keygen` to run: `<ENV>_SSH_KEYGEN`, else the one on `PATH`.
pub(crate) fn keygen_program(cx: &Context) -> String {
    std::env::var(cx.app().var("SSH_KEYGEN")).unwrap_or_else(|_| "ssh-keygen".into())
}

/// The key on one known-hosts line (`hosts type base64 [comment]`), or
/// `None` for a comment, a marked line or anything malformed.
pub(crate) fn key_of_line(line: &str) -> Option<HostKey> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
        return None;
    }
    let mut fields = line.split_whitespace();
    let (_hosts, key_type, blob) = (fields.next()?, fields.next()?, fields.next()?);
    Some(HostKey { key_type: key_type.to_string(), fingerprint: fingerprint(blob)? })
}

/// `SHA256:` and the unpadded base64 of the key blob's SHA-256, as
/// `ssh-keygen -l` prints it.
pub(crate) fn fingerprint(blob: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(blob).ok()?;
    Some(format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(Sha256::digest(bytes))))
}

/// The `HostKeyAlgorithms` that prefer the types already on record, best
/// first; unknown types are skipped.
fn algorithms_for(recorded: &[HostKey]) -> String {
    let mut algorithms: Vec<&str> = Vec::new();
    for key in recorded {
        let names: &[&str] = match key.key_type.as_str() {
            "ssh-rsa" => &["rsa-sha2-512", "rsa-sha2-256"],
            "ssh-ed25519" => &["ssh-ed25519"],
            "ecdsa-sha2-nistp256" => &["ecdsa-sha2-nistp256"],
            "ecdsa-sha2-nistp384" => &["ecdsa-sha2-nistp384"],
            "ecdsa-sha2-nistp521" => &["ecdsa-sha2-nistp521"],
            "sk-ssh-ed25519@openssh.com" => &["sk-ssh-ed25519@openssh.com"],
            "sk-ecdsa-sha2-nistp256@openssh.com" => &["sk-ecdsa-sha2-nistp256@openssh.com"],
            _ => &[],
        };
        for name in names {
            if !algorithms.contains(name) {
                algorithms.push(name);
            }
        }
    }
    algorithms.join(",")
}

/// What one scan brought: the lines `ssh` wrote and their keys.
struct Scanned {
    lines: Vec<String>,
    keys: Vec<HostKey>,
}

/// Connect once with a scratch known-hosts file that accepts any new key,
/// and offer no authentication: the host's key is all that is wanted.
fn scan(cx: &Context, target: &SshTarget, algorithms: Option<&str>) -> SshResult<Scanned> {
    let scratch = ScratchFile::new(cx)?;
    let known = format!("UserKnownHostsFile=\"{}\"", scratch.0.to_string_lossy().replace('\\', "/"));
    let mut options = vec![
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        known.as_str(),
        "-o",
        "GlobalKnownHostsFile=none",
        "-o",
        "CheckHostIP=no",
        "-o",
        "UpdateHostKeys=no",
        // A shared connection would skip the key exchange.
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
        "-o",
        "PreferredAuthentications=none",
    ];
    let preferred;
    if let Some(algorithms) = algorithms {
        preferred = format!("HostKeyAlgorithms={algorithms}");
        options.extend(["-o", preferred.as_str()]);
    }
    let mut command = ssh_command(cx, target, &options);
    command.arg("exit 0");
    let output = command.output().map_err(|error| not_started(&ssh_program(cx), None, &error))?;
    let text = std::fs::read_to_string(&scratch.0).unwrap_or_default();
    let lines: Vec<String> = text.lines().filter(|line| key_of_line(line).is_some()).map(str::to_string).collect();
    if lines.is_empty() {
        // Nothing accepted: the host was not reached (or a system-wide
        // known-hosts file disagrees with it).
        let error = classify(Some(255), String::from_utf8_lossy(&output.stderr).trim());
        return Err(match error.code {
            "ssh_failed" | "ssh_auth" => {
                SshError::new("ssh_host_scan_failed", format!("could not read the host key: {}", error.message))
            }
            _ => error,
        });
    }
    let keys = lines.iter().filter_map(|line| key_of_line(line)).collect();
    Ok(Scanned { lines, keys })
}

/// A file in the temporary directory, removed when dropped.
struct ScratchFile(PathBuf);

impl ScratchFile {
    /// An empty file of a fresh name in the temporary directory.
    fn new(cx: &Context) -> SshResult<Self> {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.subsec_nanos());
        let path = std::env::temp_dir().join(format!("{}-known-hosts-{}-{nanos}", cx.app().name, std::process::id()));
        std::fs::write(&path, b"")
            .map_err(|error| SshError::new("ssh_failed", format!("{}: {error}", path.display())))?;
        Ok(Self(path))
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Append `lines` to the known-hosts file, creating it (and `~/.ssh`, private
/// on Unix) when absent, and starting on a new line.
pub(crate) fn append_lines(file: &Path, lines: &[String]) -> SshResult<()> {
    let failed =
        |error: std::io::Error| SshError::new("known_hosts_write_failed", format!("{}: {error}", file.display()));
    if let Some(parent) = file.parent().filter(|parent| !parent.as_os_str().is_empty() && !parent.exists()) {
        sys::private_dir(parent).map_err(failed)?;
    }
    let existing = std::fs::read(file).unwrap_or_default();
    let mut text = String::new();
    if existing.last().is_some_and(|last| *last != b'\n') {
        text.push('\n');
    }
    for line in lines {
        text.push_str(line.trim_end());
        text.push('\n');
    }
    let mut handle = std::fs::OpenOptions::new().create(true).append(true).open(file).map_err(failed)?;
    handle.write_all(text.as_bytes()).map_err(failed)?;
    handle.sync_all().map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED25519: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIE4s2mGl/q34aQA6Sl8ZtiS2ZApHcYxJfzaMK1XwlE37";

    #[test]
    fn fingerprints_match_ssh_keygen() {
        // `ssh-keygen -l -E sha256` of this key.
        assert_eq!(fingerprint(ED25519).as_deref(), Some("SHA256:j/w23+9JZUYRZrs9tVzDog3bl2gmggCuJ2IL2JiGN34"));
        let key = key_of_line(&format!("|1|c2FsdA==|aGFzaA== ssh-ed25519 {ED25519}")).unwrap();
        assert_eq!(key.key_type, "ssh-ed25519");
        for line in ["# comment", "@revoked * ssh-ed25519 AAAA", "", "host ssh-ed25519 !!notbase64"] {
            assert_eq!(key_of_line(line), None, "{line}");
        }
    }

    #[test]
    fn the_known_hosts_name_follows_port_and_alias() {
        let text = "user me\nhostname gpu.lan\nport 2222\nhostkeyalias none\nuserknownhostsfile /k/a /k/b\n";
        let config = Config::parse(text).unwrap();
        assert_eq!(config.name(), "[gpu.lan]:2222");
        // On Windows `/k/…` is Git's spelling of drive K: (`local_path`).
        let expected = if cfg!(windows) { ["K:/a", "K:/b"] } else { ["/k/a", "/k/b"] };
        assert_eq!(config.known_hosts, expected.map(PathBuf::from));
        assert_eq!(Config::parse("hostname gpu.lan\nport 22\n").unwrap().name(), "gpu.lan");
        assert_eq!(Config::parse("hostname gpu.lan\nport 2222\nhostkeyalias gpu\n").unwrap().name(), "gpu");
    }

    #[test]
    fn known_hosts_paths_with_spaces_and_msys_drives() {
        assert_eq!(
            split_paths(r"C:\Users\A B/.ssh/known_hosts C:\Users\A B/.ssh/known_hosts2"),
            [r"C:\Users\A B/.ssh/known_hosts", r"C:\Users\A B/.ssh/known_hosts2"]
        );
        assert_eq!(split_paths("/home/me/.ssh/known_hosts ~/.ssh/k2"), ["/home/me/.ssh/known_hosts", "~/.ssh/k2"]);
        if cfg!(windows) {
            assert_eq!(local_path("/c/Users/me/.ssh/known_hosts"), Some(PathBuf::from("C:/Users/me/.ssh/known_hosts")));
        }
        assert_eq!(local_path("none"), None);
    }

    #[test]
    fn recorded_types_are_preferred() {
        let key = |key_type: &str| HostKey { key_type: key_type.into(), fingerprint: String::new() };
        assert_eq!(
            algorithms_for(&[key("ssh-rsa"), key("ssh-ed25519"), key("ssh-dss")]),
            "rsa-sha2-512,rsa-sha2-256,ssh-ed25519"
        );
        assert_eq!(algorithms_for(&[]), "");
    }

    #[test]
    fn appended_lines_start_on_a_new_line_and_create_the_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join(".ssh/known_hosts");
        let line = format!("gpu ssh-ed25519 {ED25519}");
        append_lines(&file, std::slice::from_ref(&line)).unwrap();
        std::fs::write(&file, format!("{line}\nother ssh-ed25519 {ED25519}")).unwrap();
        append_lines(&file, &[format!("third ssh-ed25519 {ED25519}")]).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text.lines().map(|line| line.split(' ').next().unwrap()).collect::<Vec<_>>(),
            ["gpu", "other", "third"]
        );
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn the_remove_command_names_the_file() {
        let report = HostKeyReport {
            name: "[gpu]:2222".into(),
            hostname: "gpu".into(),
            port: 2222,
            status: HostKeyStatus::Changed,
            presented: HostKey { key_type: "ssh-ed25519".into(), fingerprint: "SHA256:x".into() },
            recorded: vec![],
            known_hosts: PathBuf::from("/home/me/.ssh/known_hosts"),
            shared_with_terminal: true,
        };
        assert_eq!(report.remove_command(), "ssh-keygen -R \"[gpu]:2222\" -f \"/home/me/.ssh/known_hosts\"");
        assert_eq!(report.to_json()["status"], "changed");
        assert_eq!(report.to_json()["shared_with_terminal"], true);
    }

    #[test]
    fn only_the_users_own_ssh_setup_is_shared_with_the_terminal() {
        assert!(terminal_shares(false, false));
        assert!(!terminal_shares(true, false), "<ENV>_SSH_CONFIG");
        assert!(!terminal_shares(false, true), "a macOS sandbox");
    }

    /// A changed key is replaced (#17): the host's earlier key goes from
    /// every known-hosts file that names it and the presented one is
    /// recorded; other hosts keep theirs, and a key other than the one
    /// confirmed is refused.
    #[test]
    fn a_changed_key_is_replaced_by_the_confirmed_one_only() {
        let cx = Context::new(crate::App { name: "tool", env: "TOOL" });
        let folder = tempfile::tempdir().unwrap();
        let generated = sys::command(keygen_program(&cx))
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(folder.path().join("new"))
            .status();
        if !generated.is_ok_and(|status| status.success()) {
            eprintln!("no ssh-keygen here: nothing to replace with");
            return;
        }
        let public = std::fs::read_to_string(folder.path().join("new.pub")).unwrap();
        let blob = public.split_whitespace().nth(1).unwrap().to_string();
        let (known, second) = (folder.path().join("known_hosts"), folder.path().join("known_hosts2"));
        std::fs::write(&known, format!("gpu.lan ssh-ed25519 {ED25519}\nother.lan ssh-ed25519 {ED25519}\n")).unwrap();
        std::fs::write(&second, format!("gpu.lan ssh-ed25519 {ED25519}\n")).unwrap();
        let line = format!("gpu.lan ssh-ed25519 {blob}");
        let presented = key_of_line(&line).unwrap();
        let report = HostKeyReport {
            name: "gpu.lan".into(),
            hostname: "gpu.lan".into(),
            port: 22,
            status: HostKeyStatus::Changed,
            presented: presented.clone(),
            recorded: vec![key_of_line(&format!("gpu.lan ssh-ed25519 {ED25519}")).unwrap()],
            known_hosts: known.clone(),
            shared_with_terminal: true,
        };
        assert_eq!(confirmed(&report, "SHA256:another").unwrap_err().code, "ssh_host_key_mismatch");
        confirmed(&report, &presented.fingerprint).unwrap();

        let after = replaced(&cx, &report, &[line], &[known.clone(), second.clone()]).unwrap();
        assert_eq!((after.status, after.recorded.clone()), (HostKeyStatus::Known, vec![presented.clone()]));
        assert_eq!(lookup(&cx, &known, "gpu.lan").unwrap(), [presented]);
        assert!(lookup(&cx, &second, "gpu.lan").unwrap().is_empty());
        assert_eq!(lookup(&cx, &known, "other.lan").unwrap().len(), 1, "another host keeps its key");

        // Replaced at once by several: one record still.
        let line = format!("gpu.lan ssh-ed25519 {blob}");
        std::thread::scope(|scope| {
            let replacing: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| replaced(&cx, &report, std::slice::from_ref(&line), std::slice::from_ref(&known)))
                })
                .collect();
            for one in replacing {
                one.join().unwrap().unwrap();
            }
        });
        assert_eq!(lookup(&cx, &known, "gpu.lan").unwrap().len(), 1);
    }

    /// Several confirming a new host at once record its key once, and one
    /// that finds another key recorded meanwhile records nothing (#18).
    #[test]
    fn a_host_confirmed_at_once_is_recorded_once() {
        let cx = Context::new(crate::App { name: "tool", env: "TOOL" });
        if sys::command(keygen_program(&cx)).arg("-?").output().is_err() {
            eprintln!("no ssh-keygen here");
            return;
        }
        let folder = tempfile::tempdir().unwrap();
        let known = folder.path().join("ssh").join("known_hosts");
        let line = format!("new.lan ssh-ed25519 {ED25519}");
        let presented = key_of_line(&line).unwrap();
        let report = HostKeyReport {
            name: "new.lan".into(),
            hostname: "new.lan".into(),
            port: 22,
            status: HostKeyStatus::Unknown,
            presented,
            recorded: Vec::new(),
            known_hosts: known.clone(),
            shared_with_terminal: true,
        };
        std::thread::scope(|scope| {
            let confirming: Vec<_> =
                (0..6).map(|_| scope.spawn(|| recorded(&cx, report.clone(), std::slice::from_ref(&line)))).collect();
            for one in confirming {
                assert_eq!(one.join().unwrap().unwrap().status, HostKeyStatus::Known);
            }
        });
        assert_eq!(std::fs::read_to_string(&known).unwrap(), format!("{line}\n"), "one record");
        // Another key for it now: refused, nothing written.
        std::fs::write(&known, "new.lan ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC7\n").unwrap();
        assert_eq!(recorded(&cx, report, &[line]).unwrap_err().code, "ssh_host_key");
        assert!(!std::fs::read_to_string(&known).unwrap().contains("ed25519"));
    }
}
