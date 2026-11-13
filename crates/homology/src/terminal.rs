//! Interactive PTY sessions. Local shells and system SSH share the same byte transport.
//! Login shells load the same environment as a normal terminal, including CLI installations.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::console::Machine;

/// The transcript is bounded independently of the UI's terminal scrollback.
const KEEP: usize = 1024 * 1024;

/// Lock shared state even after a worker panicked.
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Output retained as complete UTF-8 characters; ANSI control bytes remain intact.
#[derive(Default)]
struct Transcript {
    text: String,
    dropped: u64,
}

impl Transcript {
    /// Append complete text, dropping only at character boundaries.
    fn append(&mut self, text: &str) {
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
}

/// A PTY's incremental output, suitable for an ANSI/VT terminal emulator.
pub struct Output {
    pub text: String,
    pub offset: u64,
    pub running: bool,
}

/// An owned process, its controlling terminal and bounded output.
struct Session {
    master: Mutex<Box<dyn MasterPty + Send>>,
    input: mpsc::Sender<Vec<u8>>,
    writer_error: Arc<Mutex<Option<String>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    transcript: Arc<Mutex<Transcript>>,
}

impl Drop for Session {
    /// End the process tree while its leader's pid still belongs to this session.
    fn drop(&mut self) {
        let mut child = lock(&self.child);
        if matches!(child.try_wait(), Ok(None)) {
            if let Some(pid) = child.process_id() {
                homology_sys::end_group(pid);
            }
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

/// PTY sessions owned by one application, ended when it drops them.
#[derive(Default)]
pub struct Terminals {
    next: AtomicU64,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Terminals {
    /// Start a login shell locally or through the caller's already-trusted SSH context.
    pub fn start(
        &self,
        machine: Machine,
        folder: Option<&str>,
        environment: &str,
        rows: u16,
        columns: u16,
    ) -> Result<String, String> {
        let mut command = match machine {
            Machine::Local => {
                let shell = if cfg!(windows) {
                    "powershell.exe".to_string()
                } else {
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
                };
                let mut command = homology_sys::command(shell);
                if cfg!(windows) {
                    command.arg("-NoLogo");
                } else {
                    command.arg("-il");
                }
                if let Some(folder) = folder {
                    command.current_dir(folder);
                }
                if !environment.is_empty() {
                    if let Some(bin) = std::path::Path::new(environment).parent() {
                        let separator = if cfg!(windows) { ";" } else { ":" };
                        command.env(
                            "PATH",
                            format!("{}{separator}{}", bin.display(), std::env::var("PATH").unwrap_or_default()),
                        );
                    }
                }
                Self::command(command, false)
            }
            Machine::Ssh { cx, target } => {
                let directory = folder
                    .map(|folder| format!("cd {} || exit; ", homology_ssh::quoted_dir(folder)))
                    .unwrap_or_default();
                let path = environment
                    .rsplit_once('/')
                    .map(|(bin, _)| format!("export PATH={}:\"$PATH\"; ", homology_ssh::shell_quote(bin)))
                    .unwrap_or_default();
                let script = format!("{directory}{path}export TERM=xterm-256color; exec \"${{SHELL:-/bin/sh}}\" -il");
                let command = homology_ssh::session_command((*cx).as_ref(), &target, &script)
                    .map_err(|error| error.to_string())?;
                Self::command(command, true)
            }
        };
        command.env("TERM", "xterm-256color");
        self.spawn(command, rows, columns)
    }

    /// Convert an authenticated system command, preserving its environment and exact arguments.
    fn command(command: std::process::Command, remote: bool) -> CommandBuilder {
        let mut builder = CommandBuilder::new(command.get_program());
        if remote {
            builder.arg("-tt");
        }
        builder.args(command.get_args());
        for (name, value) in command.get_envs() {
            if let Some(value) = value {
                builder.env(name, value);
            } else {
                builder.env_remove(name);
            }
        }
        if let Some(folder) = command.get_current_dir() {
            builder.cwd(folder);
        }
        builder
    }

    /// Attach a process to its own PTY and read it without blocking application frames.
    fn spawn(&self, command: CommandBuilder, rows: u16, columns: u16) -> Result<String, String> {
        let pair = portable_pty::native_pty_system().openpty(size(rows, columns)).map_err(|error| error.to_string())?;
        let mut child = pair.slave.spawn_command(command).map_err(|error| error.to_string())?;
        drop(pair.slave);
        let io = (|| {
            let reader = pair.master.try_clone_reader().map_err(|error| error.to_string())?;
            let writer = pair.master.take_writer().map_err(|error| error.to_string())?;
            Ok::<_, String>((reader, writer))
        })();
        let (reader, mut writer) = match io {
            Ok(io) => io,
            Err(error) => {
                if let Some(pid) = child.process_id() {
                    homology_sys::end_group(pid);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let (input, writes) = mpsc::channel::<Vec<u8>>();
        let writer_error = Arc::new(Mutex::new(None));
        let failed = writer_error.clone();
        std::thread::spawn(move || {
            for bytes in writes {
                if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    *lock(&failed) = Some(error.to_string());
                    break;
                }
            }
        });
        let transcript = Arc::new(Mutex::new(Transcript::default()));
        let into = transcript.clone();
        std::thread::spawn(move || read(reader, &into));
        let session =
            Session { master: Mutex::new(pair.master), input, writer_error, child: Mutex::new(child), transcript };
        let id = format!("pty:{}", self.next.fetch_add(1, Ordering::Relaxed));
        lock(&self.sessions).insert(id.clone(), Arc::new(session));
        Ok(id)
    }

    /// Resolve one session without retaining the registry lock during IO.
    fn get(&self, id: &str) -> Result<Arc<Session>, String> {
        lock(&self.sessions).get(id).cloned().ok_or_else(|| "Terminal is closed".into())
    }

    /// Write exact bytes; no newline, echo or local line discipline is added.
    pub fn send(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        let session = self.get(id)?;
        session.input.send(bytes.to_vec()).map_err(|_| "Terminal input is closed".to_string())
    }

    /// Notify the kernel and SSH client of terminal dimensions (SIGWINCH).
    pub fn resize(&self, id: &str, rows: u16, columns: u16) -> Result<(), String> {
        lock(&self.get(id)?.master).resize(size(rows, columns)).map_err(|error| error.to_string())
    }

    /// Read output after an absolute byte offset.
    pub fn read(&self, id: &str, offset: u64) -> Result<Output, String> {
        let session = self.get(id)?;
        if let Some(error) = lock(&session.writer_error).clone() {
            return Err(error);
        }
        let running = lock(&session.child).try_wait().map_err(|error| error.to_string())?.is_none();
        let transcript = lock(&session.transcript);
        let start = offset.saturating_sub(transcript.dropped) as usize;
        Ok(Output {
            text: transcript.text.get(start..).unwrap_or_default().to_string(),
            offset: transcript.dropped + transcript.text.len() as u64,
            running,
        })
    }

    /// Stop and reap a session; dropping the registry also stops every session.
    pub fn stop(&self, id: &str) {
        let removed = lock(&self.sessions).remove(id);
        drop(removed);
    }
}

/// A nonzero terminal size, including when a window is minimized.
fn size(rows: u16, columns: u16) -> PtySize {
    PtySize { rows: rows.max(2), cols: columns.max(2), pixel_width: 0, pixel_height: 0 }
}

/// Preserve split Unicode and terminal control sequences across read boundaries.
fn read(mut reader: impl Read, transcript: &Mutex<Transcript>) {
    let mut buffer = [0u8; 8192];
    let mut pending = Vec::new();
    while let Ok(count) = reader.read(&mut buffer) {
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..count]);
        loop {
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    lock(transcript).append(text);
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    lock(transcript).append(&String::from_utf8_lossy(&pending[..valid]));
                    pending.drain(..valid);
                    if let Some(length) = error.error_len() {
                        lock(transcript).append("�");
                        pending.drain(..length);
                    } else {
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
// PTY integration fixtures must fail loudly instead of hiding a missing Python or bad terminal.
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Wait for a marker without allowing an interactive regression to hang the test runner.
    fn until(terminals: &Terminals, id: &str, marker: &str) -> String {
        let started = Instant::now();
        loop {
            let output = terminals.read(id, 0).unwrap();
            if output.text.contains(marker) {
                return output.text;
            }
            assert!(started.elapsed() < Duration::from_secs(5), "missing {marker}: {}", output.text);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn real_pty_supports_raw_input_and_window_size() {
        let terminals = Terminals::default();
        let mut command = CommandBuilder::new("python3");
        command.args(["-u", "-c", "import os,sys,tty,fcntl,termios,struct; print('TTY',os.isatty(0),os.isatty(1),flush=True); tty.setraw(0); print('READY',flush=True); byte=os.read(0,1); rows,cols,_,_=struct.unpack('HHHH',fcntl.ioctl(0,termios.TIOCGWINSZ,bytes(8))); print('RAW',byte.decode(),rows,cols,flush=True)"]);
        let id = terminals.spawn(command, 24, 80).unwrap();
        let initial = until(&terminals, &id, "READY");
        assert!(initial.contains("TTY True True"));
        terminals.resize(&id, 40, 120).unwrap();
        terminals.send(&id, b"x").unwrap();
        assert!(until(&terminals, &id, "RAW").contains("RAW x 40 120"));
        terminals.stop(&id);
        assert!(terminals.read(&id, 0).is_err());
    }

    #[test]
    fn ssh_pty_preserves_options_and_forces_remote_terminal_allocation() {
        let mut command = std::process::Command::new("ssh");
        command.args(["-F", "/config with spaces", "-p", "2222", "--", "me@server", "exec shell"]);
        let builder = Terminals::command(command, true);
        let arguments: Vec<_> = builder.get_argv().iter().map(|argument| argument.to_string_lossy()).collect();
        assert_eq!(
            arguments,
            ["ssh", "-tt", "-F", "/config with spaces", "-p", "2222", "--", "me@server", "exec shell"]
        );
    }

    #[test]
    fn unicode_output_survives_byte_by_byte_reads() {
        /// A reader that deliberately splits UTF-8 characters across every call.
        struct Bytes(std::io::Cursor<Vec<u8>>);
        impl Read for Bytes {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(&mut buffer[..1])
            }
        }
        let transcript = Mutex::new(Transcript::default());
        read(Bytes(std::io::Cursor::new("中文\x1b[31m内容".as_bytes().to_vec())), &transcript);
        assert_eq!(lock(&transcript).text, "中文\x1b[31m内容");
    }
}
