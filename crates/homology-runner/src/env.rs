//! Python environments on the runner's computer.
//!
//! Finding them runs nothing: it reads where conda, venv, pyenv and uv
//! keep their environments, and looks for the system's `python3`. Probing
//! one runs its interpreter on a short script of our own, in an empty
//! directory (so no module of the user's shadows the ones it imports),
//! with a time limit: the Python version, torch and what it sees of CUDA,
//! MPS and the GPUs; `nvidia-smi` for the GPUs when torch cannot say. No
//! code of the user's runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::{json, Value};

/// One environment found: what manages it, its name, its interpreter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub kind: &'static str,
    pub name: String,
    pub python: PathBuf,
}

impl Found {
    /// The environment as `env.discover` reports it.
    pub fn to_json(&self) -> Value {
        json!({"kind": self.kind, "name": self.name, "python": self.python.to_string_lossy()})
    }
}

/// Where to look: the user's home, the variables that move conda's and
/// uv's folders, `PATH`, and the directories worked on (their `.venv`).
pub struct Places {
    pub home: Option<PathBuf>,
    pub vars: Vec<(String, String)>,
    pub path: Vec<PathBuf>,
    /// The usual places of the system's `python3` besides `PATH`.
    pub system: Vec<PathBuf>,
    /// The directories worked on: their `.venv` counts too.
    pub roots: Vec<PathBuf>,
}

impl Places {
    /// This computer's.
    pub fn here(roots: Vec<PathBuf>) -> Places {
        let home = homology_sys::home_dir();
        let (vars, path) = (
            crate::params::set_vars(&[
                "CONDA_PREFIX", "CONDA_EXE", "MAMBA_ROOT_PREFIX", "APPDATA", "LOCALAPPDATA", "XDG_DATA_HOME",
            ]),
            crate::params::path_dirs(),
        );
        let system = if cfg!(windows) {
            Vec::new()
        } else {
            ["/usr/bin/python3", "/usr/local/bin/python3", "/opt/homebrew/bin/python3"].map(PathBuf::from).to_vec()
        };
        Places { home, vars, path, system, roots }
    }

    /// A variable of [`Places::vars`] as a path, when it is set.
    fn var(&self, name: &str) -> Option<PathBuf> {
        self.vars.iter().find(|(key, _)| key == name).map(|(_, value)| PathBuf::from(value))
    }
}

/// The interpreter of the environment at `prefix`, if it has one.
fn interpreter(prefix: &Path) -> Option<PathBuf> {
    let candidates: &[&str] =
        if cfg!(windows) { &["python.exe", "Scripts/python.exe"] } else { &["bin/python", "bin/python3"] };
    candidates.iter().map(|name| prefix.join(name)).find(|path| path.is_file())
}

/// Folders inside `dir`, sorted; none when it cannot be read.
fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).map(|entry| entry.path()).filter(|path| path.is_dir()).collect())
        .unwrap_or_default();
    found.sort();
    found
}

/// The last component of `path`; the whole path when it has none.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Conda (and mamba, micromamba) environments: the prefixes conda lists in
/// `~/.conda/environments.txt`, and the usual installation folders with
/// their `envs/`. An installation's own prefix is `base`.
fn conda(places: &Places) -> Vec<Found> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = &places.home {
        for name in [
            "miniconda3",
            "anaconda3",
            "miniforge3",
            "mambaforge",
            "micromamba",
            ".conda",
            "opt/miniconda3",
            "AppData/Local/miniconda3",
            "AppData/Local/anaconda3",
        ] {
            roots.push(home.join(name));
        }
    }
    if !cfg!(windows) {
        for name in ["/opt/conda", "/opt/miniconda3", "/opt/anaconda3", "/opt/miniforge3"] {
            roots.push(PathBuf::from(name));
        }
    }
    roots.extend(places.var("MAMBA_ROOT_PREFIX"));
    // CONDA_EXE is <root>/bin/conda (Scripts\conda.exe on Windows).
    roots.extend(places.var("CONDA_EXE").and_then(|exe| exe.parent()?.parent().map(Path::to_path_buf)));
    let mut prefixes: Vec<(PathBuf, bool)> = Vec::new();
    for root in &roots {
        if root.join("conda-meta").is_dir() {
            prefixes.push((root.clone(), true));
        }
        prefixes.extend(subfolders(&root.join("envs")).into_iter().map(|env| (env, false)));
    }
    if let Some(home) = &places.home {
        if let Ok(listed) = std::fs::read_to_string(home.join(".conda/environments.txt")) {
            prefixes.extend(listed_environments(&listed).into_iter().map(|prefix| (prefix, false)));
        }
    }
    prefixes.extend(places.var("CONDA_PREFIX").map(|prefix| (prefix, false)));
    let roots: BTreeSet<PathBuf> =
        prefixes.iter().filter(|(_, root)| *root).map(|(prefix, _)| prefix.clone()).collect();
    prefixes
        .into_iter()
        .filter(|(prefix, _)| prefix.join("conda-meta").is_dir())
        .filter_map(|(prefix, _)| {
            let python = interpreter(&prefix)?;
            let name = if roots.contains(&prefix) { "base".to_string() } else { file_name(&prefix) };
            Some(Found { kind: "conda", name, python })
        })
        .collect()
}

/// The prefixes conda's `environments.txt` lists: one a line, blank lines
/// and surrounding spaces left out.
pub fn listed_environments(text: &str) -> Vec<PathBuf> {
    text.lines().map(str::trim).filter(|line| !line.is_empty()).map(PathBuf::from).collect()
}

/// Virtual environments: `~/.virtualenvs/*` and each root's `.venv`.
fn venvs(places: &Places) -> Vec<Found> {
    let mut prefixes: Vec<PathBuf> =
        places.home.iter().flat_map(|home| subfolders(&home.join(".virtualenvs"))).collect();
    prefixes.extend(places.roots.iter().map(|dir| dir.join(".venv")));
    prefixes
        .into_iter()
        .filter(|prefix| prefix.join("pyvenv.cfg").is_file())
        .filter_map(|prefix| {
            Some(Found { kind: "venv", name: prefix.to_string_lossy().into_owned(), python: interpreter(&prefix)? })
        })
        .collect()
}

/// pyenv's and uv's own Pythons.
fn managed(places: &Places) -> Vec<Found> {
    let mut found = Vec::new();
    if let Some(home) = &places.home {
        found.extend(subfolders(&home.join(".pyenv/versions")).into_iter().filter_map(|prefix| {
            Some(Found { kind: "pyenv", name: file_name(&prefix), python: interpreter(&prefix)? })
        }));
    }
    let uv = if cfg!(windows) {
        places.var("APPDATA").map(|dir| dir.join("uv/python"))
    } else {
        places
            .var("XDG_DATA_HOME")
            .or_else(|| places.home.as_ref().map(|home| home.join(".local/share")))
            .map(|dir| dir.join("uv/python"))
    };
    if let Some(uv) = uv {
        found.extend(
            subfolders(&uv).into_iter().filter_map(|prefix| {
                Some(Found { kind: "uv", name: file_name(&prefix), python: interpreter(&prefix)? })
            }),
        );
    }
    found
}

/// The system's Python: `python3` (`python.exe` on Windows) on `PATH`,
/// and the usual places. The Microsoft Store's stand-in that only opens
/// the Store is left out.
fn system(places: &Places) -> Vec<Found> {
    let names: &[&str] = if cfg!(windows) { &["python.exe"] } else { &["python3"] };
    let mut paths: Vec<PathBuf> =
        places.path.iter().flat_map(|dir| names.iter().map(move |name| dir.join(name))).collect();
    paths.extend(places.system.iter().cloned());
    paths
        .into_iter()
        .filter(|path| path.is_file() && !path.to_string_lossy().contains("WindowsApps"))
        .map(|python| Found { kind: "python", name: python.to_string_lossy().into_owned(), python })
        .collect()
}

/// Every environment found, each interpreter once (the first way it was
/// found wins: conda, venv, pyenv/uv, then the system's).
pub fn discover(places: &Places) -> Vec<Found> {
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for environment in [conda(places), venvs(places), managed(places), system(places)].into_iter().flatten() {
        let key = std::fs::canonicalize(&environment.python).unwrap_or_else(|_| environment.python.clone());
        if seen.insert(key) {
            found.push(environment);
        }
    }
    found
}

/// What the probe prints, after this marker, as one JSON line.
const MARKER: &str = "HOMOLOGY-PROBE ";

/// The probe: our own few lines, nothing of the user's.
const PROBE: &str = r#"
import json, platform, sys
out = {"python": platform.python_version(), "executable": sys.executable, "implementation": platform.python_implementation()}
if "--torch" in sys.argv:
    try:
        import torch
        out["torch"] = torch.__version__
        out["cuda"] = bool(torch.cuda.is_available())
        out["cuda_version"] = torch.version.cuda
        mps = getattr(torch.backends, "mps", None)
        out["mps"] = bool(mps is not None and mps.is_available())
        out["gpus"] = [{"name": torch.cuda.get_device_name(i), "memory_mb": torch.cuda.get_device_properties(i).total_memory // 1048576} for i in range(torch.cuda.device_count())] if out["cuda"] else []
    except ImportError:
        out["torch"] = None
    except Exception as error:
        out["torch_error"] = "%s: %s" % (type(error).__name__, error)
print("HOMOLOGY-PROBE " + json.dumps(out))
"#;

/// The default time a probe may take (torch and CUDA can be slow to load).
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest `env.probe` may be asked to wait.
pub const MAX_PROBE_TIMEOUT: Duration = Duration::from_secs(600);

/// Run `command` for at most `timeout` ([`crate::exec::run_with`]: in a
/// process group of its own, ended whole past it), `input` on its stdin:
/// its stdout, or why not.
pub fn stdout_of(command: Command, input: Option<&[u8]>, timeout: Duration) -> Result<String, String> {
    let ran = crate::exec::run_with(command, timeout, input).map_err(|error| crate::exec::cannot_start(&error))?;
    if ran.timed_out {
        return Err(format!("it did not finish within {} s", timeout.as_secs()));
    }
    match ran.code {
        Some(0) => Ok(ran.stdout),
        code => {
            let status = code.map_or_else(|| "a signal".to_string(), |code| format!("exit code {code}"));
            Err(format!("it ended with {status}: {}", ran.stderr.trim().lines().last().unwrap_or_default()))
        }
    }
}

/// The interpreter at `python` on the probe, with or without torch.
fn probe_once(python: &Path, torch: bool, timeout: Duration) -> Result<Value, String> {
    let scratch = std::env::temp_dir();
    let mut command = crate::sys::command(python);
    command.arg("-c").arg(PROBE).current_dir(&scratch).env("PYTHONDONTWRITEBYTECODE", "1");
    if torch {
        command.arg("--torch");
    }
    answer_of(&stdout_of(command, None, timeout)?)
}

/// The probe's answer in what it printed: the JSON after [`MARKER`] on the
/// first line that has it.
pub fn answer_of(output: &str) -> Result<Value, String> {
    let line =
        output.lines().find_map(|line| line.strip_prefix(MARKER)).ok_or_else(|| "it printed no answer".to_string())?;
    serde_json::from_str(line).map_err(|error| format!("its answer is not JSON: {error}"))
}

/// The GPUs `nvidia-smi` sees, when it is there.
fn nvidia_smi() -> Option<Value> {
    let mut command = crate::sys::command("nvidia-smi");
    command.args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"]);
    gpus_of(&stdout_of(command, None, Duration::from_secs(10)).ok()?)
}

/// The GPUs in what `nvidia-smi --query-gpu=name,memory.total
/// --format=csv,noheader,nounits` printed: `[{name, memory_mb}]` (`null`
/// memory when it is not a number), `None` when there is none.
pub fn gpus_of(output: &str) -> Option<Value> {
    let gpus: Vec<Value> = output
        .lines()
        .filter_map(|line| {
            let (name, memory) = line.rsplit_once(',')?;
            Some(json!({"name": name.trim(), "memory_mb": memory.trim().parse::<u64>().ok()}))
        })
        .collect();
    (!gpus.is_empty()).then(|| Value::Array(gpus))
}

/// Probe the interpreter at `python`: `{ok, python, executable, torch,
/// cuda, cuda_version, mps, gpus, gpus_source, errors}`. A probe that
/// cannot load torch in time still says what Python it is; one whose
/// interpreter does not run is `ok: false` with the reason.
pub fn probe(python: &Path, timeout: Duration) -> Value {
    let mut errors = serde_json::Map::new();
    let mut result = match probe_once(python, true, timeout) {
        Ok(result) => result,
        Err(why) => {
            errors.insert("torch".into(), json!(why));
            match probe_once(python, false, timeout.min(Duration::from_secs(10))) {
                Ok(result) => result,
                Err(why) => {
                    return json!({"ok": false, "error": format!("the interpreter {} does not run: {why}", python.display())})
                }
            }
        }
    };
    let Some(object) = result.as_object_mut() else {
        return json!({"ok": false, "error": "the probe answered something else than an object"});
    };
    if let Some(error) = object.remove("torch_error") {
        errors.insert("torch".into(), error);
    }
    let has_gpus = object.get("gpus").and_then(Value::as_array).is_some_and(|gpus| !gpus.is_empty());
    if has_gpus {
        object.insert("gpus_source".into(), json!("torch"));
    } else if let Some(gpus) = nvidia_smi() {
        object.insert("gpus".into(), gpus);
        object.insert("gpus_source".into(), json!("nvidia-smi"));
    } else {
        object.entry("gpus").or_insert(json!([]));
    }
    for key in ["torch", "cuda", "cuda_version", "mps"] {
        object.entry(key).or_insert(Value::Null);
    }
    object.insert("ok".into(), json!(true));
    object.insert("errors".into(), Value::Object(errors));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::time::Instant;

    /// A command that leaves a process behind holding its output open
    /// neither hangs [`stdout_of`] once it ended, nor outlives the time
    /// given: the whole group is ended (homology#27).
    #[cfg(unix)]
    #[test]
    fn what_a_command_leaves_running_does_not_hold_it() {
        let started = Instant::now();
        let mut ended = Command::new("sh");
        ended.args(["-c", "sleep 7411 & echo done"]);
        assert_eq!(stdout_of(ended, None, Duration::from_secs(20)).unwrap().trim(), "done");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let started = Instant::now();
        let mut hangs = Command::new("sh");
        hangs.args(["-c", "sleep 7412 & sleep 7413"]);
        let said = stdout_of(hangs, Some(b"input"), Duration::from_secs(1)).unwrap_err();
        assert!(said.contains("did not finish"), "{said}");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        // No `sleep 7412` or `sleep 7413` left (Linux: read from `/proc`).
        #[cfg(target_os = "linux")]
        {
            let running = |wanted: &[u8]| {
                std::fs::read_dir("/proc").unwrap().flatten().any(|entry| {
                    let stat = std::fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
                    let zombie = stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'));
                    std::fs::read(entry.path().join("cmdline")).is_ok_and(|line| line == wanted) && !zombie
                })
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while (running(b"sleep\x007412\x00") || running(b"sleep\x007413\x00")) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            assert!(!running(b"sleep\x007412\x00") && !running(b"sleep\x007413\x00"), "the group was left running");
        }
    }

    fn places(home: &Path) -> Places {
        Places {
            home: Some(home.to_path_buf()),
            vars: Vec::new(),
            path: Vec::new(),
            system: Vec::new(),
            roots: Vec::new(),
        }
    }

    /// A fake environment at `prefix` with an interpreter file (and the
    /// folder `marker` that says what made it).
    fn environment(prefix: &Path, marker: Option<&str>) {
        std::fs::create_dir_all(marker.map_or_else(|| prefix.to_path_buf(), |marker| prefix.join(marker))).unwrap();
        let python = if cfg!(windows) { prefix.join("python.exe") } else { prefix.join("bin/python") };
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(python, b"").unwrap();
    }

    #[test]
    fn conda_installations_their_envs_and_listed_prefixes_are_found_once() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("miniconda3");
        environment(&root, Some("conda-meta"));
        environment(&root.join("envs/yolo"), Some("conda-meta"));
        let elsewhere = home.path().join("work/envs/seg");
        environment(&elsewhere, Some("conda-meta"));
        std::fs::create_dir_all(home.path().join(".conda")).unwrap();
        let listed = format!("{}\n{}\n\n", root.join("envs/yolo").display(), elsewhere.display());
        std::fs::write(home.path().join(".conda/environments.txt"), listed).unwrap();
        std::fs::create_dir_all(root.join("envs/broken")).unwrap();
        let found = discover(&places(home.path()));
        let names: Vec<(&str, &str)> = found.iter().map(|found| (found.kind, found.name.as_str())).collect();
        assert_eq!(names, [("conda", "base"), ("conda", "yolo"), ("conda", "seg")]);
    }

    #[test]
    fn venvs_pyenv_and_uv_are_found() {
        let home = tempfile::tempdir().unwrap();
        let model = home.path().join("model");
        std::fs::create_dir_all(model.join(".venv")).unwrap();
        environment(&model.join(".venv"), None);
        std::fs::write(model.join(".venv/pyvenv.cfg"), b"home = /usr/bin\n").unwrap();
        environment(&home.path().join(".virtualenvs/old"), None);
        std::fs::write(home.path().join(".virtualenvs/old/pyvenv.cfg"), b"").unwrap();
        environment(&home.path().join(".pyenv/versions/3.12.4"), None);
        let mut places = places(home.path());
        places.roots.push(model.clone());
        if cfg!(windows) {
            places.vars.push(("APPDATA".into(), home.path().join("roaming").to_string_lossy().into_owned()));
            environment(&home.path().join("roaming/uv/python/cpython-3.13"), None);
        } else {
            environment(&home.path().join(".local/share/uv/python/cpython-3.13"), None);
        }
        let kinds: Vec<&str> = discover(&places).iter().map(|found| found.kind).collect();
        assert_eq!(kinds, ["venv", "venv", "pyenv", "uv"]);
    }

    #[test]
    fn the_stores_stand_in_python_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let stand_in = dir.path().join("WindowsApps");
        std::fs::create_dir_all(&stand_in).unwrap();
        let name = if cfg!(windows) { "python.exe" } else { "python3" };
        std::fs::write(stand_in.join(name), b"").unwrap();
        let places =
            Places { home: None, vars: Vec::new(), path: vec![stand_in], system: Vec::new(), roots: Vec::new() };
        assert!(system(&places).is_empty());
    }

    /// A Python this computer has, to probe for real.
    fn some_python() -> PathBuf {
        let found = discover(&Places::here(Vec::new()));
        found
            .into_iter()
            .find(|found| found.kind == "python" || found.kind == "conda")
            .map(|found| found.python)
            .expect("a Python on this computer")
    }

    #[test]
    fn a_real_interpreter_is_probed() {
        let python = some_python();
        let probe = probe(&python, PROBE_TIMEOUT);
        assert_eq!(probe["ok"], json!(true), "{probe}");
        assert!(probe["python"].as_str().unwrap().starts_with('3'), "{probe}");
        assert!(probe["gpus"].is_array() && probe.get("torch").is_some() && probe["errors"].is_object(), "{probe}");
    }

    /// An interpreter that hangs (here a script that sleeps) is given up
    /// on: the probe says why, it does not hang with it.
    #[cfg(unix)]
    #[test]
    fn an_interpreter_that_hangs_is_given_up_on() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let python = dir.path().join("python");
        std::fs::write(&python, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let probe = probe(&python, Duration::from_secs(1));
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert_eq!(probe["ok"], json!(false), "{probe}");
        assert!(probe["error"].as_str().unwrap().contains("did not finish"), "{probe}");
    }

    #[test]
    fn an_interpreter_that_does_not_run_is_not_ok() {
        let dir = tempfile::tempdir().unwrap();
        let probe = probe(&dir.path().join("no-python"), Duration::from_secs(5));
        assert_eq!(probe["ok"], json!(false));
        assert!(probe["error"].as_str().unwrap().contains("does not run"), "{probe}");
    }
}
