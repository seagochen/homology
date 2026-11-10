//! Handing a password to the system `ssh` without a terminal.
//!
//! `ssh` asks its `SSH_ASKPASS` program for a password and reads the answer
//! from that program's stdout. The program is the one using this crate
//! ([`crate::App`]): started with `<ENV>_ASKPASS` set, it connects to the
//! channel named there, presents the one-time token beside it and prints
//! what comes back ([`client_main`]). The password itself is never in an argument, an
//! environment variable or a file: it lives in this process, behind a
//! listener that answers once, to the holder of the token, and closes when
//! the `ssh` run ends.
//!
//! The channel is a Unix socket only this user can open, in this process's
//! temporary directory ([`homology_sys::local_socket`]), and on Windows a
//! port on `127.0.0.1`. A sandboxed macOS app may not listen on a port
//! without the `network.server` entitlement, which App Review refuses to an
//! app that is not a network server; the helper `ssh` starts inherits the
//! sandbox and reaches the socket in the container.
//!
//! A sandboxed macOS app cannot use itself: `ssh` inherits the sandbox, and
//! an executable with its own `app-sandbox` entitlement is killed when it
//! starts inside an inherited one. Such a bundle ships a separate
//! `<name>-askpass` beside the app, signed with `app-sandbox` and
//! `inherit` only, and [`program`] prefers it.

use std::io::{BufRead, BufReader, Read, Write};
#[cfg(not(unix))]
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::{App, SshError, SshResult};

/// The variable set on the `ssh` child, `<address> <token>`, the address a
/// socket's path (or, on Windows, `127.0.0.1:<port>`): `<ENV>_ASKPASS`.
pub fn env_name(app: App) -> String {
    app.var("ASKPASS")
}

/// The variable naming the askpass program, `<ENV>_ASKPASS_PROGRAM`; the
/// helper beside this executable, or this executable itself, when unset.
pub fn program_env_name(app: App) -> String {
    app.var("ASKPASS_PROGRAM")
}

/// A password, kept only in memory and overwritten when dropped. It never
/// prints.
pub struct Password(Vec<u8>);

impl Password {
    /// The password the user typed.
    pub fn new(text: &str) -> Self {
        Self(text.as_bytes().to_vec())
    }

    /// Whether nothing was typed.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The password's bytes, for the askpass channel only.
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            *byte = 0;
        }
        std::hint::black_box(&self.0);
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Password(***)")
    }
}

/// The program `ssh` runs as `SSH_ASKPASS` for `app`.
pub fn program(app: App) -> SshResult<std::path::PathBuf> {
    if let Some(program) = std::env::var_os(program_env_name(app)) {
        return Ok(program.into());
    }
    let this = std::env::current_exe()
        .map_err(|error| SshError::new("ssh_failed", format!("cannot locate this program for SSH_ASKPASS: {error}")))?;
    Ok(helper_beside(app, &this).unwrap_or(this))
}

/// `<name>-askpass` in the same directory as `program`, when there is one.
fn helper_beside(app: App, program: &std::path::Path) -> Option<std::path::PathBuf> {
    let helper = program.parent()?.join(format!("{}-askpass{}", app.name, std::env::consts::EXE_SUFFIX));
    helper.is_file().then_some(helper)
}

/// A listener that hands `password` out once, to whoever presents the token.
pub struct Server {
    address: String,
    token: String,
    stop: Arc<AtomicBool>,
    answered: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// The socket's path, removed when the server stops.
    #[cfg(unix)]
    socket: std::path::PathBuf,
}

impl Server {
    /// Listen for the askpass program, on a private socket (on Windows a
    /// loopback port), with a fresh one-time token, until dropped.
    pub fn start(password: &Password) -> SshResult<Self> {
        let failed =
            |error: std::io::Error| SshError::new("ssh_failed", format!("cannot open the askpass channel: {error}"));
        #[cfg(unix)]
        let (listener, socket) = homology_sys::local_socket::bind("ap").map_err(failed)?;
        #[cfg(unix)]
        let address = socket.to_string_lossy().into_owned();
        #[cfg(not(unix))]
        let listener = TcpListener::bind("127.0.0.1:0").map_err(failed)?;
        #[cfg(not(unix))]
        let address = listener.local_addr().map_err(failed)?.to_string();
        let stopped = |error| {
            #[cfg(unix)]
            homology_sys::local_socket::remove(&socket);
            failed(error)
        };
        listener.set_nonblocking(true).map_err(stopped)?;
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let stop = Arc::new(AtomicBool::new(false));
        let answered = Arc::new(AtomicBool::new(false));
        let answer = Password(password.bytes().to_vec());
        let (expected, stopping, handed) = (token.clone(), stop.clone(), answered.clone());
        let accept = move || {
            let (stream, _) = listener.accept()?;
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            Ok(stream)
        };
        let thread = std::thread::spawn(move || serve(accept, &expected, &answer, &stopping, &handed));
        Ok(Self {
            address,
            token,
            stop,
            answered,
            thread: Some(thread),
            #[cfg(unix)]
            socket,
        })
    }

    /// Whether the askpass program collected the password. When `ssh` asked
    /// for one and this is still false, the program never ran or could not
    /// reach this channel: the password was not tried.
    pub fn answered(&self) -> bool {
        self.answered.load(Ordering::SeqCst)
    }

    /// The value of `<ENV>_ASKPASS` for the `ssh` child.
    pub fn env_value(&self) -> String {
        format!("{} {}", self.address, self.token)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        #[cfg(unix)]
        homology_sys::local_socket::remove(&self.socket);
    }
}

/// Answer the first client `accept` gives with the right token, then stop;
/// any other client gets nothing. `accept` does not block: it fails with
/// `WouldBlock` while no client is waiting.
fn serve<S: Read + Write>(
    accept: impl Fn() -> std::io::Result<S>,
    token: &str,
    answer: &Password,
    stop: &AtomicBool,
    answered: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        match accept() {
            Ok(mut stream) => {
                let mut line = String::new();
                let presented = BufReader::new(Read::by_ref(&mut stream).take(256)).read_line(&mut line).is_ok();
                if presented && line.trim_end() == token {
                    let mut reply = answer.bytes().to_vec();
                    reply.push(b'\n');
                    if stream.write_all(&reply).is_ok() {
                        answered.store(true, Ordering::SeqCst);
                    }
                    for byte in reply.iter_mut() {
                        *byte = 0;
                    }
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(_) => return,
        }
    }
}

/// Ask the server named by `value` (`<address> <token>`) for the password.
pub fn fetch(value: &str) -> Option<Vec<u8>> {
    // The token has no spaces; a socket's path might.
    let (address, token) = value.rsplit_once(' ')?;
    #[cfg(unix)]
    let mut stream = UnixStream::connect(address).ok()?;
    #[cfg(not(unix))]
    let mut stream = TcpStream::connect_timeout(&address.parse().ok()?, Duration::from_secs(5)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream.write_all(format!("{token}\n").as_bytes()).ok()?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).ok()?;
    if reply.last() != Some(&b'\n') {
        return None;
    }
    reply.pop();
    Some(reply)
}

/// When this process was started by `ssh` as `app`'s askpass program,
/// answer and return the exit code; `None` otherwise. Call it first in
/// `main`.
pub fn client_main(app: App) -> Option<std::process::ExitCode> {
    let value = std::env::var(env_name(app)).ok()?;
    Some(match fetch(&value) {
        Some(mut password) => {
            password.push(b'\n');
            let mut stdout = std::io::stdout().lock();
            let written = stdout.write_all(&password).and_then(|()| stdout.flush());
            for byte in password.iter_mut() {
                *byte = 0;
            }
            if written.is_ok() {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            }
        }
        None => std::process::ExitCode::FAILURE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_is_handed_out_once_to_the_token_holder() {
        let server = Server::start(&Password::new("s3cret pass")).unwrap();
        let value = server.env_value();
        assert!(!value.contains("s3cret"), "the environment value names the channel only");
        let (address, _) = value.rsplit_once(' ').unwrap();
        assert_eq!(fetch(&format!("{address} wrong-token")), None);
        assert!(!server.answered(), "a wrong token collects nothing");
        assert_eq!(fetch(&value).as_deref(), Some(&b"s3cret pass"[..]));
        assert!(server.answered());
        assert_eq!(fetch(&value), None, "one answer only");
    }

    /// No port is opened: the channel is a socket only this user can open,
    /// gone when the server is.
    #[cfg(unix)]
    #[test]
    fn the_channel_is_a_private_socket_not_a_port() {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        let server = Server::start(&Password::new("pw")).unwrap();
        let value = server.env_value();
        let (address, _) = value.rsplit_once(' ').unwrap();
        let socket = std::path::PathBuf::from(address);
        assert!(socket.is_absolute() && !address.contains("127.0.0.1"), "{address}");
        let metadata = std::fs::metadata(&socket).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        drop(server);
        assert!(!socket.exists(), "the socket is removed with the server");
    }

    #[test]
    fn a_helper_beside_the_program_is_preferred() {
        let temporary = tempfile::tempdir().unwrap();
        let app = App { name: "tool", env: "TOOL" };
        let program = temporary.path().join(format!("tool{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&program, b"").unwrap();
        assert_eq!(helper_beside(app, &program), None);
        let helper = temporary.path().join(format!("tool-askpass{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&helper, b"").unwrap();
        assert_eq!(helper_beside(app, &program), Some(helper));
        assert_eq!((env_name(app), program_env_name(app)), ("TOOL_ASKPASS".into(), "TOOL_ASKPASS_PROGRAM".into()));
    }

    #[test]
    fn passwords_do_not_print() {
        assert_eq!(format!("{:?}", Password::new("hunter2")), "Password(***)");
    }
}
