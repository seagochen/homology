//! `code.complete` through the real completion (the bundled jedi) and the
//! system's `python3` in a bare virtual environment: the standard library
//! and the folder's own modules complete, a position past the text is the
//! caller's mistake, and a missing interpreter says so. Unix: `python3`.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use homology_runner::complete::{self, COMPLETE_FAILED};
use homology_runner::protocol::INVALID_PARAMS;
use homology_runner::server::Runner;
use homology_runner::RunnerResult;
use serde_json::{json, Value};

/// The cache folder of the test that runs.
static CACHE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The tests here share [`CACHE`]: one at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn cache() -> RunnerResult<PathBuf> {
    Ok(CACHE.lock().unwrap().clone().unwrap())
}

/// The runner, its cache in the test's folder.
const RUNNER: Runner = Runner { cache, ..Runner::PLAIN };

/// A `python3` with nothing installed: a virtual environment of the system's.
fn bare_python(dir: &Path) -> PathBuf {
    let system = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 on PATH");
    let made = std::process::Command::new(system).args(["-m", "venv", "--without-pip"]).arg(dir).status().unwrap();
    assert!(made.success(), "python3 -m venv --without-pip failed");
    dir.join("bin").join("python3")
}

/// The names `code.complete` gives for `params`.
fn names(answer: &Value) -> Vec<String> {
    answer["completions"].as_array().unwrap().iter().map(|item| item["name"].as_str().unwrap().to_string()).collect()
}

fn ask(params: Value) -> RunnerResult<Value> {
    let Value::Object(params) = params else { panic!("an object") };
    complete::run(&RUNNER, &complete::asked(&params)?)
}

#[test]
fn the_standard_library_and_the_folders_own_modules_complete() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    *CACHE.lock().unwrap() = Some(dir.path().join("cache"));
    let python = bare_python(&dir.path().join("env"));
    let python = python.to_string_lossy();

    let answer = ask(json!({"python": python, "text": "import os\nos.pa", "line": 2, "column": 5})).unwrap();
    assert!(names(&answer).contains(&"path".to_string()), "{answer}");
    let path = answer["completions"].as_array().unwrap().iter().find(|item| item["name"] == "path").unwrap();
    // What to insert; its kind is jedi's (a module, or a statement naming one).
    assert_eq!(path["complete"].as_str(), Some("th"));
    assert!(path["type"].as_str().is_some_and(|kind| !kind.is_empty()), "{path}");
    assert!(answer["python"].as_str().is_some_and(|version| version.starts_with('3')));

    // What the file defines, and a module beside it in the folder.
    let root = dir.path().join("model");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("helpers.py"), "def tidy_labels(rows):\n    return rows\n").unwrap();
    let text = "import helpers\n\ndef build_model(context):\n    return {}\n\nhelpers.ti\nbuild_";
    let answer = ask(json!({"python": python, "text": text, "line": 6, "column": 10, "root": root.to_string_lossy(), "path": "annotarycat_interfaces.py"})).unwrap();
    assert_eq!(names(&answer), ["tidy_labels"], "{answer}");
    let answer = ask(json!({"python": python, "text": text, "line": 7, "column": 6, "limit": 1})).unwrap();
    assert_eq!(names(&answer), ["build_model"], "{answer}");
    // The bundled packages are not the environment's.
    let answer = ask(json!({"python": python, "text": "import jed", "line": 1, "column": 10})).unwrap();
    assert!(!names(&answer).iter().any(|name| name == "jedi"), "{answer}");
}

#[test]
fn a_position_past_the_text_or_no_interpreter_is_said() {
    let _one = ONE_AT_A_TIME.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    *CACHE.lock().unwrap() = Some(dir.path().join("cache"));
    let python = bare_python(&dir.path().join("env"));
    let past = ask(json!({"python": python.to_string_lossy(), "text": "x = 1", "line": 9, "column": 0})).unwrap_err();
    assert_eq!(past.code, INVALID_PARAMS, "{past:?}");
    let missing = ask(
        json!({"python": dir.path().join("nowhere/python3").to_string_lossy(), "text": "x", "line": 1, "column": 1}),
    )
    .unwrap_err();
    assert_eq!(missing.code, COMPLETE_FAILED, "{missing:?}");
}
