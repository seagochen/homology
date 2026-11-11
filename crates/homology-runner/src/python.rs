//! The Python the runner brings to the user's environments: the code
//! check (`python/homology_check.py`) and the pyflakes it bundles, the
//! completion (`python/homology_complete.py`) and the jedi and parso it
//! bundles (homology#14), with
//! whatever files the program serving the runner adds (its rules, its own
//! package). It is part of the program and written once per version into
//! the cache's `python/<version>-<hash>/`; nothing is installed into the
//! user's environment.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

/// A file of the folder: its `/`-separated path in it, and its bytes.
pub type File = (&'static str, &'static [u8]);

include!(concat!(env!("OUT_DIR"), "/python_files.rs"));

/// The folder in `cache` holding [`FILES`] and `more`, written when this
/// `version` with these files has none; why not. Its name carries a hash
/// of the files, so a program that changes them gets a folder of its own.
pub fn place(cache: &Path, version: &str, more: &[File]) -> Result<PathBuf, String> {
    let files: Vec<&File> = FILES.iter().chain(more).collect();
    let mut hasher = Sha256::new();
    for (path, bytes) in &files {
        hasher.update(path.as_bytes());
        hasher.update(bytes);
    }
    let digest = hex::encode(hasher.finalize());
    let python = cache.join("python");
    let dir = python.join(format!("{version}-{}", &digest[..16]));
    let complete = |dir: &Path| {
        files.iter().all(|(path, _)| path.split('/').fold(dir.to_path_buf(), |file, part| file.join(part)).is_file())
    };
    if complete(&dir) {
        return Ok(dir);
    }
    crate::sys::private_dir(&python).map_err(|error| format!("cannot make {}: {error}", python.display()))?;
    // Written beside it, then renamed: a half-written folder is never used.
    let building = python.join(format!(".{}-{}", std::process::id(), unique()));
    for (path, bytes) in &files {
        let file = path.split('/').fold(building.clone(), |file, part| file.join(part));
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("cannot make {}: {error}", parent.display()))?;
        }
        std::fs::write(&file, bytes).map_err(|error| format!("cannot write {}: {error}", file.display()))?;
    }
    if std::fs::rename(&building, &dir).is_err() {
        // Another check or run wrote it meanwhile.
        let _ = std::fs::remove_dir_all(&building);
    }
    if complete(&dir) {
        Ok(dir)
    } else {
        Err(format!("cannot place the runner's Python in {}", dir.display()))
    }
}

/// Put `dir` first on the `PYTHONPATH` of a program's variables `env`:
/// before the one they give, or else before the one this runner was
/// started with.
pub fn put_on_path(env: &mut Vec<(String, String)>, dir: &Path) -> Result<(), String> {
    let given = env.iter().position(|(key, _)| key == "PYTHONPATH");
    let rest = match given {
        Some(at) => Some(std::ffi::OsString::from(env.remove(at).1)),
        None => std::env::var_os("PYTHONPATH"),
    };
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(rest.iter().flat_map(std::env::split_paths).filter(|path| !path.as_os_str().is_empty()));
    let joined =
        std::env::join_paths(paths).map_err(|error| format!("cannot put {} on PYTHONPATH: {error}", dir.display()))?;
    env.push(("PYTHONPATH".into(), joined.to_string_lossy().into_owned()));
    Ok(())
}

/// A name no other folder being written has.
pub fn unique() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.subsec_nanos()).unwrap_or(0);
    (u64::from(nanos) << 16) ^ NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_is_written_once_per_version_and_files() {
        let cache = tempfile::tempdir().unwrap();
        let dir = place(cache.path(), "1.0.0", &[]).unwrap();
        assert!(dir.join("vendor/pyflakes/checker.py").is_file() && dir.join("homology_check.py").is_file());
        assert_eq!(place(cache.path(), "1.0.0", &[]).unwrap(), dir);
        let more = place(cache.path(), "1.0.0", &[("mine/rules.py", b"TOOL = 'x'\n")]).unwrap();
        assert_ne!(more, dir);
        assert!(more.join("mine/rules.py").is_file() && more.join("homology_check.py").is_file());
        let folders: Vec<_> = std::fs::read_dir(cache.path().join("python")).unwrap().filter_map(Result::ok).collect();
        assert_eq!(folders.len(), 2, "no half-written copy is left");
    }

    /// A file added to the checker's folder and not listed here would be
    /// missing on every host.
    #[test]
    fn every_file_of_the_checker_is_brought_along() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
        let mut on_disk = Vec::new();
        let mut folders = vec![root.clone()];
        while let Some(folder) = folders.pop() {
            for entry in std::fs::read_dir(folder).unwrap().filter_map(Result::ok) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_dir() && name != "__pycache__" {
                    folders.push(path);
                } else if path.is_file() && name != "README.md" {
                    on_disk.push(path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        on_disk.sort();
        let mut listed: Vec<String> = FILES.iter().map(|(path, _)| path.to_string()).collect();
        listed.sort();
        assert_eq!(listed, on_disk);
    }

    #[test]
    fn the_folder_goes_first_on_the_path() {
        let dir = Path::new("/cache/python/1-abc");
        let mut env = vec![("PYTHONPATH".to_string(), "/mine".to_string()), ("A".to_string(), "1".to_string())];
        put_on_path(&mut env, dir).unwrap();
        let separator = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(
            env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("PYTHONPATH".to_string(), format!("{}{separator}/mine", dir.display()))
            ]
        );
    }
}
