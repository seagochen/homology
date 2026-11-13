//! The system helpers Homology's crates share, on the standard library
//! and the system's file locks (`fs4`): starting a program without a
//! console window, writing a file so that it is never half-written, taking
//! turns on a file within the process or across programs ([`LockFile`]),
//! the user's home, cache and
//! configuration folders, folders only the user can read, and Unix sockets
//! a program's own processes talk over ([`local_socket`]).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;

pub mod bundled;
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

/// End the process group `leader` leads on this computer (Unix: started
/// with `process_group(0)`), or its process tree (Windows), with all it
/// runs; quietly when nothing is left of it. Called while the leader is not
/// yet reaped, so its id names no one else's.
pub fn end_group(leader: u32) {
    let mut ending = if cfg!(windows) {
        let mut taskkill = command("taskkill");
        taskkill.args(["/T", "/F", "/PID", &leader.to_string()]);
        taskkill
    } else {
        let mut kill = command("kill");
        kill.args(["-KILL", "--", &format!("-{leader}")]);
        kill
    };
    let _ = ending.stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Open the file or folder `target` with the program the system uses for
/// it (`open` on macOS, `explorer` on Windows, `xdg-open` elsewhere). Only
/// starting it counts (`explorer` exits with 1 even when it worked); the
/// program is reaped once it ends, never left a zombie.
pub fn open(target: &Path) -> io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    reaped(command(opener).arg(target))
}

/// Open `url` with the program the system uses for its scheme. On Windows
/// through the shell's URL handler (`rundll32 url.dll,FileProtocolHandler`),
/// not `explorer`: given a URL it may open a folder instead (it did for
/// `mailto:` with the new Outlook).
pub fn open_url(url: &str) -> io::Result<()> {
    let mut opener = if cfg!(target_os = "macos") {
        command("open")
    } else if cfg!(windows) {
        let mut rundll = command("rundll32");
        rundll.arg("url.dll,FileProtocolHandler");
        rundll
    } else {
        command("xdg-open")
    };
    reaped(opener.arg(url))
}

/// Start `command` and wait for it on a thread of its own, so that it is
/// reaped once it ends.
fn reaped(command: &mut Command) -> io::Result<()> {
    let mut child = command.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
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

/// How long [`LockFile::take`] waits for another program that holds the
/// lock.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The turns of one file's [`LockFile`] in the process: tickets handed out
/// as threads arrive (`next`), served in that order (`serving`).
#[derive(Default)]
struct Turns {
    tickets: Mutex<(u64, u64)>,
    served: Condvar,
}

/// The [`Turns`] of every lock taken so far, by its lock file.
static TURNS: LazyLock<Mutex<HashMap<PathBuf, Arc<Turns>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// A thread's turn at a [`LockFile`]: the next one's comes when it is dropped.
struct Turn(Arc<Turns>);

impl Turn {
    /// Wait for the turn of the thread arriving now at `turns`.
    fn wait(turns: Arc<Turns>) -> Self {
        let mut tickets = turns.tickets.lock().unwrap_or_else(PoisonError::into_inner);
        let ticket = tickets.0;
        tickets.0 += 1;
        while tickets.1 != ticket {
            tickets = turns.served.wait(tickets).unwrap_or_else(PoisonError::into_inner);
        }
        drop(tickets);
        Self(turns)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut tickets = self.0.tickets.lock().unwrap_or_else(PoisonError::into_inner);
        tickets.1 += 1;
        self.0.served.notify_all();
    }
}

/// A file's lock across programs and within the process (#18), held while
/// this lives: hold it around a read-change-write of a file other programs
/// change too (a list they share), so that each change is made on the
/// latest file and none is lost.
///
/// Threads of the process queue for it in the order they arrive; programs
/// take turns with the system's lock on `<name>.lock` beside the file
/// (`flock` on Unix, `LockFileEx` on Windows), which the system lets go of
/// when its program ends, dead or not: no lock is ever taken over from a
/// holder that is merely slow. The `.lock` file stays.
pub struct LockFile {
    file: File,
    _turn: Turn,
}

impl std::fmt::Debug for LockFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LockFile").field("file", &self.file).finish_non_exhaustive()
    }
}

impl LockFile {
    /// Take the lock of `path`: after the threads of this process that
    /// asked first, and waiting for another program that holds it
    /// (`TimedOut` after a few seconds).
    pub fn take(path: &Path) -> io::Result<Self> {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let lock = path.with_file_name(name);
        if let Some(parent) = lock.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let turns = {
            let mut all = TURNS.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(all.entry(resolved(&lock)).or_default())
        };
        let turn = Turn::wait(turns);
        let file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock)?;
        let started = Instant::now();
        while !FileExt::try_lock_exclusive(&file)? {
            if started.elapsed() > LOCK_WAIT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{} is held by another program", lock.display()),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(Self { file, _turn: turn })
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // Closing the file lets go too; this says so first, before the turn passes.
        let _ = FileExt::unlock(&self.file);
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

    /// The variable that makes this test program the other program holding
    /// a lock: the file to lock.
    const HOLDER: &str = "HOMOLOGY_TEST_LOCK_HOLDER";

    /// Run as the other program ([`HOLDER`] set): hold the lock and say so
    /// with `<file>.held`, until killed.
    #[test]
    fn holder() {
        let Some(path) = std::env::var_os(HOLDER).map(PathBuf::from) else { return };
        let _held = LockFile::take(&path).unwrap();
        fs::write(path.with_extension("held"), "").unwrap();
        std::thread::sleep(Duration::from_secs(60));
    }

    /// The threads of a process take a lock in the order they asked (#18).
    #[test]
    fn threads_take_a_lock_in_turn() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.json");
        let order = Arc::new(Mutex::new(Vec::new()));
        let held = LockFile::take(&path).unwrap();
        let waiting: Vec<_> = (0..6)
            .map(|at| {
                let (path, order) = (path.clone(), Arc::clone(&order));
                let waiter = std::thread::spawn(move || {
                    let _held = LockFile::take(&path).unwrap();
                    order.lock().unwrap().push(at);
                });
                // Each asks after the one before.
                std::thread::sleep(Duration::from_millis(30));
                waiter
            })
            .collect();
        assert!(order.lock().unwrap().is_empty(), "all wait while it is held");
        drop(held);
        for waiter in waiting {
            waiter.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), (0..6).collect::<Vec<_>>());
    }

    /// Another program's lock is waited for however slow it is, and is free
    /// the moment that program dies (#18).
    #[test]
    fn another_program_s_lock_is_never_taken_over_and_frees_when_it_dies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.json");
        let mut holder = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::holder", "--nocapture", "--test-threads=1"])
            .env(HOLDER, &path)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !path.with_extension("held").exists() {
            assert!(started.elapsed() < Duration::from_secs(30), "the holder never took the lock");
            std::thread::sleep(Duration::from_millis(20));
        }
        let waiter = {
            let path = path.clone();
            std::thread::spawn(move || LockFile::take(&path).map(|_| Instant::now()))
        };
        std::thread::sleep(Duration::from_millis(500));
        assert!(!waiter.is_finished(), "a holder that is slow keeps it");
        let killed = Instant::now();
        holder.kill().unwrap();
        holder.wait().unwrap();
        let taken = waiter.join().unwrap().unwrap();
        assert!(taken >= killed && taken - killed < Duration::from_secs(2), "free once it died");
        assert!(path.with_file_name("shared.json.lock").is_file(), "the lock file stays");
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
