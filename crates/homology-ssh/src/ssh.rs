//! Running commands on a host with the system `ssh`: a script to its end,
//! or a session whose stdin and stdout the caller talks over; the way it
//! logs in (the user's configuration and agent, a login key set up here, a
//! password once); and its failures under stable codes ([`classify`]).

use std::path::Path;
use std::process::Command;

use homology_sys as sys;

use crate::askpass::{self, Password};
use crate::logins;
use crate::target::SshTarget;
use crate::{Context, SshError, SshResult};

/// The variable naming another `ssh` (tests and unusual installs):
/// `<ENV>_SSH`.
pub fn ssh_env(cx: &Context) -> String {
    cx.app().var("SSH")
}

/// The `ssh` to run: `<ENV>_SSH`, else the one on `PATH`.
pub fn ssh_program(cx: &Context) -> String {
    std::env::var(ssh_env(cx)).unwrap_or_else(|_| "ssh".into())
}

/// `-F <file>` when `<ENV>_SSH_CONFIG`, or the sandboxed program's own
/// configuration, names one ([`Context::ssh_config`]).
pub fn config_args(cx: &Context) -> Vec<std::ffi::OsString> {
    cx.ssh_config().map(|file| vec!["-F".into(), file.into_os_string()]).unwrap_or_default()
}

/// The common options: never prompt (a prompt would hang a window with no
/// terminal), give up on dead hosts, and keep idle connections alive.
pub fn ssh_options() -> [&'static str; 6] {
    ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15"]
}

/// A password login: prompts go to the askpass program (so not batch
/// mode), but only for the password, once; the host key must already be
/// known; no keys are offered.
pub fn password_options() -> [&'static str; 14] {
    [
        "-o",
        "BatchMode=no",
        "-o",
        "StrictHostKeyChecking=yes",
        "-o",
        "PubkeyAuthentication=no",
        "-o",
        "PreferredAuthentications=password,keyboard-interactive",
        "-o",
        "NumberOfPasswordPrompts=1",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
    ]
}

/// The failure of a program that did not start: `ssh_missing` when it is not
/// there (`variable`, when given, names the variable that points at another),
/// else `ssh_failed`.
pub fn not_started(program: &str, variable: Option<&str>, error: &std::io::Error) -> SshError {
    match error.kind() {
        std::io::ErrorKind::NotFound => match variable {
            Some(variable) => {
                SshError::new("ssh_missing", format!("cannot find `{program}`; install OpenSSH or set {variable}"))
            }
            None => SshError::new("ssh_missing", format!("cannot find `{program}`; install OpenSSH")),
        },
        _ => SshError::new("ssh_failed", format!("cannot run `{program}`: {error}")),
    }
}

/// How a run logs in.
pub enum Login<'a> {
    /// The user's configuration and agent, plus the key the program set up
    /// for this host, if any.
    Default,
    /// This key only, in batch mode.
    Identity(&'a Path),
    /// This password, through the askpass channel; no keys.
    Password(&'a Password),
}

/// `-i <key>` when the program set up a login key for `target`.
pub fn identity_args(cx: &Context, target: &SshTarget) -> Vec<std::ffi::OsString> {
    logins::identity(cx, target).map(|key| vec!["-i".into(), key.into_os_string()]).unwrap_or_default()
}

/// Run `script` (a POSIX shell command line) on the host; its stdout.
pub fn run(cx: &Context, target: &SshTarget, script: &str) -> SshResult<Vec<u8>> {
    run_as(cx, target, script, Login::Default)
}

/// `run` with `input` on the script's stdin: a file sent in the same
/// connection that checks and places it (a runner), so nothing needs a
/// second login or a temporary file here.
pub fn run_with_input(cx: &Context, target: &SshTarget, script: &str, input: &[u8]) -> SshResult<Vec<u8>> {
    use std::io::Write;
    let program = ssh_program(cx);
    let (mut command, _askpass) = command_for(cx, target, script, Login::Default)?;
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|error| not_started(&program, Some(&ssh_env(cx)), &error))?;
    // Written aside so that a script answering before it has read
    // everything cannot stall the input against its output.
    let writer = child.stdin.take().map(|mut stdin| {
        let input = input.to_vec();
        std::thread::spawn(move || stdin.write_all(&input))
    });
    let output = child
        .wait_with_output()
        .map_err(|error| SshError::new("ssh_failed", format!("cannot run `{program}`: {error}")))?;
    let written =
        writer.map(|writer| writer.join().unwrap_or_else(|_| Err(std::io::Error::other("the writer stopped"))));
    if output.status.success() {
        if let Some(Err(error)) = written {
            return Err(SshError::new("ssh_failed", format!("could not send the input to the host: {error}")));
        }
        return Ok(output.stdout);
    }
    Err(classify(output.status.code(), String::from_utf8_lossy(&output.stderr).trim()))
}

/// The `ssh` command that runs `script` on the host and keeps its stdin
/// and stdout for the caller to talk over (a runner's `serve`):
/// the same login, configuration and host key checks as [`run`], in batch
/// mode. The caller sets the stdio and starts it.
pub fn session_command(cx: &Context, target: &SshTarget, script: &str) -> SshResult<Command> {
    command_for(cx, target, script, Login::Default).map(|(command, _askpass)| command)
}

/// `run`, logging in with `key` only.
pub fn run_with_identity(cx: &Context, target: &SshTarget, script: &str, key: &Path) -> SshResult<Vec<u8>> {
    run_as(cx, target, script, Login::Identity(key))
}

/// `run`, logging in with `password` (handed to `ssh` through the askpass
/// channel, never on its command line or in its environment). A refusal is
/// `ssh_password_wrong`.
pub fn run_with_password(cx: &Context, target: &SshTarget, script: &str, password: &Password) -> SshResult<Vec<u8>> {
    run_as(cx, target, script, Login::Password(password)).map_err(|error| match error.code {
        "ssh_auth" | "ssh_password_required" => SshError::new("ssh_password_wrong", "the host refused the password"),
        _ => error,
    })
}

/// [`run`] logging in as `login` says: `ssh`'s stdout, or its failure
/// classified (`ssh_askpass_failed` when a password never reached it).
pub fn run_as(cx: &Context, target: &SshTarget, script: &str, login: Login<'_>) -> SshResult<Vec<u8>> {
    let program = ssh_program(cx);
    // The askpass channel stays open until the run ends.
    let (mut command, askpass) = command_for(cx, target, script, login)?;
    let output = command.output().map_err(|error| not_started(&program, Some(&ssh_env(cx)), &error))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let error = classify(output.status.code(), stderr.trim());
    Err(match askpass {
        Some(server) => untried_password(cx, error, server.answered()),
        None => error,
    })
}

/// A password run the host asked a password for, whose askpass program
/// never collected it: the program did not start (a sandbox can stop it)
/// or could not reach the channel. The password was not tried, so it must
/// not be called wrong.
fn untried_password(cx: &Context, error: SshError, answered: bool) -> SshError {
    if error.code != "ssh_password_required" || answered {
        return error;
    }
    let program = askpass::program(cx.app()).map(|program| program.display().to_string()).unwrap_or_default();
    SshError::new(
        "ssh_askpass_failed",
        format!("the password never reached ssh: its askpass program `{program}` did not run or could not answer"),
    )
}

/// The `ssh` command for one run, and the askpass channel a password
/// login answers through.
pub fn command_for(
    cx: &Context,
    target: &SshTarget,
    script: &str,
    login: Login<'_>,
) -> SshResult<(Command, Option<askpass::Server>)> {
    let mut command = sys::command(ssh_program(cx));
    command.args(config_args(cx));
    let mut askpass = None;
    match login {
        Login::Default => {
            command.args(ssh_options()).args(identity_args(cx, target));
        }
        Login::Identity(key) => {
            command.args(ssh_options()).args(["-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none", "-i"]).arg(key);
        }
        Login::Password(password) => {
            let server = askpass::Server::start(password)?;
            command.args(password_options());
            command
                .env("SSH_ASKPASS", askpass::program(cx.app())?)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(askpass::env_name(cx.app()), server.env_value());
            // OpenSSH before 8.4 uses askpass only with a display set.
            if std::env::var_os("DISPLAY").is_none() {
                command.env("DISPLAY", "none");
            }
            askpass = Some(server);
        }
    }
    if let Some(port) = target.port {
        command.arg("-p").arg(port.to_string());
    }
    // The host was checked not to start with `-`; `--` ends the options anyway.
    command.arg("--").arg(&target.host).arg(script);
    Ok((command, askpass))
}

/// Turn a failed run into a stable code. `ssh` exits with 255 for its own
/// errors; anything else came from the remote command: `remote_failed`
/// with its exit status, which the caller's scripts may give a meaning.
pub fn classify(status: Option<i32>, stderr: &str) -> SshError {
    let message = if stderr.is_empty() {
        format!("ssh exited with {status:?}")
    } else {
        stderr.lines().last().unwrap_or(stderr).to_string()
    };
    if status != Some(255) {
        return SshError { status, ..SshError::new("remote_failed", message) };
    }
    let lower = stderr.to_lowercase();
    // A key that differs from the one on record (or, with CheckHostIP, from
    // the one recorded for the address) is a warning; a host with no key on
    // record merely fails verification.
    let code = if [
        "remote host identification has changed",
        "has changed and you have requested strict checking",
        "possible dns spoofing",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "ssh_host_key"
    } else if lower.contains("host key verification failed") {
        "ssh_host_unknown"
    } else if lower.contains("permission denied") && accepts_password(&lower) {
        "ssh_password_required"
    } else if lower.contains("permission denied") || lower.contains("too many authentication failures") {
        "ssh_auth"
    } else if [
        "could not resolve", "connection refused", "timed out", "no route to host", "network is unreachable",
        "connection closed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "ssh_unreachable"
    } else {
        "ssh_failed"
    };
    let hint = match code {
        "ssh_host_unknown" => "; the host's key is not in your known hosts yet: compare its fingerprint, then trust it",
        "ssh_host_key" => "; the host's key differs from the one in your known hosts: the server may have been reinstalled, or someone may be intercepting the connection",
        "ssh_auth" => "; add your key to ssh-agent or name it with IdentityFile in ~/.ssh/config",
        "ssh_password_required" => "; the host takes a password: enter it once to set up a login key",
        _ => "",
    };
    SshError::new(code, format!("{message}{hint}"))
}

/// Whether the methods in `Permission denied (publickey,password).` include
/// a password (or keyboard-interactive, how PAM asks for one).
fn accepts_password(lower: &str) -> bool {
    lower.rsplit_once("permission denied (").and_then(|(_, rest)| rest.split_once(')')).is_some_and(|(methods, _)| {
        methods.split(',').any(|method| matches!(method.trim(), "password" | "keyboard-interactive"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_session_has_its_own_ssh_configuration_and_login_keys() {
        // What one session sets is not another's.
        let (one, other) = (Context::default(), Context::default());
        one.set_ssh_config(std::path::PathBuf::from("/container/ssh-config/config"));
        if std::env::var_os(one.app().var("SSH_CONFIG")).is_none() {
            assert_eq!(
                config_args(&one),
                [std::ffi::OsString::from("-F"), std::ffi::OsString::from("/container/ssh-config/config")]
            );
            assert!(config_args(&other).is_empty());
        }
        let keys = tempfile::tempdir().unwrap();
        one.set_keys_dir(keys.path().to_path_buf());
        assert_eq!(logins::directory(&one).unwrap(), keys.path());
        assert_eq!(logins::directory(&other).unwrap_err().code, "ssh_keys_unavailable");
    }

    #[test]
    fn a_password_is_in_no_argument_or_environment_variable() {
        let target = SshTarget::parse("ssh://me@gpu:2222/data").unwrap();
        let secret = "correct horse battery staple";
        let cx = Context::new(crate::App { name: "tool", env: "TOOL" });
        let (command, server) = command_for(&cx, &target, "true", Login::Password(&Password::new(secret))).unwrap();
        let arguments: Vec<String> =
            command.get_args().map(|argument| argument.to_string_lossy().into_owned()).collect();
        let environment: Vec<String> = command
            .get_envs()
            .map(|(name, value)| {
                format!(
                    "{}={}",
                    name.to_string_lossy(),
                    value.map(|value| value.to_string_lossy().into_owned()).unwrap_or_default()
                )
            })
            .collect();
        assert!(
            arguments.iter().chain(&environment).all(|text| !text.contains(secret)),
            "{arguments:?} {environment:?}"
        );
        assert!(
            arguments.contains(&"PubkeyAuthentication=no".to_string())
                && arguments.contains(&"StrictHostKeyChecking=yes".to_string())
        );
        assert!(environment.iter().any(|entry| entry == "SSH_ASKPASS_REQUIRE=force"));
        // What the askpass program would be given leads to the password, once.
        let channel = environment.iter().find_map(|entry| entry.strip_prefix("TOOL_ASKPASS=")).unwrap();
        assert_eq!(askpass::fetch(channel).as_deref(), Some(secret.as_bytes()));
        assert_eq!(askpass::fetch(channel), None);
        drop(server);
    }

    #[test]
    fn failures_get_stable_codes() {
        assert_eq!(classify(Some(255), "me@gpu: Permission denied (publickey).").code, "ssh_auth");
        let unknown = "No ED25519 host key is known for gpu and you have requested strict checking.\nHost key verification failed.";
        assert_eq!(classify(Some(255), unknown).code, "ssh_host_unknown");
        let changed = "@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\nHost key for gpu has changed and you have requested strict checking.\nHost key verification failed.";
        assert_eq!(classify(Some(255), changed).code, "ssh_host_key");
        assert_eq!(
            classify(Some(255), "ssh: Could not resolve hostname nope: No such host is known.").code,
            "ssh_unreachable"
        );
        assert_eq!(
            classify(Some(255), "ssh: connect to host 10.0.0.9 port 22: Connection timed out").code,
            "ssh_unreachable"
        );
        let failed = classify(Some(3), "boom");
        assert_eq!((failed.code, failed.status, failed.message.as_str()), ("remote_failed", Some(3), "boom"));
        assert_eq!(classify(Some(255), "boom").status, None, "ssh's own failure");
        assert_eq!(
            classify(Some(255), "cxt@gpu: Permission denied (publickey,password).").code,
            "ssh_password_required"
        );
        assert_eq!(
            classify(Some(255), "cxt@gpu: Permission denied (publickey,keyboard-interactive).").code,
            "ssh_password_required"
        );
        assert!(classify(Some(255), "Permission denied").message.contains("ssh-agent"));
    }

    #[test]
    fn a_password_askpass_never_collected_is_not_called_wrong() {
        let refused = || classify(Some(255), "me@gpu: Permission denied (publickey,password).");
        let cx = Context::default();
        assert_eq!(untried_password(&cx, refused(), true).code, "ssh_password_required", "tried and refused");
        assert_eq!(untried_password(&cx, refused(), false).code, "ssh_askpass_failed");
        let unreachable = classify(Some(255), "ssh: connect to host gpu port 22: Connection refused");
        assert_eq!(untried_password(&cx, unreachable, false).code, "ssh_unreachable", "no password was asked for");
    }
}
