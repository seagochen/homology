//! The runner's side: answer requests read from `input` on `output`, one
//! line each, until `shutdown`, the end of the input, or a peer that does
//! not speak the protocol.
//!
//! The operations every runner has are the session's own (`hello`, `ping`,
//! `shutdown`), [`crate::fs`]'s, the environments' ([`crate::env`]) and the
//! code check ([`crate::check`]). A program that serves a runner of its
//! own adds its operations through [`Runner`]: `hello` lists them with the
//! others, and a request for one goes to its `answer`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::protocol::{
    read_line, Line, Message, Request, RunnerError, RunnerResult, HELLO_REQUIRED, INVALID_PARAMS, MAX_LINE,
    MESSAGE_TOO_LARGE, PROTOCOL, PROTOCOL_MISMATCH, UNKNOWN_OP, VERSION,
};
use crate::JsonMap;

/// The operations every runner answers, as `hello` lists them.
pub const OPS: &[&str] = &[
    "hello", "ping", "shutdown", "fs.list", "fs.read", "fs.write", "fs.mkdir", "fs.move", "fs.copy", "fs.delete",
    "fs.search", "env.discover", "env.probe", "code.check", "toolchain.discover", "toolchain.probe", "code.exec",
    "code.complete",
];

/// A runner as a program serves it: its name and version, as `hello` gives
/// them, where it keeps its cache, what it adds to the code check, and the
/// operations it adds to [`OPS`].
#[derive(Clone, Copy)]
pub struct Runner {
    /// The program's name, for messages (`homology-runner`).
    pub name: &'static str,
    /// The program's version: what a caller deploys by.
    pub version: &'static str,
    /// The operations it adds.
    pub ops: &'static [&'static str],
    /// The answer to one of `ops`.
    pub answer: fn(&str, &JsonMap) -> RunnerResult<Value>,
    /// Its cache folder on the runner's computer (the checker's Python, the
    /// files being checked): only what can be had again.
    pub cache: fn() -> RunnerResult<PathBuf>,
    /// Python files it brings besides the checker's ([`crate::python`]):
    /// its rules modules and what they import.
    pub python: &'static [crate::python::File],
    /// The rules modules the code check imports ([`crate::check`]).
    pub rules: &'static [&'static str],
}

impl Runner {
    /// The runner of this crate alone (the `homology-runner` program): no
    /// operation besides [`OPS`].
    pub const PLAIN: Runner = Runner {
        name: "homology-runner",
        version: VERSION,
        ops: &[],
        answer: none,
        cache: plain_cache,
        python: &[],
        rules: &[],
    };

    /// Every operation it answers.
    pub fn ops(&self) -> Vec<&'static str> {
        OPS.iter().chain(self.ops).copied().collect()
    }

    /// The outcome of every operation but the session's own (`hello`,
    /// `ping`, `shutdown`).
    fn dispatch(&self, op: &str, params: &JsonMap) -> RunnerResult<Value> {
        match op {
            "env.discover" => return env_discover(params),
            "env.probe" => return env_probe(params),
            "code.check" => {
                let asked = crate::check::asked(params)?;
                return crate::check::run(self, &asked.python, &asked.files, &asked.listing, asked.timeout);
            }
            "toolchain.discover" => return Ok(toolchain_discover()),
            "toolchain.probe" => return crate::toolchain::probe_op(params),
            "code.exec" => return crate::exec::op(params),
            "code.complete" => return crate::complete::run(self, &crate::complete::asked(params)?),
            op if op.starts_with("fs.") && OPS.contains(&op) => return crate::fs::op(op, params),
            _ => {}
        }
        if self.ops.contains(&op) {
            return (self.answer)(op, params);
        }
        Err(RunnerError::new(
            UNKNOWN_OP,
            format!("this runner ({} {}) has no operation {op:?}", self.name, self.version),
        ))
    }
}

/// The cache of [`Runner::PLAIN`]: `homology/runner` in the user's cache
/// folder ([`crate::sys::cache_home`]).
fn plain_cache() -> RunnerResult<PathBuf> {
    crate::sys::cache_home()
        .map(|dir| dir.join("homology").join("runner"))
        .ok_or_else(|| RunnerError::new(crate::check::CHECK_FAILED, "this computer has no cache folder"))
}

/// `env.discover`: `{roots?: [path]}` (their `.venv` count too);
/// `{environments: [{kind, name, python}]}`.
fn env_discover(params: &JsonMap) -> RunnerResult<Value> {
    let roots = match params.get("roots") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(dirs)) => dirs
            .iter()
            .map(|dir| {
                dir.as_str().map(PathBuf::from).ok_or_else(|| RunnerError::new(INVALID_PARAMS, "roots holds paths"))
            })
            .collect::<RunnerResult<_>>()?,
        Some(_) => return Err(RunnerError::new(INVALID_PARAMS, "roots is a list of paths")),
    };
    let found = crate::env::discover(&crate::env::Places::here(roots));
    Ok(json!({"environments": found.iter().map(crate::env::Found::to_json).collect::<Vec<_>>()}))
}

/// `toolchain.discover`: `{}`; `{toolchains: [{kind, name, path}]}`.
fn toolchain_discover() -> Value {
    let found = crate::toolchain::discover(&crate::toolchain::Places::here());
    json!({"toolchains": found.iter().map(crate::toolchain::Tool::to_json).collect::<Vec<_>>()})
}

/// `env.probe`: `{python, timeout_secs?}`; what [`crate::env::probe`] says.
fn env_probe(params: &JsonMap) -> RunnerResult<Value> {
    let python = params
        .get("python")
        .and_then(Value::as_str)
        .filter(|python| !python.is_empty())
        .ok_or_else(|| RunnerError::new(INVALID_PARAMS, "env.probe needs python, the interpreter's path"))?;
    let timeout = crate::params::timeout_secs(params, crate::env::PROBE_TIMEOUT, crate::env::MAX_PROBE_TIMEOUT)?;
    Ok(crate::env::probe(Path::new(python), timeout))
}

/// The `answer` of a runner that adds no operation.
fn none(op: &str, _params: &JsonMap) -> RunnerResult<Value> {
    Err(RunnerError::new(UNKNOWN_OP, format!("no operation {op:?}")))
}

/// How a session ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// `shutdown`, or the input closed.
    Done,
    /// The peer does not speak the protocol: told why, then left.
    Refused(String),
}

/// Debug builds only: `HOMOLOGY_RUNNER_FAULT` makes the runner misbehave
/// on `ping`, so the client's handling of a broken runner can be tested
/// against the real program. Release builds ignore it.
pub const FAULT_ENV: &str = "HOMOLOGY_RUNNER_FAULT";

/// How the runner misbehaves for a test ([`FAULT_ENV`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// End the process without answering.
    Exit,
    /// Answer with a line that is not a message.
    Garbage,
    /// Never answer.
    Silent,
    /// Say another protocol in `hello`.
    Protocol,
}

/// The fault a debug build was asked for, if any.
fn fault() -> Option<Fault> {
    if !cfg!(debug_assertions) {
        return None;
    }
    match std::env::var(FAULT_ENV).ok()?.as_str() {
        "exit" => Some(Fault::Exit),
        "garbage" => Some(Fault::Garbage),
        "silent" => Some(Fault::Silent),
        "protocol" => Some(Fault::Protocol),
        _ => None,
    }
}

/// Serve one session of `runner`.
pub fn serve(runner: &Runner, mut input: impl BufRead, mut output: impl Write) -> std::io::Result<Ended> {
    let fault = fault();
    let mut greeted = false;
    while let Some(line) = read_line(&mut input, MAX_LINE)? {
        let line = match line {
            Line::Text(line) => line,
            // Its id cannot be read without keeping it: say so and leave.
            Line::TooLong => {
                let error = RunnerError::new(MESSAGE_TOO_LARGE, format!("a line over {MAX_LINE} bytes"));
                send(&mut output, &Message::Event { event: "error".into(), data: error.to_json() })?;
                return Ok(Ended::Refused(error.message));
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request = match Request::parse(&line) {
            Ok(request) => request,
            Err((Some(id), error)) => {
                send(&mut output, &Message::Reply { id, outcome: Err(error) })?;
                continue;
            }
            // Nothing to answer to: say so as an event and leave.
            Err((None, error)) => {
                send(&mut output, &Message::Event { event: "error".into(), data: error.to_json() })?;
                return Ok(Ended::Refused(error.message));
            }
        };
        if !greeted && request.op != "hello" {
            let error = RunnerError::new(HELLO_REQUIRED, format!("the first request is hello, not {:?}", request.op));
            send(&mut output, &Message::Reply { id: request.id, outcome: Err(error.clone()) })?;
            return Ok(Ended::Refused(error.message));
        }
        match request.op.as_str() {
            "hello" => {
                let outcome = hello(runner, &request.params, fault == Some(Fault::Protocol));
                let refused = outcome.as_ref().err().map(|error| error.message.clone());
                send(&mut output, &Message::Reply { id: request.id, outcome })?;
                if let Some(message) = refused {
                    return Ok(Ended::Refused(message));
                }
                greeted = true;
            }
            "ping" => ping(&mut output, request.id, fault)?,
            "shutdown" => {
                send(&mut output, &Message::Reply { id: request.id, outcome: Ok(json!({})) })?;
                return Ok(Ended::Done);
            }
            op => send(&mut output, &Message::Reply { id: request.id, outcome: runner.dispatch(op, &request.params) })?,
        }
    }
    Ok(Ended::Done)
}

/// Answer `ping`, or misbehave the way a debug build was asked to
/// ([`FAULT_ENV`]).
fn ping(output: &mut impl Write, id: u64, fault: Option<Fault>) -> std::io::Result<()> {
    match fault {
        Some(Fault::Exit) => std::process::exit(3),
        Some(Fault::Garbage) => {
            writeln!(output, "this is not a message")?;
            output.flush()
        }
        Some(Fault::Silent) => loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        },
        _ => send(output, &Message::Reply { id, outcome: Ok(json!({})) }),
    }
}

/// `hello`: `{protocol, version}` from the caller; the runner's own, and
/// the operations it has, back.
fn hello(runner: &Runner, params: &JsonMap, claim_other: bool) -> RunnerResult<Value> {
    let theirs = params
        .get("protocol")
        .and_then(Value::as_u64)
        .ok_or_else(|| RunnerError::new(INVALID_PARAMS, "hello needs protocol, a non-negative integer"))?;
    if theirs != PROTOCOL {
        let version = params.get("version").and_then(Value::as_str).unwrap_or("?");
        return Err(RunnerError::new(
            PROTOCOL_MISMATCH,
            format!(
                "the caller ({version}) speaks protocol {theirs}, this runner ({} {}) speaks {PROTOCOL}",
                runner.name, runner.version
            ),
        ));
    }
    let protocol = if claim_other { PROTOCOL + 98 } else { PROTOCOL };
    Ok(json!({"protocol": protocol, "name": runner.name, "version": runner.version, "ops": runner.ops()}))
}

/// Write one message as a line and flush it, so the app reads it at once.
fn send(output: &mut impl Write, message: &Message) -> std::io::Result<()> {
    writeln!(output, "{}", message.to_line())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{INVALID_MESSAGE, RUNNER_INVALID_OUTPUT};

    /// Serve `lines`; how it ended and the messages it wrote.
    fn session(lines: &[&str]) -> (Ended, Vec<Message>) {
        let input = lines.join("\n");
        let mut output = Vec::new();
        let ended = serve(&EXTENDED, input.as_bytes(), &mut output).unwrap();
        let written = String::from_utf8(output).unwrap();
        (ended, written.lines().map(|line| Message::parse(line).unwrap()).collect())
    }

    /// A line over the limit ends the session with `message_too_large`,
    /// the line not kept; what came before it was answered.
    #[test]
    fn a_line_over_the_limit_ends_the_session() {
        let long = "x".repeat(MAX_LINE + 1);
        let hello = r#"{"id":1,"op":"hello","params":{"protocol":1}}"#;
        let (ended, messages) = session(&[hello, &long, r#"{"id":2,"op":"ping"}"#]);
        assert!(matches!(ended, Ended::Refused(_)));
        assert_eq!(messages.len(), 2, "{messages:?}");
        let Message::Event { event, data } = &messages[1] else { panic!("{messages:?}") };
        assert_eq!((event.as_str(), data["code"].as_str()), ("error", Some(MESSAGE_TOO_LARGE)));
    }

    /// A runner that adds `echo`, answered with its params.
    const EXTENDED: Runner =
        Runner { name: "test-runner", version: "9.0.0", ops: &["echo"], answer: echo, ..Runner::PLAIN };

    fn echo(_op: &str, params: &JsonMap) -> RunnerResult<Value> {
        Ok(Value::Object(params.clone()))
    }

    const HELLO: &str = r#"{"id":1,"op":"hello","params":{"protocol":1,"version":"test"}}"#;

    fn error_code(message: &Message) -> Option<&str> {
        match message {
            Message::Reply { outcome: Err(error), .. } => Some(error.code.as_str()),
            Message::Event { data, .. } => data.get("code").and_then(Value::as_str),
            _ => None,
        }
    }

    #[test]
    fn hello_ping_and_shutdown() {
        let (ended, replies) =
            session(&[HELLO, r#"{"id":2,"op":"ping"}"#, r#"{"id":3,"op":"shutdown"}"#, r#"{"id":4,"op":"ping"}"#]);
        assert_eq!(ended, Ended::Done);
        assert_eq!(replies.len(), 3, "nothing is answered after shutdown");
        let Message::Reply { id: 1, outcome: Ok(hello) } = &replies[0] else { panic!("{:?}", replies[0]) };
        assert_eq!((hello["protocol"].as_u64(), hello["version"].as_str()), (Some(PROTOCOL), Some("9.0.0")));
        let ops = hello["ops"].as_array().unwrap();
        assert!(ops.contains(&json!("fs.search")) && ops.contains(&json!("echo")), "{ops:?}");
        assert_eq!(replies[1], Message::Reply { id: 2, outcome: Ok(json!({})) });
    }

    #[test]
    fn an_added_operation_is_answered_by_its_runner() {
        let (_, replies) = session(&[HELLO, r#"{"id":2,"op":"echo","params":{"a":1}}"#, r#"{"id":3,"op":"fs.nope"}"#]);
        assert_eq!(replies[1], Message::Reply { id: 2, outcome: Ok(json!({"a": 1})) });
        assert_eq!(error_code(&replies[2]), Some(UNKNOWN_OP));
    }

    /// The toolchain operations and `code.exec`, through a session.
    #[test]
    fn toolchains_are_listed_and_a_command_runs_in_a_folder() {
        let folder = tempfile::tempdir().unwrap();
        let exec = json!({"id": 3, "op": "code.exec", "params": {"dir": folder.path().to_string_lossy(), "command": "echo ran"}});
        let (_, replies) = session(&[
            HELLO,
            r#"{"id":2,"op":"toolchain.discover"}"#,
            &exec.to_string(),
            r#"{"id":4,"op":"toolchain.probe","params":{"path":"relative"}}"#,
        ]);
        let Message::Reply { outcome: Ok(hello), .. } = &replies[0] else { panic!("{:?}", replies[0]) };
        for op in ["toolchain.discover", "toolchain.probe", "code.exec"] {
            assert!(hello["ops"].as_array().unwrap().contains(&json!(op)), "{op}");
        }
        let Message::Reply { id: 2, outcome: Ok(listed) } = &replies[1] else { panic!("{:?}", replies[1]) };
        assert!(listed["toolchains"].is_array(), "{listed}");
        let Message::Reply { id: 3, outcome: Ok(ran) } = &replies[2] else { panic!("{:?}", replies[2]) };
        assert_eq!((ran["code"].as_i64(), ran["stdout"].as_str().map(str::trim)), (Some(0), Some("ran")));
        assert_eq!(error_code(&replies[3]), Some(INVALID_PARAMS));
    }

    #[test]
    fn the_end_of_the_input_ends_the_session() {
        assert_eq!(session(&[HELLO]).0, Ended::Done);
        assert_eq!(session(&[]).0, Ended::Done);
    }

    #[test]
    fn the_first_request_must_be_hello() {
        let (ended, replies) = session(&[r#"{"id":1,"op":"ping"}"#, HELLO]);
        assert!(matches!(ended, Ended::Refused(_)));
        assert_eq!(replies.len(), 1);
        assert_eq!(error_code(&replies[0]), Some(HELLO_REQUIRED));
    }

    #[test]
    fn another_protocol_is_refused() {
        let (ended, replies) =
            session(&[r#"{"id":1,"op":"hello","params":{"protocol":2,"version":"9.9.9"}}"#, r#"{"id":2,"op":"ping"}"#]);
        assert!(matches!(ended, Ended::Refused(ref why) if why.contains("9.9.9")));
        assert_eq!((replies.len(), error_code(&replies[0])), (1, Some(PROTOCOL_MISMATCH)));
        let (_, replies) = session(&[r#"{"id":1,"op":"hello"}"#]);
        assert_eq!(error_code(&replies[0]), Some(INVALID_PARAMS));
    }

    #[test]
    fn an_unknown_op_or_a_bad_request_is_answered_and_the_session_goes_on() {
        let (ended, replies) =
            session(&[HELLO, r#"{"id":2,"op":"train"}"#, r#"{"id":3,"params":{}}"#, r#"{"id":4,"op":"ping"}"#]);
        assert_eq!(ended, Ended::Done);
        assert_eq!(
            replies.iter().map(error_code).collect::<Vec<_>>(),
            [None, Some(UNKNOWN_OP), Some(INVALID_MESSAGE), None]
        );
    }

    #[test]
    fn a_line_without_an_id_ends_the_session_with_an_error_event() {
        let (ended, replies) = session(&[HELLO, "garbage", r#"{"id":3,"op":"ping"}"#]);
        assert!(matches!(ended, Ended::Refused(_)));
        assert_eq!(replies.len(), 2);
        assert!(matches!(&replies[1], Message::Event { event, .. } if event == "error"));
        assert_eq!(error_code(&replies[1]), Some(INVALID_MESSAGE));
        assert_ne!(error_code(&replies[1]), Some(RUNNER_INVALID_OUTPUT));
    }
}
