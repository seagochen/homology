//! The messages a caller and the runner exchange: one JSON object per
//! line, UTF-8, over the runner's stdin and stdout.
//!
//! ```text
//! {"id": 7, "op": "ping", "params": {}}                              request (caller → runner)
//! {"id": 7, "ok": true, "result": {...}}                              reply
//! {"id": 7, "ok": false, "error": {"code": "...", "message": "..."}}  failure
//! {"event": "run.progress", "data": {...}}                            event (runner → caller, no id)
//! ```
//!
//! A message never carries a password, a key or a token.

use std::fmt;

use serde_json::{json, Value};

use crate::JsonMap;

/// The protocol both sides speak. A runner refuses a `hello` naming
/// another one ([`PROTOCOL_MISMATCH`]); the caller then deploys the runner
/// of its own version.
pub const PROTOCOL: u64 = 1;

/// This crate's version. A program that serves its own runner on top of it
/// names its own version ([`crate::server::Runner`]).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Stable error codes (a message is for people; these are for programs).

/// The first request was not `hello`.
pub const HELLO_REQUIRED: &str = "hello_required";
/// The two sides speak different protocols.
pub const PROTOCOL_MISMATCH: &str = "protocol_mismatch";
/// A line that is not a request (not JSON, not an object, no id or op).
pub const INVALID_MESSAGE: &str = "invalid_message";
/// A request for an operation this runner does not have.
pub const UNKNOWN_OP: &str = "unknown_op";
/// A request's parameters are wrong.
pub const INVALID_PARAMS: &str = "invalid_params";
/// The runner could not be started.
pub const RUNNER_START_FAILED: &str = "runner_start_failed";
/// The runner ended (or closed its output) before answering.
pub const RUNNER_EXITED: &str = "runner_exited";
/// The runner wrote a line that is not a message.
pub const RUNNER_INVALID_OUTPUT: &str = "runner_invalid_output";
/// The runner did not answer in time.
pub const RUNNER_TIMEOUT: &str = "runner_timeout";
/// Writing to the runner failed.
pub const RUNNER_IO: &str = "runner_io";

/// A failure with a stable `code`, from either side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerError {
    pub code: String,
    pub message: String,
}

impl RunnerError {
    /// A failure with a stable `code` and a message for people.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        RunnerError { code: code.to_string(), message: message.into() }
    }

    /// As the `error` of a failed reply.
    pub fn to_json(&self) -> Value {
        json!({"code": self.code, "message": self.message})
    }

    /// From the `error` of a failed reply; a malformed one is itself an
    /// invalid message.
    pub fn from_json(value: &Value) -> Self {
        match (value.get("code").and_then(Value::as_str), value.get("message").and_then(Value::as_str)) {
            (Some(code), message) => RunnerError::new(code, message.unwrap_or_default()),
            _ => RunnerError::new(RUNNER_INVALID_OUTPUT, format!("a failed reply without an error code: {value}")),
        }
    }
}

impl fmt::Display for RunnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RunnerError {}

/// What the runner's operations return.
pub type RunnerResult<T> = Result<T, RunnerError>;

/// A request: the caller asks the runner to do `op`.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: u64,
    pub op: String,
    pub params: JsonMap,
}

impl Request {
    /// The request as one JSON line (without the newline).
    pub fn to_line(&self) -> String {
        json!({"id": self.id, "op": self.op, "params": self.params}).to_string()
    }

    /// One line from the caller. A line that is not a request is
    /// [`INVALID_MESSAGE`], with the id when one could be read, so that the
    /// failure can still be answered to it.
    pub fn parse(line: &str) -> Result<Request, (Option<u64>, RunnerError)> {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| (None, RunnerError::new(INVALID_MESSAGE, format!("not JSON: {error}"))))?;
        let id = value.get("id").and_then(Value::as_u64);
        let invalid = |message: &str| (id, RunnerError::new(INVALID_MESSAGE, message.to_string()));
        let Value::Object(mut object) = value else { return Err(invalid("a request is a JSON object")) };
        let id = id.ok_or_else(|| invalid("a request needs a non-negative integer id"))?;
        let op = match object.remove("op") {
            Some(Value::String(op)) if !op.is_empty() => op,
            _ => return Err(invalid("a request needs an op")),
        };
        let params = match object.remove("params") {
            None | Some(Value::Null) => JsonMap::new(),
            Some(Value::Object(params)) => params,
            Some(_) => return Err(invalid("params is an object")),
        };
        Ok(Request { id, op, params })
    }
}

/// A line from the runner: the reply to a request, or an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Reply { id: u64, outcome: RunnerResult<Value> },
    Event { event: String, data: Value },
}

impl Message {
    /// The message as one JSON line (without the newline): a reply's
    /// `ok` with its `result` or `error`, or an event with its `data`.
    pub fn to_line(&self) -> String {
        match self {
            Message::Reply { id, outcome: Ok(result) } => json!({"id": id, "ok": true, "result": result}),
            Message::Reply { id, outcome: Err(error) } => json!({"id": id, "ok": false, "error": error.to_json()}),
            Message::Event { event, data } => json!({"event": event, "data": data}),
        }
        .to_string()
    }

    /// One line from the runner; anything else is [`RUNNER_INVALID_OUTPUT`].
    pub fn parse(line: &str) -> RunnerResult<Message> {
        let invalid = |why: &str| RunnerError::new(RUNNER_INVALID_OUTPUT, format!("{why}: {}", excerpt(line)));
        let value: Value =
            serde_json::from_str(line).map_err(|_| invalid("the runner wrote a line that is not JSON"))?;
        if let Some(event) = value.get("event").and_then(Value::as_str) {
            return Ok(Message::Event {
                event: event.to_string(),
                data: value.get("data").cloned().unwrap_or(Value::Null),
            });
        }
        let id = value.get("id").and_then(Value::as_u64).ok_or_else(|| invalid("a reply without an id"))?;
        let outcome = match value.get("ok") {
            Some(Value::Bool(true)) => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            Some(Value::Bool(false)) => Err(RunnerError::from_json(value.get("error").unwrap_or(&Value::Null))),
            _ => return Err(invalid("a reply without ok")),
        };
        Ok(Message::Reply { id, outcome })
    }
}

/// The start of `line`, for an error message: enough to recognise it,
/// never a whole log.
fn excerpt(line: &str) -> String {
    const MAX: usize = 120;
    match line.char_indices().nth(MAX) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_goes_through_a_line_and_back() {
        let mut params = JsonMap::new();
        params.insert("x".into(), json!(1));
        let request = Request { id: 3, op: "ping".into(), params };
        assert_eq!(Request::parse(&request.to_line()).unwrap(), request);
    }

    #[test]
    fn a_request_may_leave_out_params() {
        assert_eq!(Request::parse(r#"{"id":1,"op":"ping"}"#).unwrap().params, JsonMap::new());
    }

    #[test]
    fn what_is_not_a_request_is_invalid_with_the_id_when_there_is_one() {
        for (line, id) in [
            ("nope", None),
            ("[1]", None),
            (r#"{"op":"ping"}"#, None),
            (r#"{"id":-1,"op":"ping"}"#, None),
            (r#"{"id":4}"#, Some(4)),
            (r#"{"id":5,"op":""}"#, Some(5)),
            (r#"{"id":6,"op":"ping","params":[1]}"#, Some(6)),
        ] {
            let (found, error) = Request::parse(line).unwrap_err();
            assert_eq!((found, error.code.as_str()), (id, INVALID_MESSAGE), "{line}");
        }
    }

    #[test]
    fn replies_and_events_go_through_a_line_and_back() {
        for message in [
            Message::Reply { id: 1, outcome: Ok(json!({"a": 1})) },
            Message::Reply { id: 2, outcome: Err(RunnerError::new(UNKNOWN_OP, "no such op")) },
            Message::Event { event: "run.progress".into(), data: json!({"done": 3}) },
        ] {
            assert_eq!(Message::parse(&message.to_line()).unwrap(), message);
        }
    }

    #[test]
    fn what_is_not_a_message_is_invalid_output_and_shows_only_its_start() {
        for line in ["hello", "{}", r#"{"id":1}"#, r#"{"ok":true}"#] {
            assert_eq!(Message::parse(line).unwrap_err().code, RUNNER_INVALID_OUTPUT, "{line}");
        }
        let long = "x".repeat(10_000);
        assert!(Message::parse(&long).unwrap_err().message.len() < 300);
        let failed = Message::parse(r#"{"id":1,"ok":false,"error":{}}"#).unwrap();
        assert!(
            matches!(failed, Message::Reply { outcome: Err(RunnerError { ref code, .. }), .. } if code == RUNNER_INVALID_OUTPUT)
        );
    }
}
