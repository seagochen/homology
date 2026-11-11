//! Checking code in the environment it will run in: the checker
//! ([`crate::python`]: `homology_check.py` and the pyflakes it bundles,
//! with the rules of the program serving the runner) is written once per
//! version into the cache's `python/`, and run with the environment's
//! interpreter on the files given, written for the while into the cache's
//! `check/`. It reads the code and looks imports up without importing
//! them: the user's code never runs.
//!
//! `code.check {python, files: [{path, text}], listing?: [path], timeout_secs?}`
//! answers `{problems: [{file, line, col, code, message, tool, severity}],
//! tools: {pyflakes, imports, ruff, pylint, <each rules module's tool>}, python}`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::{RunnerError, RunnerResult, INVALID_PARAMS};
use crate::server::Runner;
use crate::JsonMap;

/// The checker could not be run, or said nothing it should.
pub const CHECK_FAILED: &str = "check_failed";
/// It did not finish in time.
pub const CHECK_TIMEOUT: &str = "check_timeout";

/// How long a check may take, and the default.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// Files checked at once, and the text of each at most.
pub const MAX_FILES: usize = 200;
pub const MAX_TEXT: usize = 2 * 1024 * 1024;

/// The check itself could not run (`check_failed`).
fn failed(message: impl Into<String>) -> RunnerError {
    RunnerError::new(CHECK_FAILED, message)
}

/// The files to check, as `(path, text)`, checked for their shape.
pub fn files_of(value: Option<&Value>) -> RunnerResult<Vec<(String, String)>> {
    let listed =
        value.and_then(Value::as_array).ok_or_else(|| RunnerError::new(INVALID_PARAMS, "files are [{path, text}]"))?;
    if listed.is_empty() || listed.len() > MAX_FILES {
        return Err(RunnerError::new(INVALID_PARAMS, format!("1 to {MAX_FILES} files are checked at once")));
    }
    listed
        .iter()
        .map(|file| {
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| RunnerError::new(INVALID_PARAMS, "a file has a path"))?;
            let text = file
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| RunnerError::new(INVALID_PARAMS, "a file has its text"))?;
            if !crate::fs::is_relative(path) {
                return Err(RunnerError::new(
                    INVALID_PARAMS,
                    format!("{path:?} is not a relative path inside the folder"),
                ));
            }
            if text.len() > MAX_TEXT {
                return Err(RunnerError::new(INVALID_PARAMS, format!("{path} is larger than {MAX_TEXT} bytes")));
            }
            Ok((path.to_string(), text.to_string()))
        })
        .collect()
}

/// A check asked for: `code.check`'s params, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The environment's interpreter.
    pub python: PathBuf,
    /// The files to check, `(path, text)`.
    pub files: Vec<(String, String)>,
    /// Every file of the directory: which imports are its own modules.
    pub listing: Vec<String>,
    pub timeout: Duration,
}

/// `code.check`'s params, checked.
pub fn asked(params: &JsonMap) -> RunnerResult<Asked> {
    let python = params
        .get("python")
        .and_then(Value::as_str)
        .filter(|python| !python.is_empty())
        .ok_or_else(|| RunnerError::new(INVALID_PARAMS, "code.check needs python, the interpreter's path"))?;
    let files = files_of(params.get("files"))?;
    let timeout = crate::params::timeout_secs(params, TIMEOUT, MAX_TIMEOUT)?;
    Ok(Asked { python: PathBuf::from(python), files, listing: crate::params::strings(params, "listing")?, timeout })
}

/// Check `files` with the interpreter at `python` and the checker of
/// `runner`; `listing` is every file of the directory (which imports are
/// its own modules).
pub fn run(
    runner: &Runner,
    python: &Path,
    files: &[(String, String)],
    listing: &[String],
    timeout: Duration,
) -> RunnerResult<Value> {
    let cache = (runner.cache)()?;
    let checker = crate::python::place(&cache, runner.version, runner.python).map_err(failed)?;
    let checks = cache.join("check");
    crate::sys::private_dir(&checks).map_err(|error| failed(format!("cannot make {}: {error}", checks.display())))?;
    let work = checks.join(format!("{}-{}", std::process::id(), crate::python::unique()));
    let answer = (|| {
        for (path, text) in files {
            let file = path.split('/').fold(work.clone(), |file, part| file.join(part));
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| failed(format!("cannot make {}: {error}", parent.display())))?;
            }
            std::fs::write(&file, text).map_err(|error| failed(format!("cannot write {}: {error}", file.display())))?;
        }
        let paths: Vec<&String> = files.iter().map(|(path, _)| path).collect();
        let asked = json!({
            "root": work.to_string_lossy(),
            "files": paths,
            "listing": listing,
            "timeout": (timeout.as_secs() / 2).max(10),
            "rules": runner.rules,
        });
        let mut command = crate::sys::command(python);
        command.arg("-B").arg(checker.join("homology_check.py")).current_dir(&work).env("PYTHONDONTWRITEBYTECODE", "1");
        let output =
            crate::env::run_with(&mut command, Some(asked.to_string().into_bytes()), timeout).map_err(|why| {
                if why.contains("did not finish") {
                    RunnerError::new(CHECK_TIMEOUT, format!("the check {why}"))
                } else {
                    failed(format!("the checker could not run with {}: {why}", python.display()))
                }
            })?;
        let answer: Value = serde_json::from_str(output.trim())
            .map_err(|error| failed(format!("the checker's answer is not JSON: {error}")))?;
        if !answer["problems"].is_array() {
            return Err(failed("the checker gave no problems list"));
        }
        Ok(answer)
    })();
    let _ = std::fs::remove_dir_all(&work);
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_checked_for_their_shape() {
        assert!(files_of(Some(&json!([{"path": "src/a.py", "text": "x = 1\n"}]))).is_ok());
        for bad in [json!([]), json!([{"path": "../a.py", "text": ""}]), json!([{"path": "a.py"}]), json!("a.py")] {
            assert_eq!(files_of(Some(&bad)).unwrap_err().code, INVALID_PARAMS, "{bad}");
        }
    }
}
