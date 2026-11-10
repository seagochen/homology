//! `code.check` through the real checker and the system's `python3`: what
//! pyflakes and the import lookup find, that the user's code never runs,
//! ruff and pylint run only when installed (here stand-ins on PYTHONPATH
//! that answer as they do), and the rules a program adds. Unix: `python3`.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use homology_runner::server::Runner;
use homology_runner::{check, RunnerResult};
use serde_json::{json, Value};

/// The tests here set the runner's cache folder and look into it: one at
/// a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The cache folder of the test that runs.
static CACHE: Mutex<Option<PathBuf>> = Mutex::new(None);

fn use_cache(dir: &Path) {
    *CACHE.lock().unwrap() = Some(dir.to_path_buf());
}

fn cache() -> RunnerResult<PathBuf> {
    Ok(CACHE.lock().unwrap().clone().unwrap())
}

/// A rules module: every `TODO` comment of the files, unless none has one.
const TODO_RULES: &[u8] = b"TOOL = 'todo'\n\n\ndef check(texts):\n    found = [{'file': path, 'line': n + 1, 'col': line.index('TODO') + 1, 'code': 'Todo', 'message': 'a TODO'} for path, text in texts.items() for n, line in enumerate(text.splitlines()) if 'TODO' in line]\n    return found or None\n";

/// A runner that adds the rules above.
const RUNNER: Runner =
    Runner { cache, python: &[("todo_rules.py", TODO_RULES)], rules: &["todo_rules"], ..Runner::PLAIN };

fn python() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("python3"))
        .find(|path| path.is_file())
        .expect("python3 on PATH")
}

/// A `python3` with nothing installed: a virtual environment of the
/// system's, without its packages or pip, so whether ruff or pylint is in
/// the developer's own environment does not change what the test sees.
fn bare_python(dir: &Path) -> PathBuf {
    let made = std::process::Command::new(python())
        .args(["-m", "venv", "--without-pip"])
        .arg(dir)
        .status()
        .expect("python3 -m venv");
    assert!(made.success(), "python3 -m venv --without-pip failed");
    dir.join("bin").join("python3")
}

fn files(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter().map(|(path, text)| (path.to_string(), text.to_string())).collect()
}

fn codes(answer: &Value, file: &str) -> Vec<(u64, String)> {
    answer["problems"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|problem| problem["file"] == file)
        .map(|problem| (problem["line"].as_u64().unwrap(), problem["code"].as_str().unwrap().to_string()))
        .collect()
}

/// A stand-in for a tool as `python -m <name>` runs it: it prints `answer`.
fn stand_in(dir: &Path, name: &str, answer: &Value) {
    let package = dir.join(name);
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("__init__.py"), "").unwrap();
    std::fs::write(
        package.join("__main__.py"),
        format!("import sys\nsys.stdout.write({})\n", serde_json::to_string(&answer.to_string()).unwrap()),
    )
    .unwrap();
}

#[test]
fn problems_imports_no_side_effects_and_the_tools_installed() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = tempfile::tempdir().unwrap();
    use_cache(cache.path());
    let marker = cache.path().join("ran.txt");
    let side_effect = format!("open({:?}, 'w').write('ran')\n", marker.to_string_lossy());
    let train = format!(
        "import os\nimport json\nimport torch_not_here_xyz.nn\nfrom src import helpers\nimport utils\ntry:\n    import optional_xyz\nexcept ImportError:\n    optional_xyz = None\n{side_effect}print(undefined_name)\n"
    );
    let listing = ["src/train.py", "src/utils.py", "src/helpers.py", "src/broken.py"].map(String::from);
    let checked = files(&[("src/train.py", &train), ("src/broken.py", "def f(:\n    pass\n")]);
    let environment = tempfile::tempdir().unwrap();
    let python = bare_python(environment.path());

    let answer = check::run(&RUNNER, &python, &checked, &listing, Duration::from_secs(60)).unwrap();
    assert_eq!(answer["tools"]["pyflakes"], "ran", "{answer}");
    assert_eq!(answer["tools"]["ruff"], "not_installed");
    assert_eq!(answer["tools"]["pylint"], "not_installed");
    assert_eq!(answer["tools"]["todo"], "not_available", "the rules module had nothing to check");
    let train_codes = codes(&answer, "src/train.py");
    assert!(train_codes.contains(&(1, "UnusedImport".into())), "{train_codes:?}");
    assert!(train_codes.contains(&(3, "ModuleNotFound".into())), "{train_codes:?}");
    assert!(train_codes.contains(&(11, "UndefinedName".into())), "{train_codes:?}");
    // The environment's own modules, the directory's, and guarded imports are not missing.
    assert_eq!(train_codes.iter().filter(|(_, code)| code == "ModuleNotFound").count(), 1, "{train_codes:?}");
    assert_eq!(codes(&answer, "src/broken.py"), [(1, "SyntaxError".to_string())]);
    let missing =
        answer["problems"].as_array().unwrap().iter().find(|problem| problem["code"] == "ModuleNotFound").unwrap();
    assert_eq!(
        (missing["tool"].as_str(), missing["severity"].as_str(), missing["col"].as_u64()),
        (Some("imports"), Some("error"), Some(1))
    );
    assert!(!marker.exists(), "the checked code ran");
    assert!(std::fs::read_dir(cache.path().join("check")).unwrap().next().is_none(), "the files checked are not kept");

    // ruff and pylint, when the environment has them.
    let tools = tempfile::tempdir().unwrap();
    stand_in(
        tools.path(),
        "ruff",
        &json!([{"code": "E501", "message": "Line too long", "filename": "src/train.py", "location": {"row": 2, "column": 80}}]),
    );
    stand_in(
        tools.path(),
        "pylint",
        &json!([
            {"type": "error", "message-id": "E0602", "symbol": "undefined-variable", "message": "Undefined variable", "path": "src/train.py", "line": 11, "column": 6},
            // It sees only the files checked: its import errors are left out.
            {"type": "error", "message-id": "E0401", "symbol": "import-error", "message": "Unable to import 'utils'", "path": "src/train.py", "line": 5, "column": 0}
        ]),
    );
    std::env::set_var("PYTHONPATH", tools.path());
    let answer = check::run(&RUNNER, &python, &checked, &listing, Duration::from_secs(60)).unwrap();
    std::env::remove_var("PYTHONPATH");
    assert_eq!(
        (answer["tools"]["ruff"].as_str(), answer["tools"]["pylint"].as_str()),
        (Some("ran"), Some("ran")),
        "{answer}"
    );
    let train_codes = codes(&answer, "src/train.py");
    assert!(
        train_codes.contains(&(2, "E501".into())) && train_codes.contains(&(11, "E0602(undefined-variable)".into())),
        "{train_codes:?}"
    );
    assert!(!train_codes.iter().any(|(_, code)| code.starts_with("E0401")), "{train_codes:?}");
    assert!(!marker.exists());

    // An interpreter that is not there.
    let error =
        check::run(&RUNNER, Path::new("/nowhere/python3"), &checked, &listing, Duration::from_secs(10)).unwrap_err();
    assert_eq!(error.code, check::CHECK_FAILED);
}

#[test]
fn a_program_s_rules_are_imported_and_say_their_problems() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let cache = tempfile::tempdir().unwrap();
    use_cache(cache.path());
    let checked = files(&[("a.py", "x = 1\n# TODO: more\n")]);
    let answer = check::run(&RUNNER, &python(), &checked, &["a.py".to_string()], Duration::from_secs(60)).unwrap();
    assert_eq!(answer["tools"]["todo"], "ran", "{answer}");
    let todo: Vec<&Value> = answer["problems"].as_array().unwrap().iter().filter(|p| p["tool"] == "todo").collect();
    assert_eq!(todo.len(), 1, "{answer}");
    assert_eq!((todo[0]["line"].as_u64(), todo[0]["col"].as_u64()), (Some(2), Some(3)));
    assert_eq!((todo[0]["code"].as_str(), todo[0]["severity"].as_str()), (Some("Todo"), Some("warning")));
    // The plain runner has no rules.
    use_cache(cache.path());
    let plain = Runner { cache: self::cache, ..Runner::PLAIN };
    let answer = check::run(&plain, &python(), &checked, &["a.py".to_string()], Duration::from_secs(60)).unwrap();
    assert!(answer["tools"].get("todo").is_none(), "{answer}");
}
