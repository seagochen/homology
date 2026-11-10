//! Consoles: a shell (bash, else sh; cmd on Windows) with one of the user's
//! Python environments first on its PATH, or that environment's interactive
//! Python, on this computer or on an SSH host, started in a folder of the
//! caller's choosing. Line by line, without a terminal: what is sent is
//! written to its stdin (and to the transcript, as typed, where nothing
//! echoes it); what it prints, prompts included, is read on threads of
//! their own into a transcript kept to its last [`KEEP`] bytes, read back
//! by offset. Interrupting sends SIGINT to its process group, so a command
//! the shell runs stops and the shell stays (Unix; on a host through
//! another `ssh`).
//!
//! [`Consoles`] are the caller's session's: they end when it drops them (or
//! when stopped), and with each its whole process group, so what the shell
//! runs ends too (#4). Whether a console may be started at all, and by whom, is
//! the caller's to decide: it runs whatever is typed into it.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use homology_ssh::{shell_quote, Context, SshError, SshTarget};

/// The most of a console's transcript kept.
pub const KEEP: usize = 1024 * 1024;
/// The line a host's shell prints first: the console's process id.
const PID_MARK: &str = "__homology_console_pid ";
/// What an interactive bash without a terminal says once; left out.
const NO_TERMINAL: [&str; 2] = ["cannot set terminal process group", "no job control in this shell"];

/// A console could not be started, or its process could not be spoken to.
pub const CONSOLE_FAILED: &str = "console_failed";
/// No open console has that id (it was stopped, or the caller restarted).
pub const CONSOLE_NOT_FOUND: &str = "console_not_found";
/// The console's process has ended; it takes no more input.
pub const CONSOLE_ENDED: &str = "console_ended";
/// Interrupting is not possible: on Windows, or before the process is known.
pub const CONSOLE_INTERRUPT_UNAVAILABLE: &str = "console_interrupt_unavailable";

/// Why a console call failed. [`ConsoleError::code`] is stable; the
/// message is for people.
#[derive(Debug)]
pub enum ConsoleError {
    /// [`CONSOLE_FAILED`], with what went wrong.
    Failed(String),
    /// [`CONSOLE_NOT_FOUND`], with the id asked for.
    NotFound(String),
    /// [`CONSOLE_ENDED`].
    Ended,
    /// [`CONSOLE_INTERRUPT_UNAVAILABLE`], with why.
    InterruptUnavailable(&'static str),
    /// Reaching the host failed, under the SSH layer's own code.
    Ssh(SshError),
}

impl ConsoleError {
    /// The stable code: one of this module's, or the SSH layer's.
    pub fn code(&self) -> &'static str {
        match self {
            ConsoleError::Failed(_) => CONSOLE_FAILED,
            ConsoleError::NotFound(_) => CONSOLE_NOT_FOUND,
            ConsoleError::Ended => CONSOLE_ENDED,
            ConsoleError::InterruptUnavailable(_) => CONSOLE_INTERRUPT_UNAVAILABLE,
            ConsoleError::Ssh(error) => error.code,
        }
    }
}

impl fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConsoleError::Failed(message) => f.write_str(message),
            ConsoleError::NotFound(id) => write!(f, "no console {id:?} (it was stopped, or the program restarted)"),
            ConsoleError::Ended => f.write_str("the console has ended; start it again"),
            ConsoleError::InterruptUnavailable(why) => f.write_str(why),
            ConsoleError::Ssh(error) => f.write_str(&error.message),
        }
    }
}

impl std::error::Error for ConsoleError {}

impl From<SshError> for ConsoleError {
    fn from(error: SshError) -> Self {
        ConsoleError::Ssh(error)
    }
}

/// What a console runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A shell, the environment first on its PATH.
    Shell,
    /// The environment's interactive Python.
    Python,
}

/// Where a console runs.
pub enum Machine {
    /// This computer.
    Local,
    /// The host `target` names (its path is not used), through the user's
    /// own SSH with the caller's settings: any session type of the
    /// caller's that holds Homology's [`Context`].
    Ssh { cx: Arc<dyn AsRef<Context> + Send + Sync>, target: SshTarget },
}

/// What a console printed after an offset ([`Consoles::read`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    /// The text after the offset asked for (from what is kept).
    pub text: String,
    /// The offset to read on from.
    pub offset: u64,
    /// Whether its process still runs.
    pub running: bool,
    /// How it ended (its exit status, as the system says it), once it did.
    pub ended: Option<String>,
}

/// The folder an interpreter is in (its environment's `bin`), paths as
/// the host writes them.
fn bin_of(python: &str) -> Option<&str> {
    python.rsplit_once(['/', '\\']).map(|(dir, _)| dir).filter(|dir| !dir.is_empty())
}

/// Lock `mutex`, going on with what a panicked holder left.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a console printed, its last `KEEP` bytes, and how it ended.
#[derive(Default)]
struct Transcript {
    text: String,
    /// Bytes dropped from the start (offsets count from the very first).
    dropped: u64,
    /// How it ended, once it did.
    ended: Option<String>,
}

impl Transcript {
    /// Add `text`, dropping the oldest beyond `KEEP` at a character boundary.
    fn push(&mut self, text: &str) {
        self.text.push_str(text);
        if self.text.len() > KEEP {
            let mut cut = self.text.len() - KEEP;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut as u64;
        }
    }

    /// What came after `offset`, and the offset after it.
    fn since(&self, offset: u64) -> (String, u64) {
        let end = self.dropped + self.text.len() as u64;
        let from = offset.clamp(self.dropped, end) - self.dropped;
        let mut from = usize::try_from(from).unwrap_or(0);
        while from < self.text.len() && !self.text.is_char_boundary(from) {
            from += 1;
        }
        (self.text[from..].to_string(), end)
    }
}

/// One open console: its process (or the `ssh` running it), its input and
/// what it printed.
struct Console {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    transcript: Arc<Mutex<Transcript>>,
    /// The process id of what it runs, where it runs.
    pid: Arc<Mutex<Option<u32>>>,
    machine: Machine,
    kind: Kind,
}

impl Console {
    /// End the console and everything in its process group, then reap it.
    /// Its group is signalled only while its process has not been reaped
    /// (no [`Transcript::ended`] yet): until then its id cannot have been
    /// given to another process. A shell that exited by itself earlier
    /// leaves what it put in the background.
    fn end(&self) {
        let pid = *lock(&self.pid);
        let reaped = lock(&self.transcript).ended.is_some();
        if let (Some(pid), false) = (pid, reaped) {
            match &self.machine {
                Machine::Local => end_local_group(pid),
                Machine::Ssh { cx, target } => {
                    // Over the network: not on the caller's thread.
                    let (cx, target) = (cx.clone(), target.clone());
                    std::thread::spawn(move || {
                        let script = format!("kill -KILL -- -{pid} 2>/dev/null; kill -KILL {pid} 2>/dev/null; true");
                        let _ = homology_ssh::run((*cx).as_ref(), &target, &script);
                    });
                }
            }
        }
        let mut child = lock(&self.child);
        let _ = child.kill();
        if let Ok(status) = child.wait() {
            lock(&self.transcript).ended.get_or_insert(status.to_string());
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.end();
    }
}

/// Send `signal` (a `kill` name: `INT`, `KILL`) to the process group `pid`
/// leads on this computer (Unix).
fn signal_local_group(pid: u32, signal: &str) -> std::io::Result<std::process::ExitStatus> {
    homology_sys::command("kill").arg(format!("-{signal}")).arg("--").arg(format!("-{pid}")).status()
}

/// End the console `pid` on this computer with all it runs: its process
/// group (Unix; the console leads it, [`local_command`]) or its process
/// tree (Windows).
fn end_local_group(pid: u32) {
    if cfg!(windows) {
        let _ = homology_sys::command("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).status();
    } else {
        let _ = signal_local_group(pid, "KILL");
    }
}

/// The consoles open in one session of the caller's, by id.
#[derive(Default)]
pub struct Consoles {
    open: Mutex<HashMap<String, Arc<Console>>>,
}

/// The command that runs a console of `kind` on this computer, in `dir`.
fn local_command(kind: Kind, python: &str, dir: Option<&str>) -> Command {
    let mut command = match kind {
        Kind::Python => {
            let mut command = homology_sys::command(python);
            command.args(["-u", "-i", "-q"]);
            command
        }
        Kind::Shell if cfg!(windows) => {
            let mut command = homology_sys::command("cmd");
            command.args(["/Q", "/K"]).env("PROMPT", "$P$G ");
            command
        }
        Kind::Shell => {
            let shell = if std::path::Path::new("/bin/bash").exists() { "/bin/bash" } else { "/bin/sh" };
            let mut command = homology_sys::command(shell);
            if shell.ends_with("bash") {
                command.args(["--noprofile", "--norc"]);
            }
            command.arg("-i").env("PS1", "\\W $ ").env("TERM", "dumb");
            command
        }
    };
    if kind == Kind::Shell {
        if let Some(bin) = bin_of(python) {
            let separator = if cfg!(windows) { ";" } else { ":" };
            let path = std::env::var("PATH").unwrap_or_default();
            command.env("PATH", format!("{bin}{separator}{path}"));
        }
    }
    // Its own process group: an interrupt reaches what the shell runs.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    command
}

/// `dir` quoted for a POSIX shell, a leading `~` kept outside the quotes
/// so the shell expands it (as `SshTarget::quoted_path` does): a folder in
/// the home is written `~/…` (#10).
fn quoted_dir(dir: &str) -> String {
    match dir.strip_prefix('~') {
        Some("") => "~".into(),
        Some(rest) if rest.starts_with('/') => format!("~/{}", shell_quote(rest.trim_start_matches('/'))),
        _ => shell_quote(dir),
    }
}

/// The script that runs a console of `kind` on a host, in `dir` when it is
/// there: it prints its process id first ([`PID_MARK`]).
fn host_script(kind: Kind, python: &str, dir: Option<&str>) -> String {
    let place = dir.map(|dir| format!("cd {} 2>/dev/null; ", quoted_dir(dir))).unwrap_or_default();
    let run = match kind {
        Kind::Python => format!("exec {} -u -i -q", shell_quote(python)),
        Kind::Shell => {
            let path = bin_of(python).map(|bin| format!("PATH={}:\"$PATH\"; ", shell_quote(bin))).unwrap_or_default();
            format!(
                "{path}export PATH TERM=dumb PS1='\\W $ '; if command -v bash >/dev/null; then exec bash --noprofile --norc -i; else exec sh -i; fi"
            )
        }
    };
    format!("{place}echo \"{PID_MARK}$$\"; export PYTHONUNBUFFERED=1 PYTHONIOENCODING=utf-8; {run}")
}

/// Read `pipe` into `transcript` until it closes: whole characters only,
/// bash's complaint about having no terminal left out, and (`first`) a
/// host's first line taken as the process id.
fn read_into(mut pipe: impl Read, transcript: &Mutex<Transcript>, pid: &Mutex<Option<u32>>, mut first: bool) {
    let mut buffer = [0u8; 8192];
    let mut pending = Vec::new();
    loop {
        let read = match pipe.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        pending.extend_from_slice(&buffer[..read]);
        if first {
            let Some(end) = pending.iter().position(|byte| *byte == b'\n') else { continue };
            let line = String::from_utf8_lossy(&pending[..end]).to_string();
            if let Some(id) = line.trim().strip_prefix(PID_MARK.trim_end()).map(str::trim) {
                *lock(pid) = id.parse().ok();
                pending.drain(..=end);
            }
            first = false;
        }
        // Whole characters only; the rest waits for the next read.
        let valid = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(error) => error.valid_up_to(),
        };
        let mut text = String::from_utf8_lossy(&pending[..valid]).to_string();
        pending.drain(..valid);
        if NO_TERMINAL.iter().any(|said| text.contains(said)) {
            text = text
                .lines()
                .filter(|line| !NO_TERMINAL.iter().any(|said| line.contains(said)))
                .map(|line| format!("{line}\n"))
                .collect();
        }
        lock(transcript).push(&text);
    }
}

impl Consoles {
    /// Start a console of `kind` for the environment of the interpreter
    /// `python` (its folder first on a shell's PATH), in `dir` on
    /// `machine`; its id.
    pub fn start(&self, machine: Machine, kind: Kind, python: &str, dir: Option<&str>) -> Result<String, ConsoleError> {
        let mut command = match &machine {
            Machine::Local => local_command(kind, python, dir),
            Machine::Ssh { cx, target } => {
                homology_ssh::session_command((**cx).as_ref(), target, &host_script(kind, python, dir))?
            }
        };
        command
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child =
            command.spawn().map_err(|error| ConsoleError::Failed(format!("cannot start {python}: {error}")))?;
        let transcript = Arc::new(Mutex::new(Transcript::default()));
        let local = matches!(machine, Machine::Local);
        let pid = Arc::new(Mutex::new(local.then(|| child.id())));
        if let Some(pipe) = child.stdout.take() {
            let (transcript, pid) = (transcript.clone(), pid.clone());
            std::thread::spawn(move || read_into(pipe, &transcript, &pid, !local));
        }
        if let Some(pipe) = child.stderr.take() {
            let (transcript, pid) = (transcript.clone(), pid.clone());
            std::thread::spawn(move || read_into(pipe, &transcript, &pid, false));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let stdin = child.stdin.take();
        lock(&self.open).insert(
            id.clone(),
            Arc::new(Console { child: Mutex::new(child), stdin: Mutex::new(stdin), transcript, pid, machine, kind }),
        );
        Ok(id)
    }

    /// The open console `id`.
    fn get(&self, id: &str) -> Result<Arc<Console>, ConsoleError> {
        lock(&self.open).get(id).cloned().ok_or_else(|| ConsoleError::NotFound(id.to_string()))
    }

    /// Send a line. Python without a terminal does not echo it: it is
    /// written to the transcript as typed (an interactive bash echoes it).
    pub fn send(&self, id: &str, line: &str) -> Result<(), ConsoleError> {
        let console = self.get(id)?;
        if console.kind == Kind::Python {
            lock(&console.transcript).push(&format!("{line}\n"));
        }
        let mut stdin = lock(&console.stdin);
        let pipe = stdin.as_mut().ok_or(ConsoleError::Ended)?;
        pipe.write_all(format!("{line}\n").as_bytes()).and_then(|()| pipe.flush()).map_err(|_| ConsoleError::Ended)
    }

    /// What it printed after `offset`, and whether it still runs.
    pub fn read(&self, id: &str, offset: u64) -> Result<Output, ConsoleError> {
        let console = self.get(id)?;
        let ended = lock(&console.child).try_wait().ok().flatten().map(|status| status.to_string());
        let mut transcript = lock(&console.transcript);
        if transcript.ended.is_none() {
            transcript.ended = ended;
        }
        let (text, offset) = transcript.since(offset);
        Ok(Output { text, offset, running: transcript.ended.is_none(), ended: transcript.ended.clone() })
    }

    /// Ctrl+C: SIGINT to the console's process group (Unix hosts): what the
    /// shell runs stops, an interactive shell or Python carries on.
    pub fn interrupt(&self, id: &str) -> Result<(), ConsoleError> {
        let console = self.get(id)?;
        let pid = (*lock(&console.pid))
            .ok_or(ConsoleError::InterruptUnavailable("the console's process is not known yet"))?;
        match &console.machine {
            Machine::Local if cfg!(unix) => {
                signal_local_group(pid, "INT")
                    .map_err(|error| ConsoleError::Failed(format!("cannot interrupt: {error}")))?;
            }
            Machine::Local => {
                return Err(ConsoleError::InterruptUnavailable(
                    "interrupting is for Unix hosts; stop the console and start it again",
                ))
            }
            Machine::Ssh { cx, target } => {
                let script = format!(
                    "group=$(ps -o pgid= -p {pid} | tr -d ' '); kill -INT -- \"-${{group:-{pid}}}\" 2>/dev/null || kill -INT {pid}"
                );
                homology_ssh::run((**cx).as_ref(), target, &script)?;
            }
        }
        Ok(())
    }

    /// End it, with what it runs.
    pub fn stop(&self, id: &str) -> Result<(), ConsoleError> {
        let console = lock(&self.open).remove(id).ok_or_else(|| ConsoleError::NotFound(id.to_string()))?;
        console.end();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn the_transcript_keeps_its_end_and_reads_on_by_offset() {
        let mut transcript = Transcript::default();
        transcript.push(">>> ");
        let (text, offset) = transcript.since(0);
        assert_eq!((text.as_str(), offset), (">>> ", 4));
        transcript.push("print(1)\n1\n");
        assert_eq!(transcript.since(offset).0, "print(1)\n1\n");
        transcript.push(&"字".repeat(KEEP));
        assert!(transcript.text.len() <= KEEP && transcript.dropped > 0);
        let (rest, _) = transcript.since(0);
        assert!(rest.starts_with('字'), "an offset already dropped reads from what is kept");
    }

    /// What `consoles` prints for `id` until `until` appears (ten seconds at most).
    fn read_until(consoles: &Consoles, id: &str, until: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = String::new();
        let mut offset = 0;
        while !seen.contains(until) && std::time::Instant::now() < deadline {
            let read = consoles.read(id, offset).unwrap();
            seen.push_str(&read.text);
            offset = read.offset;
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        seen
    }

    /// A real Python on this computer: a line sent, its answer read back.
    #[test]
    fn a_local_python_answers_a_line() {
        let python = ["python3", "python"].into_iter().find(|name| {
            std::process::Command::new(name).arg("-c").arg("pass").status().is_ok_and(|status| status.success())
        });
        let Some(python) = python else { return };
        let consoles = Consoles::default();
        let id = consoles.start(Machine::Local, Kind::Python, python, None).unwrap();
        consoles.send(&id, "print(6 * 7)").unwrap();
        let seen = read_until(&consoles, &id, "42");
        assert!(seen.contains("print(6 * 7)") && seen.contains("42"), "{seen:?}");
        consoles.stop(&id).unwrap();
        assert_eq!(consoles.read(&id, 0).unwrap_err().code(), CONSOLE_NOT_FOUND);
    }

    /// A shell on this computer (Unix): a command run in the folder asked
    /// for, the environment's `bin` first on its PATH, the shell's own
    /// complaint about having no terminal left out; a long command
    /// interrupted, the shell carrying on.
    #[test]
    #[cfg(unix)]
    fn a_local_shell_runs_a_command_in_the_folder() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("marker.txt"), "").unwrap();
        let consoles = Consoles::default();
        let id = consoles
            .start(Machine::Local, Kind::Shell, "/opt/env/bin/python", Some(&folder.path().to_string_lossy()))
            .unwrap();
        consoles.send(&id, "ls; echo \"path=$PATH\"; echo done-$((6*7))").unwrap();
        let seen = read_until(&consoles, &id, "done-42");
        assert!(seen.contains("marker.txt") && seen.contains("path=/opt/env/bin:"), "{seen:?}");
        assert!(!seen.contains("no job control"), "{seen:?}");
        consoles.send(&id, "sleep 30; echo slept").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        consoles.interrupt(&id).unwrap();
        consoles.send(&id, "echo after-$((2*3))").unwrap();
        let seen = read_until(&consoles, &id, "after-6");
        assert!(seen.contains("after-6") && !seen.lines().any(|line| line == "slept"), "{seen:?}");
        consoles.stop(&id).unwrap();
        assert_eq!(bin_of("/home/u/env/bin/python"), Some("/home/u/env/bin"));
    }

    /// Whether a process with exactly the arguments `arguments` runs (not
    /// a zombie), from `/proc`.
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

    /// Wait up to five seconds for `done`.
    #[cfg(target_os = "linux")]
    fn eventually(done: impl Fn() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        done()
    }

    /// A command the shell runs ends with the console, stopped or dropped,
    /// and the console's own process is reaped (#4).
    #[test]
    #[cfg(target_os = "linux")]
    fn a_console_ended_leaves_nothing_running() {
        for (seconds, dropped) in [("7391", false), ("7392", true)] {
            let consoles = Consoles::default();
            let id = consoles.start(Machine::Local, Kind::Shell, "python3", None).unwrap();
            consoles.send(&id, &format!("sleep {seconds}")).unwrap();
            assert!(eventually(|| running(&["sleep", seconds])), "sleep {seconds} did not start");
            let shell = lock(&consoles.get(&id).unwrap().pid).unwrap();
            if dropped {
                drop(consoles);
            } else {
                consoles.stop(&id).unwrap();
            }
            assert!(eventually(|| !running(&["sleep", seconds])), "sleep {seconds} left running");
            // Reaped: not even a zombie is left.
            assert!(!std::path::Path::new(&format!("/proc/{shell}")).exists(), "the shell {shell} not reaped");
        }
    }

    /// A folder in the home keeps its `~` for the shell; others are quoted whole.
    #[test]
    fn a_folder_in_the_home_is_gone_to_by_its_tilde() {
        assert_eq!(quoted_dir("~"), "~");
        assert_eq!(quoted_dir("~/work"), "~/'work'");
        assert_eq!(quoted_dir("~/my 'dir'"), r"~/'my '\''dir'\'''");
        assert_eq!(quoted_dir("/srv/a b"), "'/srv/a b'");
        // `~user` is not the home: quoted as it is.
        assert_eq!(quoted_dir("~other/x"), "'~other/x'");
        assert!(host_script(Kind::Shell, "/env/bin/python", Some("~/work")).starts_with("cd ~/'work' 2>/dev/null; "));
    }

    /// A host's console prints its process id first and runs in the folder.
    #[test]
    fn a_host_script_marks_its_process_and_goes_to_the_folder() {
        let script = host_script(Kind::Python, "/srv/env/bin/python", Some("/srv/model dir"));
        assert!(script.starts_with("cd '/srv/model dir' 2>/dev/null; echo \"__homology_console_pid $$\"; "));
        assert!(script.ends_with("exec '/srv/env/bin/python' -u -i -q"), "{script}");
        let shell = host_script(Kind::Shell, "/srv/env/bin/python", None);
        assert!(shell.contains("PATH='/srv/env/bin':\"$PATH\"; ") && shell.contains("exec bash --noprofile"));
    }

    /// Calls on a console that is not open, and their codes.
    #[test]
    fn unknown_consoles_say_so() {
        let consoles = Consoles::default();
        for error in [
            consoles.send("nope", "x").unwrap_err(),
            consoles.read("nope", 0).map(|_| ()).unwrap_err(),
            consoles.interrupt("nope").unwrap_err(),
            consoles.stop("nope").unwrap_err(),
        ] {
            assert_eq!(error.code(), CONSOLE_NOT_FOUND);
        }
        assert_eq!(ConsoleError::Ended.code(), CONSOLE_ENDED);
    }
}
