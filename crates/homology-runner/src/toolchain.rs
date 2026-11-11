//! Compilers and toolchains on the runner's computer (homology#8): C and
//! C++ compilers, CUDA's `nvcc`, Rust's `cargo` and `rustc`, Node's `node`
//! and `npm`, and `deno`.
//!
//! As with Python environments ([`crate::env`]), finding them runs
//! nothing: it looks on `PATH` and where their installers put them
//! (`~/.cargo/bin`, `CUDA_PATH`, `/usr/local/cuda*/bin`). Probing one runs
//! it once for its version (`--version`; MSVC's `cl` with nothing, as it
//! tells its version only so), with a time limit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::protocol::INVALID_PARAMS;
use crate::{JsonMap, RunnerError, RunnerResult};

/// A toolchain could not be run for its version.
pub const TOOLCHAIN_PROBE_FAILED: &str = "toolchain_probe_failed";
/// The time a probe may take when the caller does not say.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The programs looked for: their kind and their name.
#[rustfmt::skip] // One kind a line reads as the table it is.
pub const PROGRAMS: &[(&str, &[&str])] = &[
    ("c", &["gcc", "g++", "clang", "clang++", "cc", "c++", "cl"]),
    ("cuda", &["nvcc"]),
    ("rust", &["cargo", "rustc"]),
    ("node", &["node", "npm", "deno"]),
];

/// One toolchain program found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tool {
    /// `c`, `cuda`, `rust` or `node` ([`PROGRAMS`]).
    pub kind: &'static str,
    /// Its name: `gcc`, `cargo`.
    pub name: &'static str,
    pub path: PathBuf,
}

impl Tool {
    /// As `toolchain.discover` reports it.
    pub fn to_json(&self) -> Value {
        json!({"kind": self.kind, "name": self.name, "path": self.path.to_string_lossy()})
    }
}

/// Where to look.
pub struct Places {
    /// The user's home (`~/.cargo/bin`).
    pub home: Option<PathBuf>,
    /// `CARGO_HOME`, `CUDA_PATH`, `CUDA_HOME` where set.
    pub vars: Vec<(String, String)>,
    pub path: Vec<PathBuf>,
    /// The folders CUDA installs into (`/usr/local`, `/opt`): their
    /// `cuda` and `cuda-*` have a `bin`.
    pub cuda_parents: Vec<PathBuf>,
    /// The suffixes a program's file may have: `.exe`, `.cmd` on Windows.
    pub suffixes: Vec<&'static str>,
}

impl Places {
    /// This computer's.
    pub fn here() -> Places {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
        let vars = ["CARGO_HOME", "CUDA_PATH", "CUDA_HOME"]
            .iter()
            .filter_map(|name| {
                std::env::var(name).ok().filter(|value| !value.is_empty()).map(|value| (name.to_string(), value))
            })
            .collect();
        let path = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
        let (cuda_parents, suffixes) = if cfg!(windows) {
            (Vec::new(), vec![".exe", ".cmd"])
        } else {
            (vec![PathBuf::from("/usr/local"), PathBuf::from("/opt")], vec![""])
        };
        Places { home, vars, path, cuda_parents, suffixes }
    }

    /// The variable `name`, where set.
    fn var(&self, name: &str) -> Option<&str> {
        self.vars.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }

    /// Every folder looked in, `PATH` first.
    fn folders(&self) -> Vec<PathBuf> {
        let mut folders = self.path.clone();
        let cargo =
            self.var("CARGO_HOME").map(PathBuf::from).or_else(|| self.home.as_ref().map(|home| home.join(".cargo")));
        folders.extend(cargo.map(|cargo| cargo.join("bin")));
        folders.extend(
            ["CUDA_PATH", "CUDA_HOME"].iter().filter_map(|name| self.var(name)).map(|cuda| Path::new(cuda).join("bin")),
        );
        for parent in &self.cuda_parents {
            let Ok(entries) = std::fs::read_dir(parent) else { continue };
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .filter(|entry| {
                    entry.file_name().to_str().is_some_and(|name| name == "cuda" || name.starts_with("cuda-"))
                })
                .map(|entry| entry.path().join("bin"))
                .collect();
            found.sort();
            folders.extend(found);
        }
        folders
    }
}

/// Whether `path` is a file that can be run.
fn runnable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// Every toolchain program found, in [`PROGRAMS`]' order; a program seen
/// twice (two folders of `PATH` that are one) once.
pub fn discover(places: &Places) -> Vec<Tool> {
    let folders = places.folders();
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for (kind, names) in PROGRAMS {
        for name in *names {
            for folder in &folders {
                for suffix in &places.suffixes {
                    let path = folder.join(format!("{name}{suffix}"));
                    if !runnable(&path) {
                        continue;
                    }
                    let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    // The same file under another name (cc, clang++) is another program.
                    if seen.insert((*name, real)) {
                        found.push(Tool { kind, name, path });
                    }
                }
            }
        }
    }
    found
}

/// The version in `banner`: the first word made of digits and dots with at
/// least one dot (`v` before it dropped), cut where they end
/// (`12.2.0-14)` → `12.2.0`).
pub fn version_of(banner: &str) -> Option<String> {
    banner.split_whitespace().find_map(|word| {
        let word = word.strip_prefix(['v', 'V']).unwrap_or(word);
        let number: String = word.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        let number = number.trim_end_matches('.');
        (number.contains('.') && number.starts_with(|c: char| c.is_ascii_digit())).then(|| number.to_string())
    })
}

/// Run the program at `path` for its version: `{version, banner}`, the
/// banner being the line the version is on (or its first line).
pub fn probe(path: &Path, timeout: Duration) -> RunnerResult<Value> {
    let mut command = crate::sys::command(path);
    let msvc = path.file_stem().is_some_and(|stem| stem.eq_ignore_ascii_case("cl"));
    if !msvc {
        command.arg("--version");
    }
    command.current_dir(std::env::temp_dir());
    let ran = crate::exec::run(command, timeout).map_err(|why| RunnerError::new(TOOLCHAIN_PROBE_FAILED, why))?;
    if ran.timed_out {
        return Err(RunnerError::new(
            TOOLCHAIN_PROBE_FAILED,
            format!("it did not answer within {} s", timeout.as_secs()),
        ));
    }
    let said = format!("{}\n{}", ran.stdout, ran.stderr);
    let lines: Vec<&str> = said.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    let line = lines.iter().find(|line| version_of(line).is_some()).or(lines.first()).copied().unwrap_or_default();
    Ok(json!({"version": version_of(line).unwrap_or_default(), "banner": line}))
}

/// `toolchain.probe`'s params: `{path, timeout_secs?}`.
pub fn probe_op(params: &JsonMap) -> RunnerResult<Value> {
    let path = crate::params::host_path(crate::params::text(params, "path")?)?;
    if !runnable(&path) {
        return Err(RunnerError::new(INVALID_PARAMS, format!("{} is not a program", path.display())));
    }
    let timeout = match params.get("timeout_secs") {
        None | Some(Value::Null) => PROBE_TIMEOUT,
        Some(value) => value
            .as_u64()
            .filter(|secs| (1..=600).contains(secs))
            .map(Duration::from_secs)
            .ok_or_else(|| RunnerError::new(INVALID_PARAMS, "timeout_secs is 1 to 600"))?,
    };
    probe(&path, timeout)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn versions_are_read_from_what_each_tool_says() {
        #[rustfmt::skip] // One tool a line reads as the table it is.
        let said = [
            ("gcc (Debian 12.2.0-14+deb12u1) 12.2.0", "12.2.0"),
            ("Debian clang version 14.0.6", "14.0.6"),
            ("cargo 1.95.0 (f2d3ce0bd 2026-03-21)", "1.95.0"),
            ("v20.11.1", "20.11.1"),
            ("Cuda compilation tools, release 12.2, V12.2.140", "12.2"),
            ("Microsoft (R) C/C++ Optimizing Compiler Version 19.38.33130 for x64", "19.38.33130"),
            ("deno 2.1.4 (stable, release, x86_64-unknown-linux-gnu)", "2.1.4"),
        ];
        for (banner, version) in said {
            assert_eq!(version_of(banner).as_deref(), Some(version), "{banner}");
        }
        for banner in ["Copyright (c) 2005-2023 NVIDIA Corporation", "nvcc: NVIDIA (R) Cuda compiler driver", ""] {
            assert_eq!(version_of(banner), None, "{banner}");
        }
    }

    /// An executable file at `path`, and its folders.
    #[cfg(unix)]
    fn program(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn tools_are_found_on_the_path_and_where_installers_put_them() {
        let root = tempfile::tempdir().unwrap();
        let (bin, home, parent) = (root.path().join("bin"), root.path().join("home"), root.path().join("local"));
        program(&bin.join("gcc"));
        std::os::unix::fs::symlink(bin.join("gcc"), bin.join("cc")).unwrap();
        std::fs::write(bin.join("clang"), "not runnable").unwrap();
        program(&home.join(".cargo/bin/cargo"));
        program(&parent.join("cuda-12.2/bin/nvcc"));
        program(&parent.join("other/bin/node"));
        let places = Places {
            home: Some(home),
            vars: Vec::new(),
            // The same folder twice, as PATH often has.
            path: vec![bin.clone(), bin.clone()],
            cuda_parents: vec![parent.clone()],
            suffixes: vec![""],
        };
        let found: Vec<(&str, &str)> = discover(&places).iter().map(|tool| (tool.kind, tool.name)).collect();
        assert_eq!(found, [("c", "gcc"), ("c", "cc"), ("cuda", "nvcc"), ("rust", "cargo")]);
        // CARGO_HOME moves cargo's folder.
        let moved = root.path().join("cargo-home");
        program(&moved.join("bin/rustc"));
        let places = Places { vars: vec![("CARGO_HOME".into(), moved.to_string_lossy().into())], ..places };
        let names: Vec<&str> = discover(&places).iter().map(|tool| tool.name).collect();
        assert!(names.contains(&"rustc") && !names.contains(&"cargo"), "{names:?}");
    }

    #[test]
    fn a_tool_here_tells_its_version() {
        let found = discover(&Places::here());
        let Some(tool) = found.iter().find(|tool| ["gcc", "cargo", "rustc"].contains(&tool.name)) else {
            eprintln!("no gcc or cargo here: not probed");
            return;
        };
        let answer = probe(&tool.path, PROBE_TIMEOUT).unwrap();
        let version = answer["version"].as_str().unwrap();
        assert!(version.contains('.'), "{answer}");
    }

    #[test]
    fn a_probe_needs_a_program() {
        let folder = tempfile::tempdir().unwrap();
        for bad in [json!({}), json!({"path": "gcc"}), json!({"path": folder.path().join("none").to_string_lossy()})] {
            let params = bad.as_object().cloned().unwrap();
            assert_eq!(probe_op(&params).unwrap_err().code, INVALID_PARAMS, "{bad}");
        }
    }
}
