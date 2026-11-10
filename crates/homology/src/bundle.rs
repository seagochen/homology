//! The runners a program carries for SSH hosts: static Linux builds,
//! `<name>-linux-<arch>` in a `runner` folder beside the program (Windows,
//! Linux) or in the bundle's `Resources` (macOS). They are data here,
//! never run on this computer: each is sent to a host and made executable
//! there. The runner for this computer is `<name>` beside the program.

use std::path::{Path, PathBuf};

use homology_runner::{RunnerError, RunnerResult};
use sha2::{Digest, Sha256};

use crate::{Program, RUNNER_MISSING};

/// The variable naming a folder of runners used instead of the package's,
/// for tests and development builds (which carry none): `<ENV>_RUNNER_DIR`.
pub fn dir_env(program: &Program) -> String {
    format!("{}_RUNNER_DIR", program.env)
}

/// The variable naming the runner for this computer used instead of the
/// one beside the program: `<ENV>_RUNNER`.
pub fn local_env(program: &Program) -> String {
    format!("{}_RUNNER", program.env)
}

/// This computer's runner: `<name>` beside the running program, or the one
/// [`local_env`] names.
pub fn local(program: &Program) -> RunnerResult<PathBuf> {
    let variable = local_env(program);
    if let Some(path) = std::env::var_os(&variable) {
        return Ok(PathBuf::from(path));
    }
    let name = format!("{}{}", program.name, std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(&name)))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            RunnerError::new(
                RUNNER_MISSING,
                format!("{name} is not beside the program (a development build: set {variable})"),
            )
        })
}

/// The machine architectures a runner is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// As `uname -m` says it on Linux (`arm64` on some).
    pub fn from_uname(machine: &str) -> Option<Arch> {
        match machine {
            "x86_64" | "amd64" => Some(Arch::X86_64),
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            _ => None,
        }
    }

    /// The architecture as runner file names and messages spell it.
    pub fn name(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// The Rust target a runner for it is built for: static, with musl.
    pub fn target(self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64-unknown-linux-musl",
            Arch::Aarch64 => "aarch64-unknown-linux-musl",
        }
    }

    /// Both, in the order they are built.
    pub const ALL: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];
}

/// The file name of `program`'s runner for `arch` in a package.
pub fn file_name(program: &Program, arch: Arch) -> String {
    format!("{}-linux-{}", program.name, arch.name())
}

/// A runner read from the package, ready to send.
pub struct Bundled {
    pub arch: Arch,
    pub bytes: Vec<u8>,
    /// Lower-case hex.
    pub sha256: String,
}

/// Where the package's runners are, if it has any.
pub fn dir(program: &Program) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(dir_env(program)) {
        return Some(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().ok()?;
    let folder = exe.parent()?;
    [folder.join("runner"), folder.join("../Resources/runner")].into_iter().find(|dir| dir.is_dir())
}

/// `program`'s Linux runner for `arch`.
pub fn linux(program: &Program, arch: Arch) -> RunnerResult<Bundled> {
    let dir = dir(program).ok_or_else(|| {
        RunnerError::new(
            RUNNER_MISSING,
            format!("this package carries no runners for SSH hosts (a development build: set {})", dir_env(program)),
        )
    })?;
    read(program, &dir, arch)
}

/// `program`'s runner for `arch` in `dir`, with its SHA-256
/// (`runner_missing` when it cannot be read).
pub fn read(program: &Program, dir: &Path, arch: Arch) -> RunnerResult<Bundled> {
    let path = dir.join(file_name(program, arch));
    let bytes = std::fs::read(&path).map_err(|error| {
        RunnerError::new(RUNNER_MISSING, format!("cannot read the {} runner {}: {error}", arch.name(), path.display()))
    })?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    Ok(Bundled { arch, bytes, sha256 })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOL: Program = Program { name: "tool-runner", version: "1.0.0", cache: "tool/runner", env: "TOOL" };

    #[test]
    fn architectures_as_uname_says_them() {
        assert_eq!(Arch::from_uname("x86_64"), Some(Arch::X86_64));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Aarch64));
        assert_eq!(Arch::from_uname("armv7l"), None);
        assert_eq!(file_name(&TOOL, Arch::Aarch64), "tool-runner-linux-aarch64");
        assert_eq!((dir_env(&TOOL), local_env(&TOOL)), ("TOOL_RUNNER_DIR".into(), "TOOL_RUNNER".into()));
    }

    #[test]
    fn a_runner_is_read_with_its_hash_and_a_missing_one_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file_name(&TOOL, Arch::X86_64)), b"abc").unwrap();
        let runner = read(&TOOL, dir.path(), Arch::X86_64).unwrap();
        assert_eq!(runner.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(read(&TOOL, dir.path(), Arch::Aarch64).err().unwrap().code, RUNNER_MISSING);
    }
}
