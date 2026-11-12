//! Completing code in the environment it will run in (homology#14): the
//! completion ([`crate::python`]: `homology_complete.py` and the jedi and
//! parso it bundles) is written once per version into the cache's
//! `python/` and run with the environment's interpreter, so the packages
//! installed there complete as they are. The code is read, never run.
//!
//! `code.complete {python, text, line, column, path?, root?, limit?, timeout_secs?}`
//! answers `{completions: [{name, complete, type}], python}`: `line` from 1,
//! `column` from 0 (characters); `root` the folder the code is in, `path`
//! the file's place in it.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::{RunnerError, RunnerResult, INVALID_PARAMS};
use crate::server::Runner;
use crate::JsonMap;

/// The completion could not be run, or said nothing it should.
pub const COMPLETE_FAILED: &str = "complete_failed";
/// It did not finish in time.
pub const COMPLETE_TIMEOUT: &str = "complete_timeout";

/// How long a completion may take, and the default: the first one in an
/// environment reads its packages, the next ones are quicker.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(120);
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// The most completions given, and the default.
pub const MAX_LIMIT: u64 = 500;
pub const LIMIT: u64 = 50;

/// A completion asked for: `code.complete`'s params, checked.
#[derive(Clone, Debug, PartialEq)]
pub struct Asked {
    /// The environment's interpreter.
    pub python: PathBuf,
    pub text: String,
    /// From 1.
    pub line: u64,
    /// From 0, in characters.
    pub column: u64,
    /// The folder the code is in, and the file's place in it.
    pub root: Option<PathBuf>,
    pub path: Option<String>,
    pub limit: u64,
    pub timeout: Duration,
}

/// A whole number at `key`, from `low`; `default` when absent.
fn number(params: &JsonMap, key: &str, low: u64, high: u64, default: Option<u64>) -> RunnerResult<u64> {
    match params.get(key) {
        None | Some(Value::Null) => {
            default.ok_or_else(|| RunnerError::new(INVALID_PARAMS, format!("code.complete needs {key}")))
        }
        Some(value) => value
            .as_u64()
            .filter(|number| (low..=high).contains(number))
            .ok_or_else(|| RunnerError::new(INVALID_PARAMS, format!("{key} is a whole number from {low} to {high}"))),
    }
}

/// `code.complete`'s params, checked.
pub fn asked(params: &JsonMap) -> RunnerResult<Asked> {
    let python = crate::params::text(params, "python")?;
    if python.is_empty() {
        return Err(RunnerError::new(INVALID_PARAMS, "code.complete needs python, the interpreter's path"));
    }
    let text = crate::params::text(params, "text")?;
    if text.len() > crate::check::MAX_TEXT {
        return Err(RunnerError::new(
            INVALID_PARAMS,
            format!("the text is larger than {} bytes", crate::check::MAX_TEXT),
        ));
    }
    let root = match params.get("root") {
        None | Some(Value::Null) => None,
        Some(_) => Some(crate::params::host_path(crate::params::text(params, "root")?)?),
    };
    let path = match params.get("path") {
        None | Some(Value::Null) => None,
        Some(_) => {
            let path = crate::params::text(params, "path")?;
            if !crate::fs::is_relative(path) {
                return Err(RunnerError::new(INVALID_PARAMS, format!("{path:?} is not a relative path inside root")));
            }
            Some(path.to_string())
        }
    };
    Ok(Asked {
        python: PathBuf::from(python),
        text: text.to_string(),
        line: number(params, "line", 1, u64::from(u32::MAX), None)?,
        column: number(params, "column", 0, u64::from(u32::MAX), None)?,
        root,
        path,
        limit: number(params, "limit", 1, MAX_LIMIT, Some(LIMIT))?,
        timeout: crate::params::timeout_secs(params, TIMEOUT, MAX_TIMEOUT)?,
    })
}

/// The completion `asked` for, with the completion of `runner`.
pub fn run(runner: &Runner, asked: &Asked) -> RunnerResult<Value> {
    let failed = |message: String| RunnerError::new(COMPLETE_FAILED, message);
    let cache = (runner.cache)()?;
    let completer = crate::python::place(&cache, runner.version, runner.python).map_err(failed)?;
    let request = json!({
        "text": asked.text,
        "line": asked.line,
        "column": asked.column,
        "root": asked.root.as_ref().map(|root| root.to_string_lossy()),
        "path": asked.path,
        "limit": asked.limit,
    });
    let mut command = crate::sys::command(&asked.python);
    command
        .arg("-B")
        .arg(completer.join("homology_complete.py"))
        .current_dir(asked.root.as_deref().filter(|root| root.is_dir()).unwrap_or(&cache))
        .env("PYTHONDONTWRITEBYTECODE", "1");
    let output =
        crate::env::stdout_of(command, Some(request.to_string().as_bytes()), asked.timeout).map_err(|why| {
            if why.contains("did not finish") {
                RunnerError::new(COMPLETE_TIMEOUT, format!("the completion {why}"))
            } else {
                failed(format!("the completion could not run with {}: {why}", asked.python.display()))
            }
        })?;
    let answer: Value = serde_json::from_str(output.trim())
        .map_err(|error| failed(format!("the completion's answer is not JSON: {error}")))?;
    if let Some(why) = answer.get("unsupported").and_then(Value::as_str) {
        return Err(failed(why.to_string()));
    }
    if let Some(error) = answer.get("error").and_then(Value::as_str) {
        return Err(RunnerError::new(INVALID_PARAMS, format!("line and column: {error}")));
    }
    if !answer["completions"].is_array() {
        return Err(failed("the completion gave no completions list".into()));
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `params` as a request's.
    fn params(value: Value) -> JsonMap {
        match value {
            Value::Object(map) => map,
            _ => JsonMap::new(),
        }
    }

    #[test]
    fn what_is_asked_is_checked() {
        let good = asked(&params(json!({"python": "/py", "text": "os.", "line": 1, "column": 3}))).unwrap();
        assert_eq!((good.line, good.column, good.limit, good.timeout), (1, 3, LIMIT, TIMEOUT));
        assert!(good.root.is_none() && good.path.is_none());
        for bad in [
            json!({"text": "x", "line": 1, "column": 0}),
            json!({"python": "", "text": "x", "line": 1, "column": 0}),
            json!({"python": "/py", "line": 1, "column": 0}),
            json!({"python": "/py", "text": "x", "line": 0, "column": 0}),
            json!({"python": "/py", "text": "x", "line": 1}),
            json!({"python": "/py", "text": "x", "line": 1, "column": -1}),
            json!({"python": "/py", "text": "x", "line": 1, "column": 0, "limit": 0}),
            json!({"python": "/py", "text": "x", "line": 1, "column": 0, "path": "../x.py"}),
            json!({"python": "/py", "text": "x", "line": 1, "column": 0, "root": "relative"}),
        ] {
            assert_eq!(asked(&params(bad.clone())).unwrap_err().code, INVALID_PARAMS, "{bad}");
        }
    }
}
