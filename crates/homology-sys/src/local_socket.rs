//! Unix sockets for a program's own processes to talk over (an askpass
//! helper, a command-line client of a running window) without listening on
//! the network.
//!
//! The macOS App Sandbox lets an app bind a TCP port only with the
//! `network.server` entitlement, which App Review refuses to an app that is
//! not a network server; a Unix socket in the app's own temporary directory
//! (inside its container when sandboxed) needs no entitlement, and the
//! app's other processes reach it there. Each socket's name is random and
//! short: a socket's path may not be longer than the system's `sun_path`,
//! and a container's temporary directory is already long.

use std::hash::{BuildHasher, Hasher};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

/// The longest path a socket may have, in bytes (`sun_path` less its NUL).
pub const MAX_PATH: usize = if cfg!(target_os = "linux") { 107 } else { 103 };

/// A socket listening at a fresh path in this process's temporary
/// directory, named `prefix` and six random hexadecimal digits, readable
/// and writable by this user only. The caller removes the path when done
/// ([`remove`]).
pub fn bind(prefix: &str) -> io::Result<(UnixListener, PathBuf)> {
    bind_in(&std::env::temp_dir(), prefix)
}

/// [`bind`] in `directory`. A path longer than [`MAX_PATH`] is refused with
/// an error that names it and its length, instead of a socket the system
/// would cut short.
pub fn bind_in(directory: &Path, prefix: &str) -> io::Result<(UnixListener, PathBuf)> {
    let mut last = io::Error::new(io::ErrorKind::AddrInUse, "no free socket name");
    for _ in 0..16 {
        let path = directory.join(format!("{prefix}{:06x}", random() & 0xff_ffff));
        let length = path.as_os_str().len();
        if length > MAX_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the socket path {} is {length} bytes, longer than the {MAX_PATH} the system allows",
                    path.display()
                ),
            ));
        }
        match UnixListener::bind(&path) {
            Ok(listener) => {
                if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
                    remove(&path);
                    return Err(error);
                }
                return Ok((listener, path));
            }
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => last = error,
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

/// Remove a socket [`bind`] made, once nothing listens on it.
pub fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// A random number, from the standard library's per-process hash keys and
/// the time.
fn random() -> u64 {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_nanos()),
    );
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn a_socket_is_private_and_reachable() {
        let directory = tempfile::tempdir().unwrap();
        let (listener, path) = bind_in(directory.path(), "t").unwrap();
        let (_, other) = bind_in(directory.path(), "t").unwrap();
        assert_ne!(path, other, "each socket gets its own name");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(b"hello").unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut got = [0u8; 5];
        server.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
        remove(&path);
        assert!(!path.exists());
    }

    #[test]
    fn a_path_too_long_is_refused_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let deep = directory.path().join("d".repeat(MAX_PATH));
        let error = bind_in(&deep, "t").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("longer than"), "{error}");
    }
}
