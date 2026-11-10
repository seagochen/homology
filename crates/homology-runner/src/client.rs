//! The caller's side: start a runner, greet it, ask it things and read its
//! events. The runner is a child process whose stdin and stdout carry the
//! protocol: on this computer the runner itself, on a host `ssh … serve`.
//! Either way the caller hands over the command; this module never decides
//! where a runner is.
//!
//! A runner that ends, writes something that is not a message or does not
//! answer in time is finished: the client kills it, so no runner is left
//! behind, and every later request fails with the same error.

use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol::{
    read_line, Line, Message, Request, RunnerError, RunnerResult, MAX_LINE, PROTOCOL, PROTOCOL_MISMATCH, RUNNER_EXITED,
    RUNNER_INVALID_OUTPUT, RUNNER_IO, RUNNER_START_FAILED, RUNNER_TIMEOUT, VERSION,
};
use crate::JsonMap;

/// How long a runner may take to start and greet.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of the runner's stderr is kept for an error message.
const STDERR_TAIL: usize = 4096;

/// An event the runner sent: its name and data.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub event: String,
    pub data: Value,
}

/// A runner being talked to.
pub struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    incoming: Receiver<RunnerResult<Message>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
    events: VecDeque<Event>,
    next_id: u64,
    hello: Value,
    /// Why the runner is finished, once it is.
    broken: Option<RunnerError>,
}

impl Client {
    /// Start `command` (a runner's `serve`; its stdio is taken over here)
    /// and greet it.
    pub fn start(mut command: Command, timeout: Duration) -> RunnerResult<Client> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| RunnerError::new(RUNNER_START_FAILED, format!("could not start the runner: {error}")))?;
        let stdin = child.stdin.take();
        let (sender, incoming) = mpsc::channel();
        let mut readers = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            readers.push(std::thread::spawn(move || {
                let mut stdout = BufReader::new(stdout);
                loop {
                    let message = match read_line(&mut stdout, MAX_LINE) {
                        Ok(None) => break,
                        Ok(Some(Line::Text(line))) if line.trim().is_empty() => continue,
                        Ok(Some(Line::Text(line))) => Message::parse(&line),
                        Ok(Some(Line::TooLong)) => Err(RunnerError::new(
                            RUNNER_INVALID_OUTPUT,
                            format!("the runner wrote a line over {MAX_LINE} bytes"),
                        )),
                        Err(error) => Err(RunnerError::new(
                            RUNNER_INVALID_OUTPUT,
                            format!("could not read the runner's output: {error}"),
                        )),
                    };
                    let stop = message.is_err();
                    if sender.send(message).is_err() || stop {
                        return;
                    }
                }
                let _ = sender.send(Err(RunnerError::new(RUNNER_EXITED, "the runner closed its output")));
            }));
        }
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stderr.take() {
            let tail = Arc::clone(&stderr);
            readers.push(std::thread::spawn(move || keep_tail(pipe, &tail)));
        }
        let mut client = Client {
            child,
            stdin,
            incoming,
            stderr,
            readers,
            events: VecDeque::new(),
            next_id: 1,
            hello: Value::Null,
            broken: None,
        };
        let hello = client.request("hello", object(json!({"protocol": PROTOCOL, "version": VERSION})), timeout)?;
        if hello.get("protocol").and_then(Value::as_u64) != Some(PROTOCOL) {
            let message = format!(
                "the runner ({}) speaks protocol {}, this client ({VERSION}) speaks {PROTOCOL}",
                hello.get("version").and_then(Value::as_str).unwrap_or("?"),
                hello.get("protocol").unwrap_or(&Value::Null)
            );
            return Err(client.finish(RunnerError::new(PROTOCOL_MISMATCH, message)));
        }
        client.hello = hello;
        Ok(client)
    }

    /// What the runner said to `hello`: `{protocol, version, ops}`.
    pub fn hello(&self) -> &Value {
        &self.hello
    }

    /// The runner's process id.
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Ask the runner to do `op` and wait at most `timeout` for its answer.
    /// Events that arrive meanwhile are kept for [`Client::next_event`].
    pub fn request(&mut self, op: &str, params: JsonMap, timeout: Duration) -> RunnerResult<Value> {
        if let Some(error) = &self.broken {
            return Err(error.clone());
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = Request { id, op: op.to_string(), params }.to_line();
        let written = match self.stdin.as_mut() {
            Some(stdin) => writeln!(stdin, "{line}").and_then(|()| stdin.flush()),
            None => Err(std::io::Error::other("the runner's input is closed")),
        };
        if let Err(error) = written {
            // A runner that ended closes its input: say why it ended.
            let why = self
                .wait_ended(Duration::from_secs(2))
                .unwrap_or_else(|| RunnerError::new(RUNNER_IO, format!("could not write to the runner: {error}")));
            return Err(self.finish(why));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.receive(deadline) {
                Ok(Message::Reply { id: answered, outcome }) if answered == id => return outcome,
                // An answer to a request given up on (cannot happen: giving up finishes the runner).
                Ok(Message::Reply { .. }) => continue,
                Ok(Message::Event { event, data }) => self.events.push_back(Event { event, data }),
                // A runner that does not answer is given up on, and killed.
                Err(error) if error.code == RUNNER_TIMEOUT && self.broken.is_none() => {
                    let message = format!("the runner did not answer {op:?} within {} s", timeout.as_secs_f32());
                    return Err(self.finish(RunnerError::new(RUNNER_TIMEOUT, message)));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// The next event the runner sent, waiting at most `timeout`; `None`
    /// when none came.
    pub fn next_event(&mut self, timeout: Duration) -> RunnerResult<Option<Event>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        if let Some(error) = &self.broken {
            return Err(error.clone());
        }
        match self.receive(Instant::now() + timeout) {
            Ok(Message::Event { event, data }) => Ok(Some(Event { event, data })),
            Ok(Message::Reply { .. }) => Ok(None),
            Err(error) if error.code == RUNNER_TIMEOUT && self.broken.is_none() => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// End the session: `shutdown`, then wait for the runner to exit; one
    /// that does not within `timeout` is killed.
    pub fn close(mut self, timeout: Duration) -> RunnerResult<ExitStatus> {
        if self.broken.is_none() {
            let _ = self.request("shutdown", JsonMap::new(), timeout);
        }
        self.stdin = None;
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = self.child.kill();
                    return self
                        .child
                        .wait()
                        .map_err(|error| RunnerError::new(RUNNER_IO, format!("could not end the runner: {error}")));
                }
            }
        }
    }

    /// Whether the runner process is still running.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The next message, or why there is none: a runner that broke is
    /// finished here (killed), except that a plain wait running out while
    /// reading events is not a failure (the caller tells them apart).
    fn receive(&mut self, deadline: Instant) -> RunnerResult<Message> {
        let wait = deadline.saturating_duration_since(Instant::now());
        match self.incoming.recv_timeout(wait) {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(error)) if error.code == RUNNER_EXITED => {
                let why = self.wait_ended(Duration::from_secs(2)).unwrap_or(error);
                Err(self.finish(why))
            }
            Ok(Err(error)) => Err(self.finish(error)),
            Err(RecvTimeoutError::Disconnected) => {
                let why = self
                    .wait_ended(Duration::from_secs(2))
                    .unwrap_or_else(|| RunnerError::new(RUNNER_EXITED, "the runner closed its output"));
                Err(self.finish(why))
            }
            Err(RecvTimeoutError::Timeout) => {
                Err(RunnerError::new(RUNNER_TIMEOUT, "the runner did not answer in time"))
            }
        }
    }

    /// The runner's exit, as an error with its status and the end of its
    /// stderr, once it has ended (waiting at most `wait`).
    fn wait_ended(&mut self, wait: Duration) -> Option<RunnerError> {
        let deadline = Instant::now() + wait;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => return None,
            }
        };
        // The stderr reader ends with the process; take what it kept.
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        let stderr =
            self.stderr.lock().map(|tail| String::from_utf8_lossy(&tail).trim().to_string()).unwrap_or_default();
        let mut message = format!("the runner ended ({status})");
        if !stderr.is_empty() {
            message.push_str(": ");
            message.push_str(&stderr);
        }
        Some(RunnerError::new(RUNNER_EXITED, message))
    }

    /// Mark the runner finished for `error` and make sure it is gone.
    fn finish(&mut self, error: RunnerError) -> RunnerError {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.broken = Some(error.clone());
        error
    }
}

impl Drop for Client {
    /// No runner outlives its client.
    fn drop(&mut self) {
        self.stdin = None;
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Keep the last [`STDERR_TAIL`] bytes of `pipe` in `tail`.
fn keep_tail(mut pipe: impl Read, tail: &Mutex<Vec<u8>>) {
    let mut buffer = [0u8; 4096];
    while let Ok(read) = pipe.read(&mut buffer) {
        if read == 0 {
            return;
        }
        let Ok(mut kept) = tail.lock() else { return };
        kept.extend_from_slice(&buffer[..read]);
        let excess = kept.len().saturating_sub(STDERR_TAIL);
        kept.drain(..excess);
    }
}

/// The fields of an object value; none for anything else.
fn object(value: Value) -> JsonMap {
    match value {
        Value::Object(fields) => fields,
        _ => JsonMap::new(),
    }
}
