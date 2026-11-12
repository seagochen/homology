//! Choosing a folder on a host one level at a time (#20): the folder a
//! target names, as the host resolves it, and the folders directly in it.
//! A POSIX `sh` script over the user's `ssh`, so nothing need be placed on
//! the host first (a runner lists a whole tree, for a folder chosen
//! already).

use crate::target::SshTarget;
use crate::{Context, SshError, SshResult};

/// What the script exits with for a folder that is not there, and one
/// that cannot be entered.
const MISSING: i32 = 3;
const DENIED: i32 = 5;

/// The folder `target` names (`~` for the home folder), absolute as the
/// host resolves it, and the folders directly in it, hidden ones and links
/// to folders included: plain ones first, then hidden ones, each by name
/// in any case. `remote_directory_missing` for a folder that is not
/// there, `remote_directory_denied` for one that cannot be entered.
pub fn directories(cx: &Context, target: &SshTarget) -> SshResult<(String, Vec<String>)> {
    listed(&crate::ssh::run(cx, target, &script(target)), target)
}

/// The script that lists `target`'s folder: its path, then a folder a
/// line. A glob that matches nothing stays as written, and fails `-d`.
fn script(target: &SshTarget) -> String {
    format!(
        "absent() {{ [ -e \"$1\" ] || [ -L \"$1\" ] && return 1; p=$1; while :; do q=$(dirname -- \"$p\"); \
         [ \"$q\" = \"$p\" ] && return 1; p=$q; if [ -e \"$p\" ] || [ -L \"$p\" ]; then [ -d \"$p\" ] && [ -x \"$p\" ]; \
         return; fi; done; }}\n\
         d={}; cd -- \"$d\" 2>/dev/null || {{ absent \"$d\" && exit {MISSING}; exit {DENIED}; }}\n\
         pwd; for entry in * .[!.]* ..?*; do [ -d \"$entry\" ] && printf '%s\\n' \"$entry\"; done; exit 0",
        target.quoted_path()
    )
}

/// What the script printed, or why it failed, as [`directories`] answers.
fn listed(ran: &SshResult<Vec<u8>>, target: &SshTarget) -> SshResult<(String, Vec<String>)> {
    let output = match ran {
        Ok(output) => output,
        Err(error) if error.status == Some(MISSING) => {
            return Err(SshError::new("remote_directory_missing", format!("{} is not there", target.path)))
        }
        Err(error) if error.status == Some(DENIED) => {
            return Err(SshError::new("remote_directory_denied", format!("{} cannot be opened", target.path)))
        }
        Err(error) => return Err(error.clone()),
    };
    let text = String::from_utf8_lossy(output);
    let mut lines = text.lines();
    let path = lines
        .next()
        .filter(|path| path.starts_with('/'))
        .ok_or_else(|| SshError::new("remote_failed", "the host did not name the folder"))?
        .to_string();
    let mut folders: Vec<String> = lines.filter(|name| !name.is_empty()).map(str::to_string).collect();
    folders.sort_by_key(|name| (name.starts_with('.'), name.to_lowercase()));
    Ok((path, folders))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The script run by the local `sh` in `home`, as the host would run it;
    /// none without one (Windows without Git's `sh`).
    fn run_here(target: &SshTarget, home: &std::path::Path) -> Option<SshResult<Vec<u8>>> {
        let output =
            std::process::Command::new("sh").arg("-c").arg(script(target)).env("HOME", home).current_dir(home).output();
        let output = output.ok()?;
        Some(match output.status.code() {
            Some(0) => Ok(output.stdout),
            status => Err(SshError { status, ..SshError::new("remote_failed", "") }),
        })
    }

    #[test]
    fn a_folder_s_folders_are_listed_plain_ones_first() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        for folder in ["data/b", "data/A two", "data/.hidden", "data/c/deep"] {
            std::fs::create_dir_all(home.join(folder)).unwrap();
        }
        std::fs::write(home.join("data/file.txt"), "").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(home.join("data/b"), home.join("data/link")).unwrap();
        let target = |path: &str| SshTarget { host: "h".into(), port: None, path: path.into() };
        let Some(ran) = run_here(&target("~/data"), &home) else {
            eprintln!("no sh here");
            return;
        };
        let (path, folders) = listed(&ran, &target("~/data")).unwrap();
        assert_eq!(path, home.join("data").display().to_string());
        let mut expected = vec!["A two", "b", "c"];
        if cfg!(unix) {
            expected.push("link");
        }
        expected.push(".hidden");
        assert_eq!(folders, expected);
        let (path, _) = listed(&run_here(&target("~"), &home).unwrap(), &target("~")).unwrap();
        assert_eq!(path, home.display().to_string(), "the home folder");
        let absolute = home.join("data/c").display().to_string();
        let (_, folders) = listed(&run_here(&target(&absolute), &home).unwrap(), &target(&absolute)).unwrap();
        assert_eq!(folders, ["deep"]);
        let missing = listed(&run_here(&target("~/none/here"), &home).unwrap(), &target("~/none/here"));
        assert_eq!(missing.unwrap_err().code, "remote_directory_missing");
        let file = listed(&run_here(&target("~/data/file.txt"), &home).unwrap(), &target("~/data/file.txt"));
        assert_eq!(file.unwrap_err().code, "remote_directory_denied", "a file is no folder to open");
    }

    #[test]
    fn a_host_that_names_no_folder_is_said() {
        let target = SshTarget { host: "h".into(), port: None, path: "~".into() };
        assert_eq!(listed(&Ok(b"oops\n".to_vec()), &target).unwrap_err().code, "remote_failed");
        let failed = Err(SshError::new("ssh_unreachable", "no"));
        assert_eq!(listed(&failed, &target).unwrap_err().code, "ssh_unreachable");
    }
}
