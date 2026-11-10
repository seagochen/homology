//! Writing a file never follows a link someone left beside it (#3): a
//! `<name>.tmp` symbolic link in the directory, pointing outside it, is
//! neither written through nor moved into the file's place. Unix: making a
//! symbolic link on Windows takes a privilege.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::symlink;

/// A link at the old temporary name to a file outside: the file outside
/// keeps its text, the one written is a plain file inside the root.
#[test]
fn a_link_at_the_temporary_name_is_not_written_through() {
    let base = tempfile::tempdir().unwrap();
    let (root, outside) = (base.path().join("root"), base.path().join("outside"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("victim.txt"), "ORIGINAL").unwrap();
    symlink(outside.join("victim.txt"), root.join("a.py.tmp")).unwrap();

    homology_runner::fs::write(&root, "a.py", "WRITTEN BY RUNNER", None).unwrap();

    assert_eq!(fs::read_to_string(outside.join("victim.txt")).unwrap(), "ORIGINAL");
    let written = fs::symlink_metadata(root.join("a.py")).unwrap();
    assert!(written.file_type().is_file(), "a.py is a plain file, not a link");
    assert_eq!(fs::read_to_string(root.join("a.py")).unwrap(), "WRITTEN BY RUNNER");
    assert!(fs::symlink_metadata(root.join("a.py.tmp")).unwrap().file_type().is_symlink(), "the link is left alone");
}

/// A dangling link at the old temporary name: nothing is made outside.
#[test]
fn a_dangling_link_at_the_temporary_name_makes_nothing_outside() {
    let base = tempfile::tempdir().unwrap();
    let (root, outside) = (base.path().join("root"), base.path().join("outside"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(outside.join("created.txt"), root.join("b.py.tmp")).unwrap();

    homology_runner::fs::write(&root, "b.py", "NEW FILE OUTSIDE", None).unwrap();

    assert!(!outside.join("created.txt").exists(), "nothing was made outside the root");
    assert!(fs::symlink_metadata(root.join("b.py")).unwrap().file_type().is_file());
    assert_eq!(fs::read_to_string(root.join("b.py")).unwrap(), "NEW FILE OUTSIDE");
    let left: Vec<_> = fs::read_dir(&root).unwrap().map(|entry| entry.unwrap().file_name()).collect();
    assert_eq!(left.len(), 2, "no temporary file is left behind: {left:?}");
}
