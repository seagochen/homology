//! A directory's files, for an editor: listed, read with their sha256,
//! written only over the content the editor opened, searched.
//!
//! - `fs.list {root}`: `{files: [{path, size}], truncated}`, sorted, the
//!   folders in [`SKIPPED`] left out, [`MAX_FILES`] at most.
//! - `fs.read {root, path}`: `{path, text, sha256, size}`; a file that is
//!   not UTF-8 text, or larger than [`MAX_TEXT`], is refused.
//! - `fs.write {root, path, text, sha256}`: saved only when the file's
//!   content is still the one whose sha256 is given (`null`: the file must
//!   not exist yet), else [`CODE_CONFLICT`] and nothing written. The text
//!   goes to a temporary file beside it, then a rename, so the file is never
//!   half-written; its permissions are kept.
//!
//! Every `path` is relative to `root`, `/`-separated, and stays inside it:
//! no `..`, and no link that leads out of it.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::protocol::{RunnerError, RunnerResult, INVALID_PARAMS, UNKNOWN_OP};
use crate::{params, JsonMap};

/// The codes the file operations on a directory fail with.
pub const CODE_NOT_FOUND: &str = "code_not_found";
pub const CODE_CONFLICT: &str = "code_conflict";
pub const CODE_TOO_LARGE: &str = "code_too_large";
pub const CODE_BINARY: &str = "code_binary";
pub const CODE_PATH_INVALID: &str = "code_path_invalid";
pub const CODE_IO: &str = "code_io";
/// The directory itself (the `root`) is not there.
pub const ROOT_MISSING: &str = "root_missing";

/// Files listed at most, and folders never looked into: environments,
/// caches and the repository's own.
pub const MAX_FILES: usize = 5000;
pub const SKIPPED: &[&str] = &[".git", ".venv", "__pycache__", "node_modules", ".mypy_cache", ".pytest_cache"];

/// The largest file the editor opens.
pub const MAX_TEXT: u64 = 2 * 1024 * 1024;

/// Matches a search gives at most, and how much of a matching line it
/// gives back (in chars).
pub const MAX_MATCHES: usize = 1000;
pub const MATCH_LINE: usize = 240;

/// A failed file operation (`code_io`): what was done, to which path, why.
fn io(what: &str, path: &Path, error: std::io::Error) -> RunnerError {
    RunnerError::new(CODE_IO, format!("{what} {}: {error}", path.display()))
}

/// The files under `dir`, relative and `/`-separated with their sizes,
/// sorted; whether the listing stopped at [`MAX_FILES`].
pub fn list_files(dir: &Path) -> RunnerResult<(Vec<(String, u64)>, bool)> {
    list_entries(dir).map(|(files, _, truncated)| (files, truncated))
}

/// A directory's files (path, size), its folders, and whether the files
/// stopped at [`MAX_FILES`].
pub type Listing = (Vec<(String, u64)>, Vec<String>, bool);

/// The files and the folders of `dir`, as [`list_files`] walks it.
pub fn list_entries(dir: &Path) -> RunnerResult<Listing> {
    if !dir.is_dir() {
        return Err(RunnerError::new(ROOT_MISSING, format!("{} is not a folder", dir.display())));
    }
    let mut folders = Vec::new();
    let mut files = Vec::new();
    let mut truncated = false;
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(dir.join(&relative)) else { continue };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = relative.join(&name);
            match entry.file_type() {
                Ok(kind) if kind.is_dir() && !SKIPPED.contains(&name.as_str()) => {
                    if folders.len() < MAX_FILES {
                        folders.push(path.to_string_lossy().replace('\\', "/"));
                    }
                    pending.push(path);
                }
                Ok(kind) if kind.is_file() => {
                    if files.len() == MAX_FILES {
                        truncated = true;
                        continue;
                    }
                    let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                    files.push((path.to_string_lossy().replace('\\', "/"), size));
                }
                _ => {}
            }
        }
    }
    files.sort();
    folders.sort();
    Ok((files, folders, truncated))
}

/// `path` under `root`, checked to be a relative path that stays inside it
/// (a link leading out of it counts as outside).
pub(crate) fn inside(root: &Path, path: &str) -> RunnerResult<PathBuf> {
    if !is_relative(path) {
        return Err(RunnerError::new(
            CODE_PATH_INVALID,
            format!("{path:?} is not a relative path inside the directory"),
        ));
    }
    if !root.is_dir() {
        return Err(RunnerError::new(ROOT_MISSING, format!("{} is not a folder", root.display())));
    }
    let full = path.split('/').fold(root.to_path_buf(), |full, part| full.join(part));
    let base = root.canonicalize().map_err(|error| io("cannot read", root, error))?;
    // The deepest part that exists, resolved, must still be under the root.
    let mut existing = full.as_path();
    while !existing.exists() {
        existing = existing.parent().unwrap_or(root);
    }
    let resolved = existing.canonicalize().map_err(|error| io("cannot read", existing, error))?;
    if !resolved.starts_with(&base) {
        return Err(RunnerError::new(CODE_PATH_INVALID, format!("{path:?} leads out of the directory")));
    }
    Ok(full)
}

/// Whether `path` is a relative, `/`-separated path that names something
/// inside its folder by its text alone: not empty, no leading `/`, no `\\`,
/// `:` or NUL, no empty, `.` or `..` part. The one rule for every path a
/// caller names (`fs.*`, the files `code.check` is given).
pub fn is_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && !path.contains('\0')
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The SHA-256 of `bytes` in lower-case hex: the version a save names.
fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The content of the file at `full`, `None` when there is none.
fn current(full: &Path) -> RunnerResult<Option<Vec<u8>>> {
    match std::fs::read(full) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io("cannot read", full, error)),
    }
}

/// `fs.list`.
pub fn list(root: &Path) -> RunnerResult<Value> {
    let (files, folders, truncated) = list_entries(root)?;
    let files: Vec<Value> = files.into_iter().map(|(path, size)| json!({"path": path, "size": size})).collect();
    Ok(json!({"files": files, "dirs": folders, "truncated": truncated}))
}

/// `fs.mkdir`: the folder `path` made (with the folders above it);
/// `made: false` when it is there already.
pub fn mkdir(root: &Path, path: &str) -> RunnerResult<Value> {
    let full = inside(root, path)?;
    if full.is_dir() {
        return Ok(json!({"path": path, "made": false}));
    }
    if full.exists() {
        return Err(RunnerError::new(CODE_CONFLICT, format!("{path} is a file")));
    }
    std::fs::create_dir_all(&full).map_err(|error| io("cannot make", &full, error))?;
    Ok(json!({"path": path, "made": true}))
}

/// The file or folder `from` and where `to` is to be, both inside the
/// directory; `to` must not be there yet (nothing is written over), nor
/// inside `from`.
fn source_and_target(root: &Path, from: &str, to: &str) -> RunnerResult<(PathBuf, PathBuf)> {
    let source = inside(root, from)?;
    let target = inside(root, to)?;
    if !(source.exists() || source.is_symlink()) {
        return Err(RunnerError::new(CODE_NOT_FOUND, format!("{from} is not there")));
    }
    if target.exists() || target.is_symlink() {
        return Err(RunnerError::new(CODE_CONFLICT, format!("{to} is there already; nothing was written over")));
    }
    if to == from || to.starts_with(&format!("{from}/")) {
        return Err(RunnerError::new(CODE_PATH_INVALID, format!("{to} is inside {from}")));
    }
    if let Some(folder) = target.parent() {
        std::fs::create_dir_all(folder).map_err(|error| io("cannot make", folder, error))?;
    }
    Ok((source, target))
}

/// `fs.move`: rename or move a file or folder within the directory.
pub fn move_path(root: &Path, from: &str, to: &str) -> RunnerResult<Value> {
    let (source, target) = source_and_target(root, from, to)?;
    std::fs::rename(&source, &target).map_err(|error| io("cannot move", &source, error))?;
    Ok(json!({"from": from, "to": to}))
}

/// `fs.copy`: copy a file or folder (with what is in it, links copied as
/// links' targets are not followed) within the directory.
pub fn copy_path(root: &Path, from: &str, to: &str) -> RunnerResult<Value> {
    let (source, target) = source_and_target(root, from, to)?;
    let mut copied = 0usize;
    copy_tree(&source, &target, &mut copied)?;
    Ok(json!({"from": from, "to": to, "files": copied}))
}

/// Copy `source` to `target`, folders with what is in them, counting the files
/// into `copied` (`code_too_large` past [`MAX_FILES`]). Anything neither a
/// file nor a folder (a link) is left out.
fn copy_tree(source: &Path, target: &Path, copied: &mut usize) -> RunnerResult<()> {
    let kind = std::fs::symlink_metadata(source).map_err(|error| io("cannot read", source, error))?.file_type();
    if kind.is_dir() {
        std::fs::create_dir_all(target).map_err(|error| io("cannot make", target, error))?;
        for entry in std::fs::read_dir(source).map_err(|error| io("cannot read", source, error))?.filter_map(Result::ok)
        {
            copy_tree(&entry.path(), &target.join(entry.file_name()), copied)?;
        }
    } else if kind.is_file() {
        if *copied >= MAX_FILES {
            return Err(RunnerError::new(CODE_TOO_LARGE, format!("more than {MAX_FILES} files to copy")));
        }
        std::fs::copy(source, target).map_err(|error| io("cannot copy", source, error))?;
        *copied += 1;
    }
    Ok(())
}

/// `fs.delete`: a file, or a folder with what is in it; never the
/// directory itself.
pub fn delete(root: &Path, path: &str) -> RunnerResult<Value> {
    let full = inside(root, path)?;
    let kind = std::fs::symlink_metadata(&full)
        .map_err(|_| RunnerError::new(CODE_NOT_FOUND, format!("{path} is not there")))?
        .file_type();
    if kind.is_dir() {
        std::fs::remove_dir_all(&full).map_err(|error| io("cannot delete", &full, error))?;
    } else {
        std::fs::remove_file(&full).map_err(|error| io("cannot delete", &full, error))?;
    }
    Ok(json!({"path": path, "deleted": true}))
}

/// `fs.read`.
pub fn read(root: &Path, path: &str) -> RunnerResult<Value> {
    let full = inside(root, path)?;
    if full.is_dir() {
        return Err(RunnerError::new(CODE_NOT_FOUND, format!("{path} is a folder")));
    }
    let size = match std::fs::metadata(&full) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RunnerError::new(CODE_NOT_FOUND, format!("{path} is not in the directory")))
        }
        Err(error) => return Err(io("cannot read", &full, error)),
    };
    if size > MAX_TEXT {
        return Err(RunnerError::new(
            CODE_TOO_LARGE,
            format!("{path} is {size} bytes; the editor opens files of up to {MAX_TEXT}"),
        ));
    }
    let bytes =
        current(&full)?.ok_or_else(|| RunnerError::new(CODE_NOT_FOUND, format!("{path} is not in the directory")))?;
    let hash = sha256(&bytes);
    let size = bytes.len();
    let text =
        String::from_utf8(bytes).map_err(|_| RunnerError::new(CODE_BINARY, format!("{path} is not text (UTF-8)")))?;
    Ok(json!({"path": path, "text": text, "sha256": hash, "size": size}))
}

/// `fs.write`: `expected` is the sha256 the editor opened, `None` for a
/// new file.
pub fn write(root: &Path, path: &str, text: &str, expected: Option<&str>) -> RunnerResult<Value> {
    if text.len() as u64 > MAX_TEXT {
        return Err(RunnerError::new(
            CODE_TOO_LARGE,
            format!("the text is {} bytes; files of up to {MAX_TEXT} are saved", text.len()),
        ));
    }
    let full = inside(root, path)?;
    if full.is_dir() {
        return Err(RunnerError::new(CODE_PATH_INVALID, format!("{path} is a folder")));
    }
    let lock = crate::sys::file_lock(&full);
    let _held = crate::sys::hold(&lock);
    let now = current(&full)?;
    let found = now.as_deref().map(sha256);
    if found.as_deref() != expected {
        let message = match (&found, expected) {
            (Some(found), Some(_)) => format!("{path} changed since it was opened (now {found}); nothing was saved"),
            (Some(found), None) => format!("{path} already exists (sha256 {found}); nothing was saved"),
            (None, _) => format!("{path} is no longer there; nothing was saved"),
        };
        return Err(RunnerError::new(CODE_CONFLICT, message));
    }
    let permissions = std::fs::metadata(&full).ok().map(|metadata| metadata.permissions());
    crate::sys::atomic_write_bytes(&full, text.as_bytes()).map_err(|error| io("cannot write", &full, error))?;
    if let Some(permissions) = permissions {
        let _ = std::fs::set_permissions(&full, permissions);
    }
    Ok(json!({"path": path, "sha256": sha256(text.as_bytes()), "size": text.len()}))
}

/// `text` lowercased character by character ([`char::to_lowercase`], the
/// same for a line and a query), with, for each byte of the result, the
/// index of the character of `text` it came from, and whether a
/// character's lowercase starts there.
pub(crate) fn folded(text: &str) -> (String, Vec<(usize, bool)>) {
    let mut lower = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len());
    for (index, character) in text.chars().enumerate() {
        let from = lower.len();
        lower.extend(character.to_lowercase());
        origin.extend((from..lower.len()).map(|at| (index, at == from)));
    }
    (lower, origin)
}

/// Where `query` is on `line`: char columns from 1, strictly increasing,
/// matches not overlapping. Without `match_case`, each character is
/// compared by its lowercase ([`folded`]) and a match starts where one of
/// the line's characters does: a column always names the character of
/// `line` the match starts at, whatever lowercasing does to lengths
/// (homology#2: `"ȺȺK"` and `k` once cut the line inside a character).
pub(crate) fn found_in(line: &str, query: &str, match_case: bool) -> Vec<usize> {
    let mut columns = Vec::new();
    if query.is_empty() {
        return columns;
    }
    if match_case {
        for (at, _) in line.match_indices(query) {
            columns.push(line[..at].chars().count() + 1);
        }
        return columns;
    }
    let (hay, origin) = folded(line);
    let (needle, _) = folded(query);
    let mut from = 0;
    while let Some(at) = hay.get(from..).and_then(|rest| rest.find(&needle)) {
        let start = from + at;
        match origin.get(start) {
            Some(&(index, true)) => {
                columns.push(index + 1);
                from = start + needle.len();
            }
            // Inside one character's lowercase: look on from its next byte
            // that starts a character of the folded text.
            _ => from = start + hay[start..].chars().next().map_or(1, char::len_utf8),
        }
    }
    columns
}

/// `fs.search`: the lines of the directory's text files where `query` is
/// (files as [`list_files`] walks them, none over [`MAX_TEXT`], none not
/// UTF-8): `{matches: [{path, line, col, text}], files, truncated}`,
/// lines and columns from 1, a line's text cut at [`MATCH_LINE`] chars;
/// `truncated` when there were more than [`MAX_MATCHES`] or more files
/// than were listed.
pub fn search(root: &Path, query: &str, match_case: bool) -> RunnerResult<Value> {
    if query.is_empty() {
        return Err(RunnerError::new(INVALID_PARAMS, "query is the text to find, not empty"));
    }
    if !root.is_dir() {
        return Err(RunnerError::new(ROOT_MISSING, format!("{} is not a directory", root.display())));
    }
    let (files, mut truncated) = list_files(root)?;
    let mut matches = Vec::new();
    let mut searched = 0;
    'files: for (path, size) in files {
        if size > MAX_TEXT {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else { continue };
        searched += 1;
        for (index, line) in text.lines().enumerate() {
            for col in found_in(line, query, match_case) {
                if matches.len() == MAX_MATCHES {
                    truncated = true;
                    break 'files;
                }
                let shown: String = line.chars().take(MATCH_LINE).collect();
                matches.push(json!({"path": path, "line": index + 1, "col": col, "text": shown}));
            }
        }
    }
    Ok(json!({"matches": matches, "files": searched, "truncated": truncated}))
}

/// Answer `fs.*`: the runner's operation, and what a caller calls directly
/// for a directory on its own computer.
pub fn op(op: &str, params: &JsonMap) -> RunnerResult<Value> {
    let text = |key: &str| params::text(params, key);
    let root = params::host_path(text("root")?)?;
    match op {
        "fs.list" => list(&root),
        "fs.read" => read(&root, text("path")?),
        "fs.write" => {
            let expected = match params.get("sha256") {
                None | Some(Value::Null) => None,
                Some(Value::String(hash)) => Some(hash.as_str()),
                Some(_) => return Err(RunnerError::new(INVALID_PARAMS, "sha256 is text or null")),
            };
            write(&root, text("path")?, text("text")?, expected)
        }
        "fs.mkdir" => mkdir(&root, text("path")?),
        "fs.move" => move_path(&root, text("from")?, text("to")?),
        "fs.copy" => copy_path(&root, text("from")?, text("to")?),
        "fs.delete" => delete(&root, text("path")?),
        "fs.search" => {
            let match_case = params.get("match_case").and_then(Value::as_bool).unwrap_or(false);
            search(&root, text("query")?, match_case)
        }
        other => Err(RunnerError::new(UNKNOWN_OP, format!("no operation {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Finding text in the directory's files (crescent#1, step 8): every
    /// place in each line, case ignored unless asked, never in skipped
    /// folders or files that are not text.
    #[test]
    fn text_is_found_in_the_directory_s_text_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".venv")).unwrap();
        std::fs::write(root.join("src/a.py"), "loss = Loss()\nprint(loss)\n").unwrap();
        std::fs::write(root.join("中.py"), "# 损失 loss\n").unwrap();
        std::fs::write(root.join(".venv/x.py"), "loss\n").unwrap();
        std::fs::write(root.join("w.bin"), [0xff, 0xfe, b'l', b'o', b's', b's']).unwrap();
        let params = |value: Value| value.as_object().unwrap().clone();
        let found = op("fs.search", &params(json!({"root": root, "query": "loss"}))).unwrap();
        let places: Vec<String> = found["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|place| format!("{}:{}:{}", place["path"].as_str().unwrap(), place["line"], place["col"]))
            .collect();
        assert_eq!(places, ["src/a.py:1:1", "src/a.py:1:8", "src/a.py:2:7", "中.py:1:6"]);
        assert_eq!(found["files"], 2, "the file that is not text is not searched");
        let exact = search(root, "Loss", true).unwrap();
        assert_eq!(exact["matches"].as_array().unwrap().len(), 1);
        assert_eq!(exact["matches"][0]["text"], "loss = Loss()");
        assert_eq!(op("fs.search", &params(json!({"root": root, "query": ""}))).unwrap_err().code, INVALID_PARAMS);
    }

    /// Characters whose lowercase has another length, before or at a match
    /// (homology#2: `"ȺȺK"` searched for `k` cut the line inside a
    /// character and ended the runner).
    #[test]
    fn lowercasing_that_changes_lengths_still_finds_by_character() {
        assert_eq!(found_in("ȺȺK", "k", false), [3]);
        assert_eq!(found_in("ȺȺK", "k", true), Vec::<usize>::new());
        assert_eq!(found_in("İstanbul İ", "i", false), [1, 10], "İ lowercases to i and a dot");
        assert_eq!(found_in("ΣΑΣ σας", "σ", false), [1, 3, 5], "a final ς is not σ");
        assert_eq!(found_in("aaaa", "aa", true), [1, 3], "matches do not overlap");
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(temporary.path().join("u.py"), "x = 'ȺȺK'\n").unwrap();
        let found = search(temporary.path(), "k", false).unwrap();
        assert_eq!((found["matches"][0]["line"].as_u64(), found["matches"][0]["col"].as_u64()), (Some(1), Some(8)));
    }

    /// The explorer's operations (2026-11-06): within the directory only,
    /// never over what is there, never the directory itself.
    #[test]
    fn files_and_folders_are_made_moved_copied_and_deleted_inside_the_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "a").unwrap();
        assert_eq!(mkdir(root, "empty/inner").unwrap()["made"], true);
        assert_eq!(mkdir(root, "empty/inner").unwrap()["made"], false);
        let listed = list(root).unwrap();
        assert_eq!(listed["dirs"], json!(["empty", "empty/inner", "src"]));
        assert_eq!(copy_path(root, "src", "lib").unwrap()["files"], 1);
        assert_eq!(std::fs::read_to_string(root.join("lib/a.py")).unwrap(), "a");
        assert_eq!(copy_path(root, "src/a.py", "lib/a.py").unwrap_err().code, CODE_CONFLICT, "nothing is written over");
        assert_eq!(copy_path(root, "src", "src/deeper").unwrap_err().code, CODE_PATH_INVALID);
        move_path(root, "lib/a.py", "lib/b.py").unwrap();
        assert!(root.join("lib/b.py").is_file() && !root.join("lib/a.py").exists());
        assert_eq!(move_path(root, "nope.py", "x.py").unwrap_err().code, CODE_NOT_FOUND);
        delete(root, "lib").unwrap();
        assert!(!root.join("lib").exists());
        assert_eq!(delete(root, "../x").unwrap_err().code, CODE_PATH_INVALID);
        assert_eq!(delete(root, "").unwrap_err().code, CODE_PATH_INVALID, "never the directory itself");
        let asked = json!({"root": root, "path": "made"}).as_object().unwrap().clone();
        assert_eq!(op("fs.mkdir", &asked).unwrap()["made"], true);
    }

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/__pycache__")).unwrap();
        std::fs::write(dir.path().join("src/train.py"), "print('hi')\n").unwrap();
        std::fs::write(dir.path().join("src/__pycache__/train.pyc"), [0u8, 159, 146]).unwrap();
        std::fs::write(dir.path().join("weights.bin"), [0u8, 159, 146, 150]).unwrap();
        dir
    }

    #[test]
    fn a_folder_is_listed_without_its_caches() {
        let dir = folder();
        let listed = list(dir.path()).unwrap();
        assert_eq!(listed["files"], json!([{"path": "src/train.py", "size": 12}, {"path": "weights.bin", "size": 4}]));
        assert_eq!(listed["truncated"], false);
        assert_eq!(list(&dir.path().join("nope")).unwrap_err().code, ROOT_MISSING);
    }

    #[test]
    fn a_file_is_read_with_its_hash_and_text_only() {
        let dir = folder();
        let file = read(dir.path(), "src/train.py").unwrap();
        assert_eq!(file["text"], "print('hi')\n");
        assert_eq!(file["sha256"], sha256(b"print('hi')\n"));
        assert_eq!(read(dir.path(), "weights.bin").unwrap_err().code, CODE_BINARY);
        assert_eq!(read(dir.path(), "src/none.py").unwrap_err().code, CODE_NOT_FOUND);
        assert_eq!(read(dir.path(), "src").unwrap_err().code, CODE_NOT_FOUND);
        for bad in ["../x.py", "/etc/passwd", "src/../../x", "a\\b", "", "C:x"] {
            assert_eq!(read(dir.path(), bad).unwrap_err().code, CODE_PATH_INVALID, "{bad}");
        }
    }

    #[test]
    fn a_file_is_saved_only_over_what_was_opened() {
        let dir = folder();
        let opened = read(dir.path(), "src/train.py").unwrap()["sha256"].as_str().unwrap().to_string();
        let saved = write(dir.path(), "src/train.py", "print('bye')\n", Some(&opened)).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("src/train.py")).unwrap(), "print('bye')\n");
        assert_eq!(saved["sha256"], sha256(b"print('bye')\n"));
        // Saved again with the old hash: someone else's change is not lost.
        assert_eq!(write(dir.path(), "src/train.py", "lost\n", Some(&opened)).unwrap_err().code, CODE_CONFLICT);
        assert_eq!(std::fs::read_to_string(dir.path().join("src/train.py")).unwrap(), "print('bye')\n");
        // A new file only where there is none.
        write(dir.path(), "src/new/model.py", "x = 1\n", None).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("src/new/model.py")).unwrap(), "x = 1\n");
        assert_eq!(write(dir.path(), "src/new/model.py", "x = 2\n", None).unwrap_err().code, CODE_CONFLICT);
        // A file removed since it was opened.
        std::fs::remove_file(dir.path().join("src/new/model.py")).unwrap();
        assert_eq!(
            write(dir.path(), "src/new/model.py", "x = 3\n", Some(&sha256(b"x = 1\n"))).unwrap_err().code,
            CODE_CONFLICT
        );
        assert!(!dir.path().join("src/new/model.py").exists());
        let left = std::fs::read_dir(dir.path().join("src")).unwrap().flatten();
        assert!(
            !left.into_iter().any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")),
            "a temporary file is left"
        );
    }

    #[cfg(unix)]
    #[test]
    fn saving_keeps_permissions_and_links_cannot_lead_out() {
        use std::os::unix::fs::PermissionsExt;
        let dir = folder();
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        write(dir.path(), "run.sh", "#!/bin/sh\necho hi\n", Some(&sha256(b"#!/bin/sh\n"))).unwrap();
        assert_eq!(std::fs::metadata(&script).unwrap().permissions().mode() & 0o777, 0o755);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "no\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        assert_eq!(read(dir.path(), "out/secret.txt").unwrap_err().code, CODE_PATH_INVALID);
        assert_eq!(write(dir.path(), "out/new.txt", "x", None).unwrap_err().code, CODE_PATH_INVALID);
    }

    #[test]
    fn the_operations_take_their_params() {
        let dir = folder();
        let root = dir.path().to_string_lossy().to_string();
        let params = |value: Value| value.as_object().unwrap().clone();
        assert_eq!(op("fs.read", &params(json!({"root": root, "path": "src/train.py"}))).unwrap()["size"], 12);
        assert_eq!(
            op("fs.write", &params(json!({"root": root, "path": "a.py", "text": "", "sha256": 3}))).unwrap_err().code,
            INVALID_PARAMS
        );
        assert_eq!(
            op("fs.read", &params(json!({"root": "relative", "path": "a.py"}))).unwrap_err().code,
            INVALID_PARAMS
        );
        assert_eq!(op("fs.stat", &params(json!({"root": root}))).unwrap_err().code, UNKNOWN_OP);
    }
}
