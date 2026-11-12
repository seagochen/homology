//! The programs a product carries with it (#24): FFmpeg's `ffmpeg` and
//! `ffprobe`, or any other, found the same way by every product. A release
//! runs only its own copy, beside its executable, in `bin/` there, or in a
//! macOS bundle's `Contents/Helpers`; one not there is still named there,
//! so that starting it fails saying so, never falling back to a program of
//! the user's. A development build takes a variable's value, then the
//! product's setting, then its own copy, then the program on `PATH`.

use std::path::{Path, PathBuf};

/// A program to run, and where it was looked for (for what an error says).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub command: PathBuf,
    pub searched: String,
    /// Only the product's own copy was looked for (a release).
    pub bundled_only: bool,
}

impl Program {
    /// Why it could not be started, where it was looked for and what to do:
    /// reinstall, for the product's own copy; else install it or set the variable.
    pub fn cannot_run(&self, error: &std::io::Error) -> String {
        if self.bundled_only {
            return format!(
                "cannot run bundled component {} (looked in: {}); reinstall the application to restore it: {error}",
                self.command.display(),
                self.searched
            );
        }
        format!(
            "cannot run {} (looked in: {}; install it or set the variable): {error}",
            self.command.display(),
            self.searched
        )
    }
}

/// Where the product's own `name` may be, for its executable in `exe_dir`:
/// beside it or in `bin/` (Windows, Linux), and `Contents/Helpers` of a
/// macOS bundle (the executable is in `Contents/MacOS`).
pub fn candidates(exe_dir: &Path, name: &str) -> Vec<PathBuf> {
    let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let mut dirs = vec![exe_dir.to_path_buf(), exe_dir.join("bin")];
    if cfg!(target_os = "macos") {
        if let Some(contents) = exe_dir.parent() {
            dirs.push(contents.join("Helpers"));
        }
    }
    dirs.into_iter().map(|dir| dir.join(&file)).collect()
}

/// The program `name` for an executable in `exe_dir`: only its own copy
/// when `bundled_only`, else the variable `env`'s value (`variable`), the
/// setting (`configured`), its own copy, `name` on `PATH`. A blank value is unset.
pub fn select(
    env: &str,
    variable: Option<String>,
    exe_dir: Option<&Path>,
    name: &str,
    configured: Option<&Path>,
    bundled_only: bool,
) -> Program {
    let own = exe_dir.map(|dir| candidates(dir, name)).unwrap_or_default();
    if bundled_only {
        let searched = if own.is_empty() {
            format!("bundled {name}: executable directory unavailable")
        } else {
            own.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")
        };
        let command = own.iter().find(|path| path.is_file()).or_else(|| own.first()).cloned().unwrap_or_default();
        return Program { command, searched, bundled_only: true };
    }
    if let Some(value) = variable.filter(|value| !value.trim().is_empty()) {
        return Program { searched: format!("{env}={value}"), command: PathBuf::from(value), bundled_only: false };
    }
    if let Some(command) = configured.filter(|path| !path.as_os_str().is_empty()) {
        return Program {
            searched: format!("settings: {}", command.display()),
            command: command.to_path_buf(),
            bundled_only: false,
        };
    }
    let mut searched: Vec<String> = vec![format!("{env} (unset)")];
    searched.extend(own.iter().map(|path| path.display().to_string()));
    searched.push("PATH".into());
    let searched = searched.join(", ");
    match own.into_iter().find(|path| path.is_file()) {
        Some(command) => Program { command, searched, bundled_only: false },
        None => Program { command: PathBuf::from(name), searched, bundled_only: false },
    }
}

/// The program `name` for the running executable, as [`select`] chooses it
/// from the variable `env`.
pub fn find(env: &str, name: &str, configured: Option<&Path>, bundled_only: bool) -> Program {
    let exe_dir = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
    select(env, std::env::var(env).ok(), exe_dir.as_deref(), name, configured, bundled_only)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn executable(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn a_release_ignores_every_override_and_never_falls_back_to_path() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["ffmpeg", "ffprobe"] {
            let outside = executable(&dir.path().join("external"), name);
            let select =
                |exe_dir| select("X_TOOL", Some(outside.display().to_string()), exe_dir, name, Some(&outside), true);
            // A program of the user's that is there does not stand in for the missing own copy.
            let missing = select(Some(dir.path()));
            assert_eq!(missing.command, dir.path().join(format!("{name}{}", std::env::consts::EXE_SUFFIX)));
            assert!(!missing.command.exists());
            assert!(!missing.searched.contains("PATH") && !missing.searched.contains("external"));
            let message = missing.cannot_run(&std::io::Error::from(std::io::ErrorKind::NotFound));
            assert!(message.contains("reinstall the application") && !message.contains("set the variable"));
            let own = executable(&dir.path().join("bin"), name);
            assert_eq!(select(Some(dir.path())).command, own);
            assert!(select(None).command.as_os_str().is_empty());
        }
    }

    #[test]
    fn a_development_build_takes_the_variable_then_the_setting_then_its_own_then_path() {
        let dir = tempfile::tempdir().unwrap();
        let own = executable(dir.path(), "ffmpeg");
        let configured = PathBuf::from("/opt/tools/ffmpeg");
        let chosen =
            select("X_FFMPEG", Some("/opt/ffmpeg".into()), Some(dir.path()), "ffmpeg", Some(&configured), false);
        assert_eq!((chosen.command, chosen.searched.as_str()), (PathBuf::from("/opt/ffmpeg"), "X_FFMPEG=/opt/ffmpeg"));
        // A blank variable is unset: the setting.
        let chosen = select("X_FFMPEG", Some("  ".into()), Some(dir.path()), "ffmpeg", Some(&configured), false);
        assert_eq!((chosen.command, chosen.searched.as_str()), (configured, "settings: /opt/tools/ffmpeg"));
        // No setting (or a blank one): its own copy, then PATH.
        assert_eq!(select("X_FFMPEG", None, Some(dir.path()), "ffmpeg", Some(Path::new("")), false).command, own);
        let elsewhere = tempfile::tempdir().unwrap();
        let path = select("X_FFMPEG", None, Some(elsewhere.path()), "ffmpeg", None, false);
        assert_eq!(path.command, PathBuf::from("ffmpeg"));
        assert!(path.searched.ends_with("PATH") && path.searched.starts_with("X_FFMPEG (unset)"));
        let message = path.cannot_run(&std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(message.contains("install it or set the variable"));
    }

    #[test]
    fn a_macos_bundle_s_helpers_are_looked_in() {
        let found = candidates(Path::new("/A.app/Contents/MacOS"), "ffprobe");
        assert_eq!(found.len(), if cfg!(target_os = "macos") { 3 } else { 2 });
        if cfg!(target_os = "macos") {
            assert_eq!(found[2], Path::new("/A.app/Contents/Helpers/ffprobe"));
        }
    }
}
