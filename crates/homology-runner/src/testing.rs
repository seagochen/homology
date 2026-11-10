//! A runner Cargo built, for tests (feature `testing`): a test program is
//! `target/<profile>/deps/<test>`, and the runner programs are
//! `target/<profile>/<name>` beside its folder.

use std::path::PathBuf;

/// The built runner program `name` (`homology-runner`, or a program's own
/// runner), or why it is not there: it is built by `cargo build -p <its
/// package>`, and by `cargo test --workspace`.
pub fn built(name: &str) -> Result<PathBuf, String> {
    let test = std::env::current_exe().map_err(|error| format!("cannot find this test program: {error}"))?;
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let built = test
        .parent()
        .and_then(|deps| deps.parent())
        .map(|profile| profile.join(&file))
        .ok_or_else(|| format!("{} is not in a Cargo target directory", test.display()))?;
    if built.is_file() {
        Ok(built)
    } else {
        Err(format!("{} is missing: build it (cargo build --bin {name})", built.display()))
    }
}
