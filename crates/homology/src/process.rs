//! Owned pipe processes for language protocols and long-running application services.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};

/// Protocol frames are bounded independently of the retained event queue.
const MAX_FRAME: usize = 4 * 1024 * 1024;
/// Retained output cannot grow with a noisy or unattended service.
const MAX_OUTPUT: usize = 1024 * 1024;

/// Recover a worker's shared output after a poisoned lock.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How stdout is transported. Stderr is always retained as raw log bytes.
#[derive(Clone, Copy)]
pub enum Framing {
    Raw,
    ContentLength,
}

/// A Node-based pipe guard terminates remote children when the SSH input closes.
/// Unlike a PTY it preserves protocol bytes and works for foreground application servers too.
const REMOTE_GUARD: &str = r#"
const {spawn}=require('node:child_process');
const child=spawn(process.argv[1],process.argv.slice(2),{stdio:['pipe','inherit','inherit'],detached:true});
let timer;
function end(){if(!child.pid)return;try{process.kill(-child.pid,'SIGTERM')}catch{};timer=setTimeout(()=>{try{process.kill(-child.pid,'SIGKILL')}catch{};process.exit(1)},1000);timer.unref()}
process.stdin.pipe(child.stdin);child.stdin.on('error',()=>{});
process.stdin.on('end',end);process.on('SIGTERM',end);process.on('SIGINT',end);process.on('SIGHUP',end);
child.on('error',e=>{console.error(e.message);process.exit(127)});
child.on('exit',(code,signal)=>{try{process.kill(-child.pid,'SIGKILL')}catch{};clearTimeout(timer);process.exit(code===null?1:code)});
"#;

/// Build a trusted system-SSH command with a caller-selected remote Node runtime.
/// The guard owns descendants and closes them on disconnect without terminal emulation.
pub fn remote_command(
    context: &homology_ssh::Context,
    target: &homology_ssh::SshTarget,
    folder: &str,
    node: &str,
    program: &str,
    arguments: &[String],
) -> Result<Command, String> {
    let words = [node, "-e", REMOTE_GUARD, "--", program]
        .into_iter()
        .chain(arguments.iter().map(String::as_str))
        .map(homology_ssh::shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    homology_ssh::session_command(context, target, &format!("cd {} && exec {words}", homology_ssh::quoted_dir(folder)))
        .map_err(|error| error.to_string())
}

/// One output event; protocol payloads exclude their Content-Length headers.
#[derive(Clone, Debug)]
pub enum Output {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Error(String),
}

/// A bounded, nonblocking mailbox; old output is dropped before producers can stall.
struct Mailbox {
    maximum: usize,
    events: VecDeque<Output>,
    bytes: usize,
    dropped: bool,
}

impl Default for Mailbox {
    /// Preserve the raw-stream retention budget unless a protocol selects its own.
    fn default() -> Self {
        Self::new(MAX_OUTPUT)
    }
}

impl Mailbox {
    /// A protocol mailbox must have room for at least one complete allowed frame.
    fn new(maximum: usize) -> Self {
        Self { maximum, events: VecDeque::new(), bytes: 0, dropped: false }
    }

    /// Retain the newest bounded output; report that a reader missed older events.
    fn push(&mut self, output: Output) {
        self.bytes += length(&output);
        self.events.push_back(output);
        while self.bytes > self.maximum || self.events.len() > 512 {
            if let Some(event) = self.events.pop_front() {
                self.bytes = self.bytes.saturating_sub(length(&event));
                self.dropped = true;
            }
        }
    }
}

/// Byte cost of one retained event.
fn length(output: &Output) -> usize {
    match output {
        Output::Stdout(bytes) | Output::Stderr(bytes) => bytes.len(),
        Output::Error(error) => error.len(),
    }
}

/// A process and its pipes. The caller owns routing, protocols, and repaint policy.
pub struct Process {
    child: Child,
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    output: Arc<Mutex<Mailbox>>,
    exited: Option<Option<i32>>,
}

impl Process {
    /// Spawn an already configured local or SSH command in its own process group.
    pub fn start(mut command: Command, framing: Framing) -> Result<Self, String> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().map_err(|error| error.to_string())?;
        let maximum = if matches!(framing, Framing::ContentLength) { MAX_FRAME } else { MAX_OUTPUT };
        let output = Arc::new(Mutex::new(Mailbox::new(maximum)));
        let mut process = Self { child, input: None, output, exited: None };
        let mut input = process.child.stdin.take().ok_or("Process stdin unavailable")?;
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(8);
        process.input = Some(sender);
        let output = process.output.clone();
        std::thread::spawn(move || {
            for bytes in receiver {
                if let Err(error) = input.write_all(&bytes).and_then(|()| input.flush()) {
                    lock(&output).push(Output::Error(error.to_string()));
                    break;
                }
            }
        });
        let stdout = process.child.stdout.take().ok_or("Process stdout unavailable")?;
        let stderr = process.child.stderr.take().ok_or("Process stderr unavailable")?;
        let output = process.output.clone();
        std::thread::spawn(move || {
            if let Err(error) = read(stdout, framing, &output) {
                lock(&output).push(Output::Error(error));
            }
        });
        let output = process.output.clone();
        std::thread::spawn(move || {
            let _ = raw(stderr, true, &output);
        });
        Ok(process)
    }

    /// Send exact protocol bytes; the caller decides framing and line endings.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_FRAME {
            return Err("Process input exceeds 4 MiB".into());
        }
        let input = self.input.as_ref().ok_or("Process input is closed")?;
        input.try_send(bytes.to_vec()).map_err(|error| error.to_string())
    }

    /// Drain output without waiting for the child or its readers.
    pub fn drain(&mut self) -> Vec<Output> {
        let mut mailbox = lock(&self.output);
        let mut events = Vec::new();
        if mailbox.dropped {
            events.push(Output::Error("Service output exceeded its retention limit".into()));
        }
        events.extend(mailbox.events.drain(..));
        mailbox.bytes = 0;
        mailbox.dropped = false;
        events
    }

    /// None while running; Some(exit code) after exit (None code means a signal).
    pub fn exited(&mut self) -> Result<Option<Option<i32>>, String> {
        if self.exited.is_none() {
            #[cfg(target_os = "linux")]
            {
                use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
                let status = waitid(
                    Id::Pid(nix::unistd::Pid::from_raw(self.child.id() as i32)),
                    WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
                )
                .map_err(|error| error.to_string())?;
                if !matches!(status, WaitStatus::StillAlive) {
                    // The unreaped leader still owns this pid, even after a shell exits early.
                    homology_sys::end_group(self.child.id());
                    self.exited = Some(self.child.wait().map_err(|error| error.to_string())?.code());
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                self.exited = self.child.try_wait().map_err(|error| error.to_string())?.map(|status| status.code());
            }
        }
        Ok(self.exited)
    }

    /// Close pipes and terminate the owned process tree. Safe to call repeatedly.
    pub fn stop(&mut self) {
        self.input.take();
        if self.exited.is_none() {
            homology_sys::end_group(self.child.id());
            let _ = self.child.kill();
            self.exited = Some(self.child.wait().ok().and_then(|status| status.code()));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Carry arbitrary stdout or stderr bytes without assuming UTF-8 read boundaries.
fn raw(mut reader: impl Read, stderr: bool, output: &Mutex<Mailbox>) -> Result<(), String> {
    let mut buffer = [0; 8192];
    loop {
        let count = reader.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(());
        }
        let bytes = buffer[..count].to_vec();
        lock(output).push(if stderr { Output::Stderr(bytes) } else { Output::Stdout(bytes) });
    }
}

/// Parse bounded Content-Length frames, including empty lines between tsserver messages.
fn read(reader: impl Read, framing: Framing, output: &Mutex<Mailbox>) -> Result<(), String> {
    if matches!(framing, Framing::Raw) {
        return raw(reader, false, output);
    }
    let mut reader = BufReader::new(reader);
    while let Some(frame) = crate::framing::content_length(&mut reader, MAX_FRAME).map_err(|error| error.to_string())? {
        lock(output).push(Output::Stdout(frame));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framed_unicode_and_retention_are_bounded() {
        let output = Mutex::new(Mailbox::default());
        read(&b"\r\nContent-Length: 6\r\n\r\n\xe4\xb8\xad\xe6\x96\x87"[..], Framing::ContentLength, &output).unwrap();
        assert!(matches!(&lock(&output).events[0], Output::Stdout(bytes) if bytes == "中文".as_bytes()));
        for _ in 0..1024 {
            lock(&output).push(Output::Stdout(vec![0; 8192]));
        }
        assert!(lock(&output).bytes <= MAX_OUTPUT);
        assert!(lock(&output).dropped);
        assert!(read(&b"Content-Length: 99999999\r\n\r\n"[..], Framing::ContentLength, &output).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn a_valid_multi_megabyte_frame_is_not_dropped_by_the_mailbox() {
        let mut command = homology_sys::command("sh");
        command.args(["-c", "printf 'Content-Length: 2097152\\r\\n\\r\\n'; head -c 2097152 /dev/zero"]);
        let mut process = Process::start(command, Framing::ContentLength).unwrap();
        let started = std::time::Instant::now();
        loop {
            for event in process.drain() {
                match event {
                    Output::Stdout(bytes) => {
                        assert_eq!(bytes.len(), 2 * 1024 * 1024);
                        assert!(bytes.iter().all(|byte| *byte == 0));
                        return;
                    }
                    Output::Error(error) => panic!("legal frame was lost: {error}"),
                    Output::Stderr(_) => {}
                }
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires Node.js to exercise the remote pipe guard without an SSH server"]
    fn closing_remote_input_reaps_descendants_that_ignore_termination() {
        struct Descendant(u32);
        impl Drop for Descendant {
            fn drop(&mut self) {
                let command = std::fs::read(format!("/proc/{}/cmdline", self.0)).unwrap_or_default();
                if String::from_utf8_lossy(&command).contains("homology-guard-fixture") {
                    let _ = homology_sys::command("kill").args(["-KILL", &self.0.to_string()]).status();
                }
            }
        }
        let leaf = "process.title='homology-guard-fixture';process.on('SIGTERM',()=>{});console.log(process.pid);setInterval(()=>{},1000)";
        let parent = "require('child_process').spawn(process.execPath,['-e',process.argv[1]],{stdio:['ignore','inherit','inherit']});setInterval(()=>{},1000)";
        let mut command = homology_sys::command("node");
        command.args(["-e", REMOTE_GUARD, "--", "node", "-e", parent, leaf]);
        let mut process = Process::start(command, Framing::Raw).unwrap();
        let start = std::time::Instant::now();
        let mut bytes = Vec::new();
        while !bytes.contains(&b'\n') {
            for event in process.drain() {
                if let Output::Stdout(chunk) = event {
                    bytes.extend(chunk);
                }
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let descendant = Descendant(String::from_utf8(bytes).unwrap().trim().parse().unwrap());
        // Closing the pipe models SSH's EOF, without prematurely killing the guard itself.
        process.input.take();
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", descendant.0)).ok();
            if stat.is_none_or(|stat| stat.split(") ").nth(1).is_some_and(|tail| tail.starts_with('Z'))) {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "grandchild ignored termination and survived SSH EOF"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    #[cfg(unix)]
    #[test]
    fn a_real_pipe_process_roundtrips_and_stops() {
        let mut command = homology_sys::command("cat");
        command.env("TERM", "dumb");
        let mut process = Process::start(command, Framing::Raw).unwrap();
        process.send("hello 中文\n".as_bytes()).unwrap();
        let start = std::time::Instant::now();
        let mut bytes = Vec::new();
        while bytes.len() < "hello 中文\n".len() {
            for event in process.drain() {
                if let Output::Stdout(chunk) = event {
                    bytes.extend(chunk);
                }
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(bytes, "hello 中文\n".as_bytes());
        process.stop();
        assert!(process.exited().unwrap().is_some());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_shell_cannot_leave_its_background_child_running() {
        let mut command = homology_sys::command("sh");
        command.args(["-c", "sleep 600 & echo $!"]);
        let mut process = Process::start(command, Framing::Raw).unwrap();
        let start = std::time::Instant::now();
        let mut bytes = Vec::new();
        while !bytes.contains(&b'\n') {
            for output in process.drain() {
                if let Output::Stdout(chunk) = output {
                    bytes.extend(chunk);
                }
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pid: u32 = String::from_utf8(bytes).unwrap().trim().parse().unwrap();
        while process.exited().unwrap().is_none() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
            if stat.is_none_or(|stat| stat.split(") ").nth(1).is_some_and(|tail| tail.starts_with('Z'))) {
                break;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "background child survived its shell");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
