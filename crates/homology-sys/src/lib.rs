//! The system helpers Homology's crates share, on the standard library
//! alone: starting a program without a console window, writing a file so
//! that it is never half-written, taking turns on a file within the
//! process or across programs ([`LockFile`]), the user's home, cache and
//! configuration folders, folders only the user can read, and Unix sockets
//! a program's own processes talk over ([`local_socket`]).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
pub mod local_socket;

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

/// The user's home folder: `HOME`, or `USERPROFILE` on Windows; none when
/// it is not set to an absolute path.
pub fn home_dir() -> Option<PathBuf> {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute())
}

/// The user's cache folder on this computer: `$XDG_CACHE_HOME` when it is
/// an absolute path, else `~/.cache` (Linux and other Unix),
/// `~/Library/Caches` (macOS), `%LOCALAPPDATA%` (Windows). None without a
/// home folder.
pub fn cache_home() -> Option<PathBuf> {
    let absolute = |name: &str| std::env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute());
    if cfg!(windows) {
        return absolute("LOCALAPPDATA");
    }
    if let Some(xdg) = absolute("XDG_CACHE_HOME").filter(|_| !cfg!(target_os = "macos")) {
        return Some(xdg);
    }
    let home = home_dir()?;
    Some(if cfg!(target_os = "macos") { home.join("Library/Caches") } else { home.join(".cache") })
}

/// The user's configuration folder on this computer: `$XDG_CONFIG_HOME`
/// when it is an absolute path, else `~/.config` (Linux and other Unix),
/// `~/Library/Application Support` (macOS), `%APPDATA%` (Windows). None
/// without a home folder.
pub fn config_home() -> Option<PathBuf> {
    config_home_with(&|name| std::env::var_os(name))
}

/// [`config_home`] with the environment read through `variable`.
fn config_home_with(variable: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let absolute = |name: &str| variable(name).map(PathBuf::from).filter(|path| path.is_absolute());
    if cfg!(windows) {
        return absolute("APPDATA");
    }
    if let Some(xdg) = absolute("XDG_CONFIG_HOME").filter(|_| !cfg!(target_os = "macos")) {
        return Some(xdg);
    }
    let home = absolute("HOME")?;
    Some(if cfg!(target_os = "macos") { home.join("Library/Application Support") } else { home.join(".config") })
}

/// How long [`LockFile::take`] waits for another holder, and how old a
/// lock file is before it counts as left by a program that died.
const LOCK_WAIT: Duration = Duration::from_secs(5);
const LOCK_STALE: Duration = Duration::from_secs(10);

/// A file's lock across programs: a `<name>.lock` file beside it, made
/// only when none is there (the system's exclusive create), removed when
/// this is dropped. Hold it around a read-change-write of a file other
/// programs change too (a list they share). The exclusive create keeps
/// the threads of one process apart as well.
#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
}

impl LockFile {
    /// Take the lock of `path`, waiting for another program that holds it
    /// (`TimedOut` after a few seconds); a lock file left by a program that
    /// died is taken over once it is old.
    pub fn take(path: &Path) -> io::Result<Self> {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let lock = path.with_file_name(name);
        if let Some(parent) = lock.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let started = Instant::now();
        loop {
            match fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
                Ok(_) => return Ok(Self { path: lock }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let age = fs::metadata(&lock)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok());
                    if age.is_some_and(|age| age > LOCK_STALE) {
                        let _ = fs::remove_file(&lock);
                        continue;
                    }
                    if started.elapsed() > LOCK_WAIT {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("{} is held by another program", lock.display()),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Make `directory` (and its parents) if needed, readable only by the user:
/// `0700` on Unix, made so and reset so when it already exists. Windows is
/// left to the user's profile it lies in.
pub fn private_dir(directory: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)?;
    // Made before by something else: closed all the same.
    #[cfg(unix)]
    fs::set_permissions(directory, <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700))?;
    Ok(())
}

/// How many names [`temporary_beside`] tries before it gives up.
const TEMPORARY_TRIES: u32 = 64;

/// A new, empty temporary file beside `path` that a write goes through
/// before it replaces `path`, and its name: `.<name>.<random>.tmp`.
///
/// It is created only if nothing is at that name yet (`create_new`, the
/// system's exclusive create): an existing file, or a symbolic link
/// someone left there, even one that points nowhere, is never opened,
/// followed or written through (homology#3); another name is tried
/// instead. The name is not predictable either, so none can be laid in
/// wait for it.
pub fn temporary_beside(path: &Path) -> io::Result<(PathBuf, File)> {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let mut last = io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary name");
    for _ in 0..TEMPORARY_TRIES {
        // Randomly keyed by the standard library, mixed with what differs
        // between calls: a name nobody can guess ahead of time.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        hasher.write_u32(std::process::id());
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        hasher.write_u128(nanos);
        let temporary = path.with_file_name(format!(".{name}.{:016x}.tmp", hasher.finish()));
        match fs::OpenOptions::new().write(true).create_new(true).open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = error,
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

/// Write `payload` to `path` so that a reader sees the old content or the
/// new, never half of it: into a synced temporary file beside it
/// ([`temporary_beside`]), then a rename, which replaces `path` itself
/// (were it a link, the link, not what it points at). The folders it goes
/// in are made.
pub fn atomic_write_bytes(path: &Path, payload: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (temporary, mut handle) = temporary_beside(path)?;
    let result = (|| {
        handle.write_all(payload)?;
        handle.flush()?;
        handle.sync_all()?;
        fs::rename(&temporary, path)
    })();
    drop(handle);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_folder_follows_the_system() {
        use std::ffi::OsString;
        let home = if cfg!(windows) { r"C:\Users\me" } else { "/home/me" };
        let environment = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| OsString::from(value))
        };
        let plain = config_home_with(&environment(if cfg!(windows) {
            &[("APPDATA", r"C:\Users\me\AppData\Roaming"), ("USERPROFILE", r"C:\Users\me")]
        } else {
            &[("HOME", "/home/me")]
        }));
        let expected = if cfg!(windows) {
            PathBuf::from(r"C:\Users\me\AppData\Roaming")
        } else if cfg!(target_os = "macos") {
            PathBuf::from(home).join("Library/Application Support")
        } else {
            PathBuf::from(home).join(".config")
        };
        assert_eq!(plain, Some(expected));
        if cfg!(all(unix, not(target_os = "macos"))) {
            let xdg = config_home_with(&environment(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "/xdg")]));
            assert_eq!(xdg, Some(PathBuf::from("/xdg")));
            let relative = config_home_with(&environment(&[("HOME", "/home/me"), ("XDG_CONFIG_HOME", "xdg")]));
            assert_eq!(relative, Some(PathBuf::from("/home/me/.config")), "a relative one is ignored");
        }
        assert_eq!(config_home_with(&|_| None), None);
    }

    #[test]
    fn a_lock_file_is_held_once_and_a_dead_holder_s_is_taken_over() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.json");
        let lock = path.with_file_name("shared.json.lock");
        {
            let _held = LockFile::take(&path).unwrap();
            assert!(lock.is_file());
            // Another program holds it: one in this process waits its turn.
            let waiter = {
                let path = path.clone();
                std::thread::spawn(move || LockFile::take(&path).map(|_| ()))
            };
            std::thread::sleep(Duration::from_millis(50));
            assert!(!waiter.is_finished());
            drop(_held);
            waiter.join().unwrap().unwrap();
        }
        assert!(!lock.exists(), "dropped, it is gone");
        // Left by a program that died long ago: taken over.
        std::fs::write(&lock, "").unwrap();
        let old = SystemTime::now() - LOCK_STALE - Duration::from_secs(1);
        File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
        drop(LockFile::take(&path).unwrap());
        assert!(!lock.exists());
    }

    #[test]
    fn two_spellings_of_a_file_share_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let a = file_lock(&dir.path().join("x.py"));
        let b = file_lock(&dir.path().join("sub/../x.py"));
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[cfg(unix)]
    #[test]
    fn a_private_folder_is_the_user_s_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("a/b");
        fs::create_dir_all(&private).unwrap();
        private_dir(&private).unwrap();
        assert_eq!(fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o700);
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

    /// A write goes through a fresh file each time, leaves none behind, and
    /// never writes through what is already at a temporary name (#3).
    #[test]
    fn a_write_leaves_no_temporary_file_and_takes_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/a.txt");
        atomic_write_bytes(&path, b"one").unwrap();
        atomic_write_bytes(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(dir.path().join("sub")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["a.txt"]);
        let (first, _) = temporary_beside(&path).unwrap();
        let (second, _) = temporary_beside(&path).unwrap();
        assert_ne!(first, second);
        assert!(first.file_name().unwrap().to_string_lossy().starts_with(".a.txt."));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_beside_the_file_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "ORIGINAL").unwrap();
        fs::create_dir(dir.path().join("root")).unwrap();
        let path = dir.path().join("root/a.py");
        std::os::unix::fs::symlink(&outside, dir.path().join("root/a.py.tmp")).unwrap();
        atomic_write_bytes(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "ORIGINAL");
        assert!(fs::symlink_metadata(&path).unwrap().file_type().is_file());
    }
}
