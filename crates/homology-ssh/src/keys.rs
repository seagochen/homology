//! Login keys the program sets up for hosts that only took a password.
//!
//! When the user asks to connect automatically from then on, the program
//! generates an ed25519 key pair in its own configuration directory (never
//! in `~/.ssh`), appends the public key to the host's
//! `~/.ssh/authorized_keys` with the password they typed once, checks that
//! the key logs in, and remembers "this host uses this key" in `hosts.json`
//! beside the keys. Every later `ssh` / `sftp` run for that host adds
//! `-i <key>`; other hosts keep using only the user's own configuration and
//! agent. The password is dropped as soon as the call returns.
//!
//! The directory is the session's ([`Context::set_keys_dir`], set by the
//! program from where it keeps its settings); without it nothing is
//! remembered.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use homology_sys as sys;

use crate::askpass::Password;
use crate::host_key::{fingerprint, keygen_program};
use crate::logins::{directory, read_hosts, write_hosts};
use crate::ssh::{self, not_started};
use crate::target::{shell_quote, SshTarget};
use crate::{Context, SshError, SshResult};

/// Moved to [`crate::logins`], which reads `hosts.json` without `ssh`; kept here as the public path.
pub use crate::logins::HostKeyLogin;

/// The hosts with a key of the program's.
pub fn list(cx: &Context) -> SshResult<Vec<HostKeyLogin>> {
    Ok(read_hosts(&directory(cx)?))
}

/// `<name>@<this computer>`, with only characters a key comment and a
/// shell word need no quoting for.
pub(crate) fn comment(name_of_app: &str) -> String {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            sys::command("hostname")
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        })
        .unwrap_or_default();
    let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
    format!("{name_of_app}@{}", if name.is_empty() { "computer" } else { &name })
}

/// Generate a key, install it on `target` with `password`, check that it
/// logs in, and remember it. On any failure nothing is left behind: no key
/// files here, no line on the host.
pub fn set_up(cx: &Context, target: &SshTarget, password: &Password) -> SshResult<HostKeyLogin> {
    let directory = directory(cx)?;
    prepared(&directory)?;
    let host_part: String =
        target.host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
    let name = format!("id_ed25519_{host_part}_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let private = directory.join(&name);
    let comment = comment(cx.app().name);
    let generated = generate(cx, &private, &comment);
    let cleanup = |private: &Path| {
        let _ = std::fs::remove_file(private);
        let _ = std::fs::remove_file(public_path(private));
    };
    if let Err(error) = generated {
        cleanup(&private);
        return Err(error);
    }
    let installed = install(cx, target, password, &private);
    let line = match installed {
        Ok(line) => line,
        Err(error) => {
            cleanup(&private);
            return Err(error);
        }
    };
    // The key must log in on its own before it is remembered.
    if let Err(error) = ssh::run_with_identity(cx, target, "true", &private) {
        let _ = ssh::run_with_password(cx, target, &remove_script(cx.app().name, &line), password);
        cleanup(&private);
        return Err(SshError::new(
            "ssh_key_rejected",
            format!("the host did not accept the installed key ({}); it was removed again", error.message),
        ));
    }
    let blob = line.split_whitespace().nth(1).unwrap_or_default();
    let login = HostKeyLogin {
        host: target.host.clone(),
        port: target.port,
        key: name,
        comment,
        fingerprint: fingerprint(blob).unwrap_or_default(),
        created_at: now(),
    };
    // Other programs share the folder: the list is changed under its lock.
    let lock = hosts_lock(&directory);
    let mut hosts: Vec<HostKeyLogin> = read_hosts(&directory);
    let replaced: Vec<HostKeyLogin> = hosts.iter().filter(|other| other.matches(target)).cloned().collect();
    hosts.retain(|other| !other.matches(target));
    hosts.push(login.clone());
    let written = lock.and_then(|_lock| write_hosts(&directory, &hosts));
    if let Err(error) = written {
        let _ = ssh::run_with_identity(cx, target, &remove_script(cx.app().name, &line), &private);
        cleanup(&private);
        return Err(error);
    }
    // A key set up earlier for the same host is not used any more.
    for old in replaced {
        cleanup(&directory.join(old.key));
    }
    Ok(login)
}

/// What removing a host's key did.
#[derive(Debug)]
pub struct Removed {
    pub login: HostKeyLogin,
    /// Whether the line was removed from the host's `authorized_keys`.
    pub remote_cleaned: bool,
    /// Why it was not, when it was not.
    pub remote_error: Option<String>,
}

/// Forget the key for `host`/`port`: remove its line from the host's
/// `authorized_keys` (logging in with the key itself), then delete it here.
/// The local key goes even when the host cannot be reached.
pub fn remove(cx: &Context, host: &str, port: Option<u16>) -> SshResult<Removed> {
    let directory = directory(cx)?;
    let _lock = hosts_lock(&directory)?;
    let mut hosts = read_hosts(&directory);
    let position = hosts
        .iter()
        .position(|login| login.host == host && login.port == port)
        .ok_or_else(|| SshError::new("ssh_key_not_found", format!("no login key is set up for {host}")))?;
    let login = hosts.remove(position);
    let private = directory.join(&login.key);
    let remote = match std::fs::read_to_string(public_path(&private)) {
        Ok(line) => {
            let target = SshTarget { host: login.host.clone(), port: login.port, path: "~".into() };
            ssh::run_with_identity(cx, &target, &remove_script(cx.app().name, line.trim()), &private).map(|_| ())
        }
        Err(error) => Err(SshError::new("ssh_keys_failed", error.to_string())),
    };
    write_hosts(&directory, &hosts)?;
    let _ = std::fs::remove_file(&private);
    let _ = std::fs::remove_file(public_path(&private));
    Ok(Removed { login, remote_cleaned: remote.is_ok(), remote_error: remote.err().map(|error| error.message) })
}

/// The keys folder made if need be, readable by the user alone.
fn prepared(directory: &Path) -> SshResult<()> {
    let failed = |error: std::io::Error| SshError::new("ssh_keys_failed", format!("{}: {error}", directory.display()));
    sys::private_dir(directory).map_err(failed)?;
    #[cfg(windows)]
    icacls_owner_only(directory)?;
    Ok(())
}

/// The lock of the keys folder's host list, which programs sharing the
/// folder change too (#17).
fn hosts_lock(directory: &Path) -> SshResult<sys::LockFile> {
    let hosts = directory.join(crate::logins::HOSTS);
    sys::LockFile::take(&hosts)
        .map_err(|error| SshError::new("ssh_keys_failed", format!("{}: {error}", hosts.display())))
}

/// What moving a program's earlier login keys into the keys folder did:
/// the hosts that got their key, and those that kept the one already there.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Adopted {
    pub moved: usize,
    pub kept: usize,
}

/// Move the login keys a program set up in `from` (its own folder, from
/// before the shared one, #17) into the session's keys folder: a host the
/// folder has no key for yet gets the earlier key (its files copied, under
/// another name when the folder has a file of that name); a host it has a
/// key for keeps that one, as does one whose key file is gone. `from` is
/// left as it was, so moving again changes nothing.
pub fn adopt(cx: &Context, from: &Path) -> SshResult<Adopted> {
    let directory = directory(cx)?;
    let earlier = read_hosts(from);
    if earlier.is_empty() || same_folder(from, &directory) {
        return Ok(Adopted::default());
    }
    prepared(&directory)?;
    let _lock = hosts_lock(&directory)?;
    let failed = |error: std::io::Error| SshError::new("ssh_keys_failed", format!("{}: {error}", directory.display()));
    let mut hosts = read_hosts(&directory);
    let mut adopted = Adopted::default();
    for mut login in earlier {
        let source = from.join(&login.key);
        if hosts.iter().any(|other| other.host == login.host && other.port == login.port) || !source.is_file() {
            adopted.kept += 1;
            continue;
        }
        let name = free_name(&directory, &login.key);
        let private = directory.join(&name);
        std::fs::copy(&source, &private).map_err(failed)?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &private,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )
        .map_err(failed)?;
        if public_path(&source).is_file() {
            std::fs::copy(public_path(&source), public_path(&private)).map_err(failed)?;
        }
        login.key = name;
        hosts.push(login);
        adopted.moved += 1;
    }
    if adopted.moved > 0 {
        write_hosts(&directory, &hosts)?;
    }
    Ok(adopted)
}

/// Whether `one` and `other` are the same folder.
fn same_folder(one: &Path, other: &Path) -> bool {
    one == other || one.canonicalize().ok().is_some_and(|one| other.canonicalize().ok() == Some(one))
}

/// `name`, or `name_2`, `name_3` … , the first no file in `directory` has.
fn free_name(directory: &Path, name: &str) -> String {
    let taken =
        |candidate: &str| directory.join(candidate).exists() || public_path(&directory.join(candidate)).exists();
    if !taken(name) {
        return name.to_string();
    }
    (2..).map(|at| format!("{name}_{at}")).find(|candidate| !taken(candidate)).unwrap_or_else(|| name.to_string())
}

/// The public key beside a private key file (`<name>.pub`).
fn public_path(private: &Path) -> PathBuf {
    let mut path = private.as_os_str().to_owned();
    path.push(".pub");
    PathBuf::from(path)
}

/// The time now in UTC, as `created_at` records it (`2026-11-10T08:30:00Z`).
pub(crate) fn now() -> String {
    let seconds = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_secs());
    utc(seconds)
}

/// `seconds` since the epoch as an ISO 8601 UTC time, to the second: the
/// civil date of the day count (Howard Hinnant's algorithm), then the time.
fn utc(seconds: u64) -> String {
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// `ssh-keygen` an unencrypted ed25519 pair at `private` (+ `.pub`),
/// readable only by this user.
fn generate(cx: &Context, private: &Path, comment: &str) -> SshResult<()> {
    let program = keygen_program(cx);
    let output = sys::command(&program)
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(private)
        .stdout(Stdio::null())
        .output()
        .map_err(|error| not_started(&program, None, &error))?;
    if !output.status.success() {
        return Err(SshError::new(
            "ssh_keys_failed",
            format!("ssh-keygen failed: {}", String::from_utf8_lossy(&output.stderr).trim()),
        ));
    }
    restrict_file(private)
}

/// Owner-only access: `0600` on Unix; on Windows inheritance is cut and only
/// the current user is granted access, as OpenSSH requires of a private key.
fn restrict_file(path: &Path) -> SshResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| SshError::new("ssh_keys_failed", format!("{}: {error}", path.display())))?;
    }
    #[cfg(windows)]
    icacls_owner_only(path)?;
    Ok(())
}

/// Cut inherited access to `path` and grant it to the current user alone
/// (`icacls`).
#[cfg(windows)]
fn icacls_owner_only(path: &Path) -> SshResult<()> {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(user)) if !domain.is_empty() => format!("{domain}\\{user}"),
        (_, Ok(user)) => user,
        _ => return Err(SshError::new("ssh_keys_failed", "USERNAME is not set")),
    };
    let output = sys::command("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .stdout(Stdio::null())
        .output()
        .map_err(|error| SshError::new("ssh_keys_failed", format!("cannot run icacls: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(SshError::new(
            "ssh_keys_failed",
            format!("icacls {}: {}", path.display(), String::from_utf8_lossy(&output.stderr).trim()),
        ))
    }
}

/// Append the public key to the host's `authorized_keys` with the password;
/// the line appended.
fn install(cx: &Context, target: &SshTarget, password: &Password, private: &Path) -> SshResult<String> {
    let line = std::fs::read_to_string(public_path(private))
        .map_err(|error| SshError::new("ssh_keys_failed", error.to_string()))?;
    let line = line.trim().to_string();
    if line.is_empty() || line.contains(['\n', '\'']) {
        return Err(SshError::new("ssh_keys_failed", "unexpected public key"));
    }
    ssh::run_with_password(cx, target, &install_script(&line), password)?;
    Ok(line)
}

/// Append `line`, creating `~/.ssh` (0700) and `authorized_keys` (0600) as
/// `ssh-copy-id` does, and starting on a new line.
pub(crate) fn install_script(line: &str) -> String {
    format!(
        "umask 077; mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && \
         {{ [ ! -s ~/.ssh/authorized_keys ] || [ -z \"$(tail -c1 ~/.ssh/authorized_keys)\" ] || echo >> ~/.ssh/authorized_keys; }} && \
         printf '%s\\n' {} >> ~/.ssh/authorized_keys",
        shell_quote(line)
    )
}

/// Drop every line holding this key's blob; the file keeps its
/// permissions. The temporary file beside it is named after the program
/// (`name`, a plain word).
pub(crate) fn remove_script(name: &str, line: &str) -> String {
    let blob = line.split_whitespace().nth(1).unwrap_or(line);
    format!(
        "f=~/.ssh/authorized_keys; [ -f \"$f\" ] || exit 0; t=\"$f.{name}-tmp\"; grep -vF {} \"$f\" > \"$t\"; s=$?; \
         if [ $s -le 1 ]; then cat \"$t\" > \"$f\"; fi; rm -f \"$t\"; [ $s -le 1 ]",
        shell_quote(blob)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const LINE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE4s2mGl/q34aQA6Sl8ZtiS2ZApHcYxJfzaMK1XwlE37 tool@pc";

    fn sh(script: &str, home: &Path) -> bool {
        Command::new("sh").arg("-c").arg(script).env("HOME", home).status().is_ok_and(|status| status.success())
    }

    #[test]
    fn the_scripts_append_once_and_remove_only_that_key() {
        if Command::new("sh").arg("-c").arg("true").status().is_err() {
            eprintln!("skipped: no sh");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let ssh_dir = home.path().join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        // An existing file without a final newline.
        std::fs::write(ssh_dir.join("authorized_keys"), "ssh-ed25519 AAAAother me@laptop").unwrap();
        assert!(sh(&install_script(LINE), home.path()));
        let text = std::fs::read_to_string(ssh_dir.join("authorized_keys")).unwrap();
        assert_eq!(text, format!("ssh-ed25519 AAAAother me@laptop\n{LINE}\n"));
        assert!(sh(&remove_script("tool", LINE), home.path()));
        assert_eq!(
            std::fs::read_to_string(ssh_dir.join("authorized_keys")).unwrap(),
            "ssh-ed25519 AAAAother me@laptop\n"
        );
        // Removing from an emptied or absent file succeeds.
        assert!(sh(&remove_script("tool", "ssh-ed25519 AAAAother x"), home.path()));
        assert_eq!(std::fs::read_to_string(ssh_dir.join("authorized_keys")).unwrap(), "");
        std::fs::remove_file(ssh_dir.join("authorized_keys")).unwrap();
        assert!(sh(&remove_script("tool", LINE), home.path()));
        // A fresh home gets the directory and the file.
        let fresh = tempfile::tempdir().unwrap();
        assert!(sh(&install_script(LINE), fresh.path()));
        assert_eq!(std::fs::read_to_string(fresh.path().join(".ssh/authorized_keys")).unwrap(), format!("{LINE}\n"));
    }

    #[test]
    fn times_are_utc_to_the_second() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_794_299_445), "2026-11-10T08:30:45Z");
    }

    #[test]
    fn the_comment_is_a_plain_word() {
        let comment = comment("tool");
        assert!(
            comment.starts_with("tool@") && comment.chars().all(|c| c.is_ascii_alphanumeric() || "@.-_".contains(c)),
            "{comment}"
        );
    }

    /// A program's earlier keys move into the shared folder (#17): hosts
    /// it has no key for get theirs, under another name when one is taken;
    /// a host with a key there keeps it; moving again changes nothing and
    /// the earlier folder stays as it was.
    #[test]
    fn earlier_keys_move_into_the_shared_folder_once() {
        let (earlier, shared) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let login = |host: &str, key: &str| HostKeyLogin {
            host: host.into(),
            port: None,
            key: key.into(),
            comment: "tool@pc".into(),
            fingerprint: format!("SHA256:{host}"),
            created_at: "2026-10-16T00:00:00Z".into(),
        };
        for (folder, name, content) in [
            (earlier.path(), "id_a", "A"),
            (earlier.path(), "id_b", "B"),
            (shared.path(), "id_a", "SHARED A NAME"),
            (shared.path(), "id_c", "C SHARED"),
            (earlier.path(), "id_c", "C EARLIER"),
        ] {
            std::fs::write(folder.join(name), content).unwrap();
            std::fs::write(folder.join(format!("{name}.pub")), format!("{content} pub")).unwrap();
        }
        let earlier_hosts =
            [login("me@a", "id_a"), login("me@b", "id_b"), login("me@c", "id_c"), login("me@gone", "id_gone")];
        write_hosts(earlier.path(), &earlier_hosts).unwrap();
        write_hosts(shared.path(), &[login("me@c", "id_c")]).unwrap();
        let cx = Context::new(crate::App { name: "tool", env: "TOOL" });
        cx.set_keys_dir(shared.path().to_path_buf());

        assert_eq!(adopt(&cx, earlier.path()).unwrap(), Adopted { moved: 2, kept: 2 });
        let hosts = read_hosts(shared.path());
        let key_of = |host: &str| hosts.iter().find(|login| login.host == host).map(|login| login.key.clone());
        assert_eq!(key_of("me@a").as_deref(), Some("id_a_2"), "id_a was taken");
        assert_eq!(std::fs::read_to_string(shared.path().join("id_a_2")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(shared.path().join("id_a_2.pub")).unwrap(), "A pub");
        assert_eq!(key_of("me@b").as_deref(), Some("id_b"));
        assert_eq!(std::fs::read_to_string(shared.path().join("id_c")).unwrap(), "C SHARED", "kept");
        assert!(key_of("me@gone").is_none(), "no key file: nothing to move");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(shared.path().join("id_b")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(adopt(&cx, earlier.path()).unwrap(), Adopted { moved: 0, kept: 4 }, "again: nothing moves");
        assert_eq!(read_hosts(shared.path()).len(), 3);
        assert_eq!(read_hosts(earlier.path()), earlier_hosts, "the earlier folder is as it was");
        assert_eq!(adopt(&cx, shared.path()).unwrap(), Adopted::default(), "the folder itself");
    }
}
