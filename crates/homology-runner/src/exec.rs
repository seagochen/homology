//! A command run in a folder for the caller (`code.exec`, homology#8): a
//! build's or a checker's (`cargo check`, `cc -fsyntax-only`), whose exit
//! code and output the caller reads, diagnostics and all. It runs by the
//! system's shell, in its own process group, for at most a time limit;
//! past it, the whole group ends, so what a build started ends with it.
//! Its stdout and stderr are each kept to [`MAX_OUTPUT`].
//!
//! The command is the caller's, as a console's line is: the runner serves
//! only its caller, over the user's own pipe or SSH, and whether a command
//! may run is the caller's to decide.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::env::MAX_OUTPUT;
use crate::protocol::INVALID_PARAMS;
use crate::{JsonMap, RunnerError, RunnerResult};

/// The command could not be started.
pub const EXEC_FAILED: &str = "exec_failed";
/// The time a command may take when the caller does not say.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// The most time a caller may give one.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// How long the output is waited for once the command ended: what it left
/// in the background may hold its pipes open.
const DRAIN: Duration = Duration::from_secs(2);

/// How a command went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// Its exit code; none when a signal ended it, or the time ran out.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The time ran out: it was ended, with its process group.
    pub timed_out: bool,
    /// Some output was over [`MAX_OUTPUT`] and dropped.
    pub truncated: bool,
}

impl Ran {
    /// As `code.exec` answers.
    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code,
            "stdout": self.stdout,
            "stderr": self.stderr,
            "timed_out": self.timed_out,
            "truncated": self.truncated,
        })
    }
}

/// What a pipe gave: its bytes (up to [`MAX_OUTPUT`]), whether more were
/// dropped, and whether it closed.
#[derive(Default)]
struct Caught {
    bytes: Vec<u8>,
    dropped: bool,
    closed: bool,
}

/// Read `pipe` into a [`Caught`] on a thread of its own.
fn catch(pipe: Option<impl Read + Send + 'static>) -> Arc<Mutex<Caught>> {
    let caught = Arc::new(Mutex::new(Caught::default()));
    let Some(mut pipe) = pipe else {
        caught.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
        return caught;
    };
    let kept = caught.clone();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            let read = match pipe.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let mut caught = kept.lock().unwrap_or_else(PoisonError::into_inner);
            let room = usize::try_from(MAX_OUTPUT).unwrap_or(usize::MAX).saturating_sub(caught.bytes.len());
            caught.dropped |= read > room;
            caught.bytes.extend_from_slice(&buffer[..read.min(room)]);
        }
        kept.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
    });
    caught
}

/// End the process group `leader` leads (Unix), or its process tree
/// (Windows).
fn end_group(leader: u32) {
    if cfg!(windows) {
        let _ = crate::sys::command("taskkill").args(["/T", "/F", "/PID", &leader.to_string()]).status();
    } else {
        let _ = crate::sys::command("kill").args(["-KILL", "--", &format!("-{leader}")]).status();
    }
}

/// Run `command` (configured, not started) in a process group of its own
/// for at most `timeout`. Past it the group ends while its leader still
/// runs (not yet reaped, so its id names no one else's). Once the command
/// ended by itself, what it left in the background stays, and its output
/// is waited for at most [`DRAIN`].
pub fn run(mut command: Command, timeout: Duration) -> Result<Ran, String> {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| format!("cannot start it: {error}"))?;
    let stdout = catch(child.stdout.take());
    let stderr = catch(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                end_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    let drained = Instant::now() + DRAIN;
    let closed = || [&stdout, &stderr].iter().all(|pipe| pipe.lock().unwrap_or_else(PoisonError::into_inner).closed);
    while !closed() && Instant::now() < drained {
        std::thread::sleep(Duration::from_millis(10));
    }
    let take = |pipe: &Arc<Mutex<Caught>>| {
        let caught = pipe.lock().unwrap_or_else(PoisonError::into_inner);
        (String::from_utf8_lossy(&caught.bytes).into_owned(), caught.dropped)
    };
    let ((stdout, out_dropped), (stderr, err_dropped)) = (take(&stdout), take(&stderr));
    Ok(Ran {
        code: status.and_then(|status| status.code()),
        stdout,
        stderr,
        timed_out,
        truncated: out_dropped || err_dropped,
    })
}

/// The system shell running `line`: `sh -c` (Unix), `cmd /C` (Windows,
/// the line passed as it is, so its own quoting holds).
fn shell(line: &str) -> Command {
    if cfg!(windows) {
        let mut command = crate::sys::command("cmd");
        command.arg("/C");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.raw_arg(line);
        }
        command
    } else {
        let mut command = crate::sys::command("/bin/sh");
        command.arg("-c").arg(line);
        command
    }
}

/// A `code.exec` asked for, its params checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The folder it runs in: an absolute path that is a folder.
    pub dir: PathBuf,
    /// The line the shell runs.
    pub command: String,
    /// Folders put first on its `PATH` (an environment's, a toolchain's).
    pub path_first: Vec<PathBuf>,
    pub timeout: Duration,
}

/// `code.exec`'s params, checked: `{dir, command, path_first?: [dir],
/// timeout_secs?}`.
pub fn asked(params: &JsonMap) -> RunnerResult<Asked> {
    let dir = crate::params::host_path(crate::params::text(params, "dir")?)?;
    if !dir.is_dir() {
        return Err(RunnerError::new(INVALID_PARAMS, format!("{} is not a folder", dir.display())));
    }
    let command = crate::params::text(params, "command")?;
    if command.trim().is_empty() {
        return Err(RunnerError::new(INVALID_PARAMS, "command is empty"));
    }
    let path_first = crate::params::strings(params, "path_first")?
        .iter()
        .map(|folder| crate::params::host_path(folder))
        .collect::<RunnerResult<_>>()?;
    let timeout = crate::params::timeout_secs(params, TIMEOUT, MAX_TIMEOUT)?;
    Ok(Asked { dir, command: command.to_string(), path_first, timeout })
}

/// `code.exec`: run what was asked; how it went.
pub fn op(params: &JsonMap) -> RunnerResult<Value> {
    let asked = asked(params)?;
    let mut command = shell(&asked.command);
    command.current_dir(&asked.dir);
    if !asked.path_first.is_empty() {
        let rest = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
        let path = std::env::join_paths(asked.path_first.iter().cloned().chain::<Vec<PathBuf>>(rest))
            .map_err(|error| RunnerError::new(INVALID_PARAMS, format!("path_first: {error}")))?;
        command.env("PATH", path);
    }
    run(command, asked.timeout).map(|ran| ran.to_json()).map_err(|why| RunnerError::new(EXEC_FAILED, why))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// `code.exec`'s params from JSON.
    fn params(value: Value) -> JsonMap {
        value.as_object().cloned().unwrap()
    }

    #[test]
    #[cfg(unix)]
    fn the_exit_code_and_both_outputs_come_back_apart() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("here.txt"), "").unwrap();
        let dir = folder.path().to_string_lossy();
        let answer = op(&params(json!({"dir": dir, "command": "ls; echo said >&2; exit 3"}))).unwrap();
        assert_eq!(answer["code"], 3);
        assert_eq!((answer["stdout"].as_str(), answer["stderr"].as_str()), (Some("here.txt\n"), Some("said\n")));
        assert_eq!((answer["timed_out"].as_bool(), answer["truncated"].as_bool()), (Some(false), Some(false)));
    }

    #[test]
    #[cfg(unix)]
    fn folders_put_first_on_the_path_are_found_first() {
        use std::os::unix::fs::PermissionsExt;
        let folder = tempfile::tempdir().unwrap();
        let tool = folder.path().join("homology-test-tool");
        std::fs::write(&tool, "#!/bin/sh\necho from-the-tool\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = folder.path().to_string_lossy();
        let answer =
            op(&params(json!({"dir": dir, "command": "homology-test-tool", "path_first": [dir.clone()]}))).unwrap();
        assert_eq!((answer["code"].as_i64(), answer["stdout"].as_str()), (Some(0), Some("from-the-tool\n")));
    }

    #[test]
    fn what_cannot_run_is_refused_by_its_params() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().to_string_lossy().to_string();
        for bad in [
            json!({"command": "true"}),
            json!({"dir": "relative", "command": "true"}),
            json!({"dir": folder.path().join("nowhere").to_string_lossy(), "command": "true"}),
            json!({"dir": dir, "command": "  "}),
            json!({"dir": dir, "command": "true", "timeout_secs": 0}),
            json!({"dir": dir, "command": "true", "timeout_secs": 601}),
            json!({"dir": dir, "command": "true", "path_first": ["bin"]}),
        ] {
            assert_eq!(asked(&params(bad.clone())).unwrap_err().code, INVALID_PARAMS, "{bad}");
        }
    }

    /// Whether a process with exactly `arguments` runs (not a zombie).
    #[cfg(target_os = "linux")]
    fn running(arguments: &[&str]) -> bool {
        let wanted: Vec<u8> = arguments.iter().flat_map(|argument| [argument.as_bytes(), b"\0"].concat()).collect();
        std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
            let path = entry.path();
            let zombie = std::fs::read_to_string(path.join("stat"))
                .is_ok_and(|stat| stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')));
            std::fs::read(path.join("cmdline")).is_ok_and(|line| line == wanted) && !zombie
        })
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn past_its_time_a_command_ends_with_what_it_started() {
        let folder = tempfile::tempdir().unwrap();
        let mut command = shell("echo started; sleep 7401 & sleep 7402");
        command.current_dir(folder.path());
        let ran = run(command, Duration::from_secs(1)).unwrap();
        assert!(ran.timed_out && ran.code.is_none(), "{ran:?}");
        assert_eq!(ran.stdout, "started\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while (running(&["sleep", "7401"]) || running(&["sleep", "7402"])) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!running(&["sleep", "7401"]) && !running(&["sleep", "7402"]), "a sleep was left running");
    }

    #[test]
    #[cfg(unix)]
    fn output_past_the_limit_is_dropped_and_said() {
        let folder = tempfile::tempdir().unwrap();
        let mut command = shell(&format!("head -c {} /dev/zero", MAX_OUTPUT + 10));
        command.current_dir(folder.path());
        let ran = run(command, Duration::from_secs(60)).unwrap();
        assert_eq!((ran.code, ran.truncated, ran.stdout.len() as u64), (Some(0), true, MAX_OUTPUT));
    }
}
