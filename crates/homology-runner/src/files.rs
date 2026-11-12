//! Any folder of the computer the runner is on, for a file manager
//! (#23, crescent#61): listed one level at a time with each entry's kind,
//! size, time and permissions; folders made, entries renamed, deleted and
//! their permissions changed; files read and written a block at a time,
//! so that a transfer shows its progress and can be stopped, and finished
//! by a rename, so that no half-written file is ever where it was going.
//!
//! - `files.list {dir}`: `{dir, parent, entries: [{name, kind, size,
//!   modified, mode, to_dir}]}`, `dir` absolute as resolved, folders
//!   first, then by name; `kind` is `file`, `dir`, `link` or `other`;
//!   `to_dir` whether a link leads to a folder; `mode` the permission
//!   bits on Unix (null elsewhere); `modified` Unix seconds.
//! - `files.stat {path}`: `{exists}` and, when it does, the entry's fields.
//! - `files.mkdir {path}`: made with any folders above it.
//! - `files.rename {from, to}`: never over something there ([`EXISTS`]).
//! - `files.delete {path}`: a file, a link (itself, never what it leads
//!   to), or a folder with everything in it.
//! - `files.chmod {path, mode}`: Unix only ([`UNSUPPORTED`] elsewhere).
//! - `files.read {path, offset, length}`: `{base64, size, eof}`, at most
//!   [`MAX_BLOCK`] bytes from `offset`.
//! - `files.write {path, offset, base64}`: at 0 the file is made (or
//!   emptied); later blocks only at its end ([`OFFSET`]).
//! - `files.finish {from, to, replace}`: the file written renamed to where
//!   it goes; over something there only when `replace`.
//! - `files.hash {path}`: `{sha256, size}`.
//!
//! Paths are absolute, or `~` / `~/…` in the home folder. The same code
//! answers for the caller's own computer: it calls [`op`] directly.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use base64::Engine;
use serde_json::{json, Value};

use crate::protocol::{RunnerError, RunnerResult, INVALID_PARAMS, UNKNOWN_OP};
use crate::{params, JsonMap};

/// The codes the file manager's operations fail with.
pub const MISSING: &str = "files_missing";
pub const EXISTS: &str = "files_exists";
pub const DENIED: &str = "files_denied";
pub const OFFSET: &str = "files_offset";
pub const UNSUPPORTED: &str = "files_unsupported";
pub const IO: &str = "files_io";

/// The most bytes a block read or written holds: base64, it fits in a
/// protocol line ([`crate::protocol::MAX_LINE`]) with room to spare.
pub const MAX_BLOCK: u64 = 4 * 1024 * 1024;

/// The operations of this module, as the runner lists them.
pub const OPS: &[&str] = &[
    "files.list", "files.stat", "files.mkdir", "files.rename", "files.delete", "files.chmod", "files.read",
    "files.write", "files.finish", "files.hash",
];

/// A failure of `what` on `path`, by why: not there, not allowed, else IO.
fn failed(what: &str, path: &Path, error: &std::io::Error) -> RunnerError {
    let code = match error.kind() {
        std::io::ErrorKind::NotFound => MISSING,
        std::io::ErrorKind::PermissionDenied => DENIED,
        std::io::ErrorKind::AlreadyExists => EXISTS,
        _ => IO,
    };
    RunnerError::new(code, format!("{what} {}: {error}", path.display()))
}

/// The number at `key` (`invalid_params` when it is missing or not a whole number).
fn number(params: &JsonMap, key: &str) -> RunnerResult<u64> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| RunnerError::new(INVALID_PARAMS, format!("{key} is a number")))
}

/// What a listing says of an entry named `name`, from what is known of it
/// without following a link (`link`) and, for a link, of what it leads to.
fn entry(name: &str, path: &Path) -> Value {
    let Ok(link) = std::fs::symlink_metadata(path) else {
        return json!({"name": name, "kind": "other", "size": 0, "modified": null, "mode": null, "to_dir": false});
    };
    let kind = if link.file_type().is_symlink() {
        "link"
    } else if link.is_dir() {
        "dir"
    } else if link.is_file() {
        "file"
    } else {
        "other"
    };
    // A link's size and time are what it leads to, as a file manager shows them.
    let shown = std::fs::metadata(path).unwrap_or(link);
    let modified = shown
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs());
    json!({
        "name": name,
        "kind": kind,
        "size": if shown.is_dir() { 0 } else { shown.len() },
        "modified": modified,
        "mode": mode(&shown),
        "to_dir": kind == "link" && shown.is_dir(),
    })
}

/// The permission bits of an entry (Unix); none elsewhere.
#[cfg(unix)]
fn mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

/// The permission bits of an entry (Unix); none elsewhere.
#[cfg(not(unix))]
fn mode(metadata: &std::fs::Metadata) -> Option<u32> {
    let _ = metadata;
    None
}

/// `files.list`: the folder `dir`, one level, folders first, then by name.
fn list(dir: &Path) -> RunnerResult<Value> {
    let resolved = std::fs::canonicalize(dir).map_err(|error| failed("list", dir, &error))?;
    let entries = std::fs::read_dir(&resolved).map_err(|error| failed("list", &resolved, &error))?;
    let mut listed: Vec<Value> =
        entries.filter_map(Result::ok).map(|item| entry(&item.file_name().to_string_lossy(), &item.path())).collect();
    let folder = |entry: &Value| entry["kind"] == "dir" || entry["to_dir"] == true;
    listed.sort_by(|a, b| {
        let name = |entry: &Value| entry["name"].as_str().unwrap_or_default().to_lowercase();
        folder(b).cmp(&folder(a)).then_with(|| name(a).cmp(&name(b)))
    });
    let text = |path: &Path| path.to_string_lossy().into_owned();
    Ok(json!({"dir": text(&resolved), "parent": resolved.parent().map(text), "entries": listed}))
}

/// `files.stat`: whether `path` is there, and what it is.
fn stat(path: &Path) -> Value {
    if std::fs::symlink_metadata(path).is_err() {
        return json!({"exists": false});
    }
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let mut found = entry(&name, path);
    if let Some(fields) = found.as_object_mut() {
        fields.insert("exists".into(), json!(true));
    }
    found
}

/// `files.rename`: `from` to `to`, never over something there.
fn rename(from: &Path, to: &Path) -> RunnerResult<Value> {
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(RunnerError::new(EXISTS, format!("{} is there already", to.display())));
    }
    std::fs::rename(from, to).map_err(|error| failed("rename", from, &error))?;
    Ok(json!({}))
}

/// `files.delete`: a file or link itself, or a folder with all in it (a
/// link in it removed, never followed).
fn delete(path: &Path) -> RunnerResult<Value> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| failed("delete", path, &error))?;
    let removed = if metadata.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    removed.map_err(|error| failed("delete", path, &error))?;
    Ok(json!({}))
}

/// `files.chmod`: the permission bits of `path` set to `mode`.
#[cfg(unix)]
fn chmod(path: &Path, mode: u64) -> RunnerResult<Value> {
    use std::os::unix::fs::PermissionsExt;
    let mode = u32::try_from(mode).ok().filter(|mode| *mode <= 0o7777);
    let mode = mode.ok_or_else(|| RunnerError::new(INVALID_PARAMS, "mode is at most 0o7777"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| failed("chmod", path, &error))?;
    Ok(json!({}))
}

/// `files.chmod`: Unix only.
#[cfg(not(unix))]
fn chmod(path: &Path, mode: u64) -> RunnerResult<Value> {
    let _ = mode;
    Err(RunnerError::new(UNSUPPORTED, format!("{}: permissions are Unix's", path.display())))
}

/// `files.read`: at most [`MAX_BLOCK`] bytes of `path` from `offset`.
fn read(path: &Path, offset: u64, length: u64) -> RunnerResult<Value> {
    let mut file = std::fs::File::open(path).map_err(|error| failed("read", path, &error))?;
    let size = file.metadata().map_err(|error| failed("read", path, &error))?.len();
    file.seek(SeekFrom::Start(offset)).map_err(|error| failed("read", path, &error))?;
    let mut block = Vec::new();
    file.take(length.min(MAX_BLOCK)).read_to_end(&mut block).map_err(|error| failed("read", path, &error))?;
    let eof = offset + block.len() as u64 >= size;
    Ok(json!({"base64": base64::engine::general_purpose::STANDARD.encode(&block), "size": size, "eof": eof}))
}

/// `files.write`: a block at `offset` (0 makes or empties the file; later
/// blocks only at its end).
fn write(path: &Path, offset: u64, encoded: &str) -> RunnerResult<Value> {
    let block = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| RunnerError::new(INVALID_PARAMS, format!("base64: {error}")))?;
    if block.len() as u64 > MAX_BLOCK {
        return Err(RunnerError::new(INVALID_PARAMS, format!("a block is at most {MAX_BLOCK} bytes")));
    }
    let mut file =
        if offset == 0 { std::fs::File::create(path) } else { std::fs::OpenOptions::new().append(true).open(path) }
            .map_err(|error| failed("write", path, &error))?;
    let size = file.metadata().map_err(|error| failed("write", path, &error))?.len();
    if size != offset {
        return Err(RunnerError::new(OFFSET, format!("{} has {size} bytes, not {offset}", path.display())));
    }
    file.write_all(&block).map_err(|error| failed("write", path, &error))?;
    Ok(json!({"size": offset + block.len() as u64}))
}

/// `files.finish`: the file written at `from` renamed to `to`; over
/// something there only when `replace`.
fn finish(from: &Path, to: &Path, replace: bool) -> RunnerResult<Value> {
    if std::fs::symlink_metadata(to).is_ok() {
        if !replace {
            return Err(RunnerError::new(EXISTS, format!("{} is there already", to.display())));
        }
        // Windows renames over nothing; a folder there is not a file to replace.
        if cfg!(windows) || std::fs::symlink_metadata(to).is_ok_and(|metadata| metadata.is_dir()) {
            delete(to)?;
        }
    }
    std::fs::rename(from, to).map_err(|error| failed("finish", from, &error))?;
    Ok(json!({}))
}

/// `files.hash`: the SHA-256 of the file at `path`, read a block at a time.
fn hash(path: &Path) -> RunnerResult<Value> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path).map_err(|error| failed("hash", path, &error))?;
    let mut hasher = sha2::Sha256::new();
    let mut block = vec![0; 1024 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut block).map_err(|error| failed("hash", path, &error))?;
        if read == 0 {
            break;
        }
        hasher.update(&block[..read]);
        size += read as u64;
    }
    Ok(json!({"sha256": hex::encode(hasher.finalize()), "size": size}))
}

/// Answer `files.*`: the runner's operation, and what a caller calls
/// directly for its own computer.
pub fn op(op: &str, params: &JsonMap) -> RunnerResult<Value> {
    let path = |key: &str| params::host_path(params::text(params, key)?);
    match op {
        "files.list" => list(&path("dir")?),
        "files.stat" => Ok(stat(&path("path")?)),
        "files.mkdir" => {
            let made = path("path")?;
            std::fs::create_dir_all(&made).map_err(|error| failed("mkdir", &made, &error))?;
            Ok(json!({}))
        }
        "files.rename" => rename(&path("from")?, &path("to")?),
        "files.delete" => delete(&path("path")?),
        "files.chmod" => chmod(&path("path")?, number(params, "mode")?),
        "files.read" => read(&path("path")?, number(params, "offset")?, number(params, "length")?),
        "files.write" => write(&path("path")?, number(params, "offset")?, params::text(params, "base64")?),
        "files.finish" => {
            let replace = params.get("replace").and_then(Value::as_bool).unwrap_or(false);
            finish(&path("from")?, &path("to")?, replace)
        }
        "files.hash" => hash(&path("path")?),
        other => Err(RunnerError::new(UNKNOWN_OP, format!("no operation {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// `params` as a request's.
    fn ask(op_name: &str, params: Value) -> RunnerResult<Value> {
        op(op_name, params.as_object().unwrap())
    }

    /// The code a request failed with.
    fn code(answer: RunnerResult<Value>) -> String {
        answer.unwrap_err().code
    }

    #[test]
    fn a_folder_anywhere_is_listed_one_level_folders_first() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        std::fs::create_dir(dir.join("zeta")).unwrap();
        std::fs::create_dir(dir.join("Alpha")).unwrap();
        std::fs::write(dir.join("名前.txt"), "名前\n").unwrap();
        std::fs::write(dir.join("b.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(dir.join("zeta/deep.txt"), "not listed").unwrap();
        let listed = ask("files.list", json!({"dir": dir})).unwrap();
        let names: Vec<&str> =
            listed["entries"].as_array().unwrap().iter().map(|entry| entry["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Alpha", "zeta", "b.bin", "名前.txt"]);
        let file = &listed["entries"][3];
        assert_eq!((file["kind"].as_str(), file["size"].as_u64()), (Some("file"), Some(7)));
        assert!(file["modified"].as_u64().unwrap() > 1_600_000_000);
        assert_eq!(listed["dir"], json!(std::fs::canonicalize(dir).unwrap().to_string_lossy()));
        assert!(listed["parent"].is_string());
        assert_eq!(code(ask("files.list", json!({"dir": dir.join("none")}))), MISSING);
        assert_eq!(code(ask("files.list", json!({"dir": "relative/path"}))), INVALID_PARAMS);
    }

    #[test]
    fn the_home_folder_is_named_by_a_tilde() {
        let Some(home) = homology_sys::home_dir() else { return };
        let listed = ask("files.list", json!({"dir": "~"})).unwrap();
        assert_eq!(listed["dir"], json!(std::fs::canonicalize(home).unwrap().to_string_lossy()));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_told_and_deleting_a_folder_never_follows_one() {
        let folder = tempfile::tempdir().unwrap();
        let (dir, outside) = (folder.path().join("dir"), folder.path().join("outside"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "kept").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("to-outside")).unwrap();
        let listed = ask("files.list", json!({"dir": dir})).unwrap();
        let link = &listed["entries"][0];
        assert_eq!((link["kind"].as_str(), link["to_dir"].as_bool()), (Some("link"), Some(true)));
        ask("files.delete", json!({"path": dir})).unwrap();
        assert!(!dir.exists());
        assert_eq!(std::fs::read_to_string(outside.join("keep.txt")).unwrap(), "kept", "not followed");
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_shown_and_changed() {
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("run.sh");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        ask("files.chmod", json!({"path": file, "mode": 0o750})).unwrap();
        assert_eq!(ask("files.stat", json!({"path": file})).unwrap()["mode"], 0o750);
        assert_eq!(code(ask("files.chmod", json!({"path": file, "mode": 0o17777}))), INVALID_PARAMS);
    }

    #[test]
    fn folders_are_made_entries_renamed_never_over_another_and_deleted() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        ask("files.mkdir", json!({"path": dir.join("a/b/c")})).unwrap();
        assert!(dir.join("a/b/c").is_dir());
        std::fs::write(dir.join("one.txt"), "1").unwrap();
        std::fs::write(dir.join("two.txt"), "2").unwrap();
        assert_eq!(code(ask("files.rename", json!({"from": dir.join("one.txt"), "to": dir.join("two.txt")}))), EXISTS);
        assert_eq!(std::fs::read_to_string(dir.join("two.txt")).unwrap(), "2", "nothing over it");
        ask("files.rename", json!({"from": dir.join("one.txt"), "to": dir.join("a/one.txt")})).unwrap();
        assert!(dir.join("a/one.txt").is_file());
        ask("files.delete", json!({"path": dir.join("a")})).unwrap();
        assert!(!dir.join("a").exists());
        assert_eq!(ask("files.stat", json!({"path": dir.join("a")})).unwrap(), json!({"exists": false}));
        assert_eq!(code(ask("files.delete", json!({"path": dir.join("a")}))), MISSING);
    }

    #[test]
    fn a_file_copied_a_block_at_a_time_is_the_same_and_finished_by_a_rename() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path();
        let bytes: Vec<u8> = (0..(MAX_BLOCK as usize * 2 + 12345)).map(|at| (at * 31 % 251) as u8).collect();
        std::fs::write(dir.join("from.bin"), &bytes).unwrap();
        let part = dir.join(".to.bin.part");
        let mut offset = 0u64;
        loop {
            let block =
                ask("files.read", json!({"path": dir.join("from.bin"), "offset": offset, "length": MAX_BLOCK * 4}))
                    .unwrap();
            let encoded = block["base64"].as_str().unwrap();
            let written = ask("files.write", json!({"path": part, "offset": offset, "base64": encoded})).unwrap();
            offset = written["size"].as_u64().unwrap();
            assert_eq!(block["size"].as_u64(), Some(bytes.len() as u64));
            if block["eof"] == true {
                break;
            }
        }
        assert_eq!(offset, bytes.len() as u64, "blocks of at most MAX_BLOCK, every byte");
        // A block not at the file's end is refused.
        assert_eq!(code(ask("files.write", json!({"path": part, "offset": 5, "base64": "AA=="}))), OFFSET);
        std::fs::write(dir.join("to.bin"), "there").unwrap();
        assert_eq!(code(ask("files.finish", json!({"from": part, "to": dir.join("to.bin")}))), EXISTS);
        assert_eq!(std::fs::read_to_string(dir.join("to.bin")).unwrap(), "there", "not replaced unasked");
        ask("files.finish", json!({"from": part, "to": dir.join("to.bin"), "replace": true})).unwrap();
        assert!(!part.exists());
        let (from, to) = (
            ask("files.hash", json!({"path": dir.join("from.bin")})).unwrap(),
            ask("files.hash", json!({"path": dir.join("to.bin")})).unwrap(),
        );
        assert_eq!(from, to);
        assert_eq!(from["size"].as_u64(), Some(bytes.len() as u64));
        // An empty file: one read, at its end.
        std::fs::write(dir.join("empty"), "").unwrap();
        assert_eq!(
            ask("files.read", json!({"path": dir.join("empty"), "offset": 0, "length": 10})).unwrap()["eof"],
            true
        );
    }
}
