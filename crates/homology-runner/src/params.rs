//! Request params checked: the helpers the operation modules share to read
//! paths, text and lists from a request's params. They depend on no session,
//! so the modules [`crate::server`] dispatches to need not depend on it.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::protocol::{RunnerError, RunnerResult, INVALID_PARAMS};
use crate::JsonMap;

/// A path the caller names on the runner's computer: absolute, or in the
/// home folder ([`homology_sys::home_dir`]: `~`, `~/…`, `~\…`). The one rule
/// every operation reads paths by.
pub fn host_path(text: &str) -> RunnerResult<PathBuf> {
    let home = || homology_sys::home_dir().ok_or_else(|| RunnerError::new(INVALID_PARAMS, "no home folder for ~"));
    let path = match text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
        Some(rest) => home()?.join(rest),
        None if text == "~" => home()?,
        None => PathBuf::from(text),
    };
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(RunnerError::new(INVALID_PARAMS, format!("{text:?} is not an absolute path")))
    }
}

/// Those of the environment variables `names` that are set and not empty,
/// with their values: what a computer's places are found from.
pub fn set_vars(names: &[&str]) -> Vec<(String, String)> {
    names
        .iter()
        .filter_map(|name| {
            std::env::var(name).ok().filter(|value| !value.is_empty()).map(|value| (name.to_string(), value))
        })
        .collect()
}

/// The folders of this process's `PATH`, in order.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default()
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

/// `timeout_secs` in `params`: `default` when absent or null, else 1 s to
/// `max` (`invalid_params` otherwise), so every operation bounds its time
/// the same way and says so in the same words.
pub fn timeout_secs(params: &JsonMap, default: Duration, max: Duration) -> RunnerResult<Duration> {
    match params.get("timeout_secs") {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|secs| (1..=max.as_secs()).contains(secs))
            .map(Duration::from_secs)
            .ok_or_else(|| RunnerError::new(INVALID_PARAMS, format!("timeout_secs is 1 to {}", max.as_secs()))),
    }
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

    /// `~`, `~/…` and `~\…` are the home folder ([`homology_sys::home_dir`])
    /// for every operation, `files.*` too (homology#27).
    #[test]
    fn a_path_in_the_home_folder_is_expanded() {
        let Some(home) = homology_sys::home_dir() else { return };
        assert_eq!(host_path("~").unwrap(), home);
        assert_eq!(host_path("~/x").unwrap(), home.join("x"));
        assert_eq!(host_path("~\\x").unwrap(), home.join("x"));
        assert_eq!(host_path("~/a/b").unwrap(), home.join("a/b"));
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

    #[test]
    fn a_timeout_is_the_default_when_absent_and_refused_outside_its_bounds() {
        let default = Duration::from_secs(15);
        let max = Duration::from_secs(600);
        for absent in [json!({}), json!({"timeout_secs": null})] {
            assert_eq!(timeout_secs(&params(absent), default, max).unwrap(), default);
        }
        for secs in [1, 600] {
            let asked = params(json!({"timeout_secs": secs}));
            assert_eq!(timeout_secs(&asked, default, max).unwrap(), Duration::from_secs(secs));
        }
        for wrong in [json!(0), json!(601), json!(-1), json!(1.5), json!("30")] {
            let error = timeout_secs(&params(json!({"timeout_secs": wrong})), default, max).unwrap_err();
            assert_eq!(error.code, INVALID_PARAMS);
            assert_eq!(error.message, "timeout_secs is 1 to 600");
        }
    }
}
