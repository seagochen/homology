//! Request params checked: the helpers the operation modules share to read
//! paths, text and lists from a request's params. They depend on no session,
//! so the modules [`crate::server`] dispatches to need not depend on it.

use std::path::PathBuf;

use serde_json::Value;

use crate::protocol::{RunnerError, RunnerResult, INVALID_PARAMS};
use crate::JsonMap;

/// A path the caller names on the runner's computer: absolute, or `~/…`
/// in the home folder.
pub fn host_path(text: &str) -> RunnerResult<PathBuf> {
    let expanded = match text.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .map_err(|_| RunnerError::new(INVALID_PARAMS, "no HOME for ~/"))?,
        None if text == "~" => std::env::var("HOME").map_err(|_| RunnerError::new(INVALID_PARAMS, "no HOME for ~"))?,
        None => text.to_string(),
    };
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(RunnerError::new(INVALID_PARAMS, format!("{text:?} is not an absolute path")))
    }
}

/// The list of text at `key`; empty when absent (`invalid_params` when it is
/// not a list of text).
pub fn strings(params: &JsonMap, key: &str) -> RunnerResult<Vec<String>> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().map(|item| item.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
        .unwrap_or(Some(Vec::new()))
        .ok_or_else(|| RunnerError::new(INVALID_PARAMS, format!("{key} is a list of text")))
}

/// The text at `key` (`invalid_params` when it is missing or not text).
pub fn text<'p>(params: &'p JsonMap, key: &str) -> RunnerResult<&'p str> {
    params.get(key).and_then(Value::as_str).ok_or_else(|| RunnerError::new(INVALID_PARAMS, format!("{key} is text")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `value`'s fields, as a request's params.
    fn params(value: Value) -> JsonMap {
        match value {
            Value::Object(map) => map,
            _ => panic!("params are an object"),
        }
    }

    #[test]
    fn a_path_in_the_home_folder_is_expanded() {
        let Ok(home) = std::env::var("HOME") else { return };
        assert_eq!(host_path("~").unwrap(), PathBuf::from(&home));
        assert_eq!(host_path("~/x").unwrap(), PathBuf::from(format!("{home}/x")));
    }

    #[test]
    fn an_absolute_path_is_kept_and_a_relative_one_refused() {
        let absolute = if cfg!(windows) { r"C:\work" } else { "/work" };
        assert_eq!(host_path(absolute).unwrap(), PathBuf::from(absolute));
        for relative in ["work", "./work", "~user/work"] {
            assert_eq!(host_path(relative).unwrap_err().code, INVALID_PARAMS, "{relative}");
        }
    }

    #[test]
    fn text_that_is_missing_or_not_text_is_refused() {
        let asked = params(json!({"path": "/x", "count": 3}));
        assert_eq!(text(&asked, "path").unwrap(), "/x");
        for key in ["count", "missing"] {
            let error = text(&asked, key).unwrap_err();
            assert_eq!(error.code, INVALID_PARAMS);
            assert_eq!(error.message, format!("{key} is text"));
        }
    }

    #[test]
    fn a_missing_list_is_empty_and_a_list_with_other_than_text_refused() {
        let asked = params(json!({"names": ["a", "b"], "mixed": ["a", 1]}));
        assert_eq!(strings(&asked, "names").unwrap(), ["a", "b"]);
        assert!(strings(&asked, "missing").unwrap().is_empty());
        assert_eq!(strings(&asked, "mixed").unwrap_err().code, INVALID_PARAMS);
    }
}
