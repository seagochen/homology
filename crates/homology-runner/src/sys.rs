//! The system helpers the runner needs, kept here so that it depends on no
//! other crate of ours: starting a program without a console window,
//! writing a file so that it is never half-written, and taking turns on a
//! file within the process.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

/// `program` as a command to configure and run: no console window of its
/// own on Windows (a GUI program that starts it would flash one), and an
/// empty stdin (a caller that writes to it says so).
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW`: a console program runs without a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// A file's lock within the process ([`file_lock`]).
pub type FileLock = Arc<Mutex<()>>;

/// The [`file_lock`] of every path asked for so far.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, FileLock>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The lock of `path` (made absolute, its links resolved as far as it
/// exists), one per file for the process: hold it around a
/// read-compare-write of that file so that writers in this process take
/// turns.
pub fn file_lock(path: &Path) -> FileLock {
    let key = resolved(path);
    let mut locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(locks.entry(key).or_default())
}

/// Hold `lock` until the guard is dropped; a lock a panicking thread held
/// is taken all the same (it guards no data).
pub fn hold(lock: &FileLock) -> std::sync::MutexGuard<'_, ()> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `path` absolute, with the links of its deepest existing folder resolved,
/// so that two spellings of one file share a lock.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(full) = path.canonicalize() {
        return full;
    }
    match (path.parent().and_then(|parent| parent.canonicalize().ok()), path.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => std::env::current_dir().map(|dir| dir.join(path)).unwrap_or_else(|_| path.to_path_buf()),
    }
}

/// The sibling `<name>.tmp` a write goes through before it replaces `path`.
fn temporary_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Write `payload` to `path` so that a reader sees the old content or the
/// new, never half of it: into a synced temporary file beside it, then a
/// rename. The folders it goes in are made.
pub fn atomic_write_bytes(path: &Path, payload: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_for(path);
    let result = (|| {
        let mut handle = File::create(&temporary)?;
        handle.write_all(payload)?;
        handle.flush()?;
        handle.sync_all()?;
        drop(handle);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_spellings_of_a_file_share_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let a = file_lock(&dir.path().join("x.py"));
        let b = file_lock(&dir.path().join("sub/../x.py"));
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn a_write_replaces_the_file_whole_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deep/x.txt");
        atomic_write_bytes(&file, b"one").unwrap();
        atomic_write_bytes(&file, b"two").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"two");
        assert_eq!(fs::read_dir(dir.path().join("deep")).unwrap().count(), 1);
    }
}
