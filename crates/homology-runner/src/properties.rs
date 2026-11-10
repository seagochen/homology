//! Properties checked on random inputs (homology#2), with `cargo test`:
//! finding text, the path rule, the file operations on random folders with
//! links pointing in, out and nowhere, the protocol's lines both ways,
//! reading bounded lines, and a session's answers to any run of requests.
//! What fixed examples miss (a character boundary, a link at the wrong
//! place) is what these are for; coverage-guided fuzzing of the same
//! entry points is in `fuzz/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(unix)]
use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};
#[cfg(unix)]
use std::path::Path;

use proptest::prelude::*;
use serde_json::{json, Value};

#[cfg(unix)]
use crate::fs::CODE_CONFLICT;
use crate::fs::{self, folded, found_in, is_relative, CODE_PATH_INVALID};
use crate::protocol::{
    read_line, Line, Message, Request, RunnerError, HELLO_REQUIRED, INVALID_MESSAGE, PROTOCOL, RUNNER_INVALID_OUTPUT,
};
use crate::server::{serve, Ended, Runner};
use crate::JsonMap;

/// Text made mostly of characters whose lowercase is another length or
/// another character (`Ⱥ`, Kelvin `K`, `İ`, `ẞ`, final `ς`), and any other.
fn tricky_text() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[ȺⱥKkİiIΣσςßẞéE a\u{212A}\u{0130}\u{1E9E}]{0,14}",
        1 => any::<String>(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Search never panics; its columns rise and each names a character
    /// of the line where the query starts (by the case rule asked for).
    #[test]
    fn a_search_names_the_characters_where_the_query_starts(
        line in tricky_text(), query in "[ȺⱥKkİiσςßa\u{212A}]{1,3}", match_case in any::<bool>()
    ) {
        let columns = found_in(&line, &query, match_case);
        prop_assert!(columns.windows(2).all(|pair| pair[0] < pair[1]), "{columns:?}");
        for column in columns {
            let rest: String = line.chars().skip(column - 1).collect();
            if match_case {
                prop_assert!(rest.starts_with(&query), "{line:?} {query:?} at {column}");
            } else {
                prop_assert!(folded(&rest).0.starts_with(&folded(&query).0), "{line:?} {query:?} at {column}");
            }
        }
    }

    /// Search on any text and query (UTF-8 at all) never panics.
    #[test]
    fn a_search_on_any_text_returns(line in any::<String>(), query in any::<String>(), match_case in any::<bool>()) {
        let _ = found_in(&line, &query, match_case);
    }

    /// The path rule: a path with a `..`, `.` or empty part, a leading
    /// `/`, a `\`, `:` or NUL is refused, by `fs.*` and the code check alike.
    #[test]
    fn paths_that_could_leave_the_folder_are_refused(parts in prop::collection::vec(
        prop_oneof![Just(".."), Just("."), Just(""), Just("a"), Just("b\\c"), Just("c:"), Just("n\0"), Just("ok")], 1..4
    )) {
        let path = parts.join("/");
        let bad = parts.iter().any(|part| matches!(*part, ".." | "." | "" | "b\\c" | "c:" | "n\0"));
        prop_assert_eq!(is_relative(&path), !bad, "{:?}", path);
        let root = tempfile::tempdir().unwrap();
        if bad {
            prop_assert_eq!(fs::read(root.path(), &path).unwrap_err().code, CODE_PATH_INVALID);
            let files = Some(json!([{"path": path, "text": ""}]));
            prop_assert!(crate::check::files_of(files.as_ref()).is_err());
        }
    }
}

/// What a random folder holds, besides the folder `d` it always has.
#[derive(Clone, Debug)]
#[cfg(unix)]
enum Made {
    Dir,
    File(String),
    /// To the folder `d` inside.
    LinkIn,
    /// To the folder outside.
    LinkOutDir,
    /// To a file outside.
    LinkOutFile,
    /// To nothing, outside.
    DanglingOut,
    /// To nothing, inside.
    DanglingIn,
    /// To another link (`l`, made first when it is not there).
    Chain,
}

/// One operation on the folder.
#[derive(Clone, Debug)]
#[cfg(unix)]
enum Op {
    Write(String, String, bool),
    Mkdir(String),
    Move(String, String),
    Copy(String, String),
    Delete(String),
}

/// A path among the names a random folder has, sometimes one it does not.
/// Mostly one part: the names a random folder's entries and their
/// temporary-looking neighbours (`a.tmp`) are about.
#[cfg(unix)]
fn a_path() -> impl Strategy<Value = String> {
    let part = prop_oneof![Just("a"), Just("b"), Just("d"), Just("l"), Just("x"), Just("..")];
    prop_oneof![
        3 => part.clone().prop_map(str::to_string),
        2 => prop::collection::vec(part, 2..4).prop_map(|parts| parts.join("/")),
    ]
}

/// The names a random folder's entries get: half of them where a write
/// of `a` or `b` once put its temporary file (homology#3).
#[cfg(unix)]
fn an_entry_name() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just("a"), Just("b"), Just("l"), Just("x"), Just("a.tmp"), Just("b.tmp"), Just("a.tmp"), Just("b.tmp")]
}

/// Any kind of entry.
#[cfg(unix)]
fn made() -> impl Strategy<Value = Made> {
    prop_oneof![
        Just(Made::Dir),
        "[a-z]{0,5}".prop_map(Made::File),
        Just(Made::LinkIn),
        Just(Made::LinkOutDir),
        Just(Made::LinkOutFile),
        Just(Made::DanglingOut),
        Just(Made::DanglingIn),
        Just(Made::Chain),
    ]
}

/// Any operation on the folder's names.
#[cfg(unix)]
fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (a_path(), "[a-z]{0,6}", any::<bool>()).prop_map(|(path, text, stale)| Op::Write(path, text, stale)),
        a_path().prop_map(Op::Mkdir),
        (a_path(), a_path()).prop_map(|(from, to)| Op::Move(from, to)),
        (a_path(), a_path()).prop_map(|(from, to)| Op::Copy(from, to)),
        a_path().prop_map(Op::Delete),
    ]
}

/// Everything under `dir`, links not followed: each path with its kind and
/// content (a file's bytes, a link's target).
#[cfg(unix)]
fn snapshot(dir: &Path) -> BTreeMap<String, String> {
    let mut seen = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        for entry in std::fs::read_dir(&at).unwrap().flatten() {
            let path = entry.path();
            let name = path.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            let kind = std::fs::symlink_metadata(&path).unwrap().file_type();
            let what = if kind.is_symlink() {
                format!("link {}", std::fs::read_link(&path).unwrap().display())
            } else if kind.is_dir() {
                stack.push(path.clone());
                "dir".to_string()
            } else {
                format!("file {:?}", std::fs::read(&path).unwrap())
            };
            seen.insert(name, what);
        }
    }
    seen
}

/// A random folder made under `root`, the folder `outside` beside it with
/// a file in it.
#[cfg(unix)]
fn lay_out(root: &Path, outside: &Path, entries: &[(bool, &str, Made)]) {
    use std::os::unix::fs::symlink;
    std::fs::create_dir_all(root.join("d")).unwrap();
    std::fs::create_dir_all(outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "outside").unwrap();
    for (in_d, name, made) in entries {
        let at = if *in_d { root.join("d").join(name) } else { root.join(name) };
        if std::fs::symlink_metadata(&at).is_ok() {
            continue;
        }
        let _ = match made {
            Made::Dir => std::fs::create_dir(&at),
            Made::File(text) => std::fs::write(&at, text),
            Made::LinkIn => symlink(root.join("d"), &at),
            Made::LinkOutDir => symlink(outside, &at),
            Made::LinkOutFile => symlink(outside.join("secret.txt"), &at),
            Made::DanglingOut => symlink(outside.join("made-by-runner"), &at),
            Made::DanglingIn => symlink(root.join("nothing-here"), &at),
            Made::Chain => {
                if std::fs::symlink_metadata(root.join("l")).is_err() {
                    let _ = symlink(outside, root.join("l"));
                }
                symlink(root.join("l"), &at)
            }
        };
    }
}

#[cfg(unix)]
proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// Any run of operations on a folder with links in, out and to nothing
    /// changes and makes nothing outside it (temporary files included); a
    /// write is read back as written; a write over another version is
    /// `code_conflict` and leaves the file; a move or copy never writes
    /// over what is there.
    #[test]
    fn file_operations_never_reach_outside_the_folder(
        entries in prop::collection::vec((any::<bool>(), an_entry_name(), made()), 0..6),
        ops in prop::collection::vec(op(), 1..8),
    ) {
        let base = tempfile::tempdir().unwrap();
        let (root, outside) = (base.path().join("root"), base.path().join("outside"));
        lay_out(&root, &outside, &entries.iter().map(|(in_d, name, made)| (*in_d, *name, made.clone())).collect::<Vec<_>>());
        let before = snapshot(&outside);
        let beside = snapshot(base.path()).into_iter().filter(|(name, _)| !name.starts_with("root")).collect::<BTreeMap<_, _>>();
        for op in ops {
            match &op {
                Op::Write(path, text, stale) => {
                    let had = fs::read(&root, path).ok();
                    let over = if *stale { Some("0".repeat(64)) } else { had.as_ref().map(|read| read["sha256"].as_str().unwrap().to_string()) };
                    match fs::write(&root, path, text, over.as_deref()) {
                        Ok(written) => {
                            let read = fs::read(&root, path).unwrap();
                            prop_assert_eq!(read["text"].as_str(), Some(text.as_str()));
                            prop_assert_eq!(&read["sha256"], &written["sha256"]);
                            prop_assert!(!*stale || had.is_none(), "a write over another version went through");
                        }
                        Err(error) if *stale && had.is_some() => {
                            prop_assert_eq!(error.code.as_str(), CODE_CONFLICT);
                            prop_assert_eq!(fs::read(&root, path).ok(), had);
                        }
                        Err(_) => {}
                    }
                }
                Op::Mkdir(path) => { let _ = fs::mkdir(&root, path); }
                Op::Move(from, to) | Op::Copy(from, to) => {
                    let there = std::fs::symlink_metadata(root.join(to)).is_ok() && is_relative(to);
                    let target_before = there.then(|| snapshot_one(&root.join(to)));
                    let result = if matches!(op, Op::Move(..)) { fs::move_path(&root, from, to) } else { fs::copy_path(&root, from, to) };
                    if there {
                        prop_assert!(result.is_err(), "{:?} wrote over what was there", op);
                        prop_assert_eq!(Some(snapshot_one(&root.join(to))), target_before);
                    }
                    if result.is_ok() {
                        prop_assert!(to != from && !to.starts_with(&format!("{from}/")), "{:?} into itself", op);
                    }
                }
                Op::Delete(path) => { let _ = fs::delete(&root, path); }
            }
            prop_assert_eq!(&snapshot(&outside), &before, "after {:?}", op);
            let now = snapshot(base.path()).into_iter().filter(|(name, _)| !name.starts_with("root")).collect::<BTreeMap<_, _>>();
            prop_assert_eq!(&now, &beside, "something was made beside the folder after {:?}", op);
        }
    }
}

/// What is at `path` (links not followed), for comparing before and after.
#[cfg(unix)]
fn snapshot_one(path: &Path) -> String {
    let kind = std::fs::symlink_metadata(path).unwrap().file_type();
    if kind.is_symlink() {
        format!("link {}", std::fs::read_link(path).unwrap().display())
    } else if kind.is_dir() {
        format!("dir {:?}", snapshot(path))
    } else {
        format!("file {:?}", std::fs::read(path).unwrap())
    }
}

/// Any JSON value of a few levels (numbers as integers: the protocol's).
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<String>().prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(any::<String>(), inner, 0..4)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

/// Any request's parameters.
fn json_map() -> impl Strategy<Value = JsonMap> {
    prop::collection::btree_map(any::<String>(), json_value(), 0..4).prop_map(|map| map.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// A request and a message read back from their lines are themselves.
    #[test]
    fn lines_read_back_as_what_was_written(
        id in any::<u64>(), op in ".+", params in json_map(), result in json_value(),
        code in "[a-z_]{1,12}", message in any::<String>(), event in any::<String>(), data in json_value(), ok in any::<bool>(),
    ) {
        let request = Request { id, op, params };
        prop_assert_eq!(Request::parse(&request.to_line()).unwrap(), request);
        let outcome = if ok { Ok(result) } else { Err(RunnerError::new(&code, message)) };
        for sent in [Message::Reply { id, outcome }, Message::Event { event, data }] {
            prop_assert_eq!(Message::parse(&sent.to_line()).unwrap(), sent);
        }
    }

    /// Any line is a request or `invalid_message`, a message or
    /// `runner_invalid_output`: never a panic, never another code.
    #[test]
    fn any_line_is_parsed_or_refused_with_its_code(line in prop_oneof![any::<String>(), json_value().prop_map(|value| value.to_string())]) {
        if let Err((_, error)) = Request::parse(&line) {
            prop_assert_eq!(error.code.as_str(), INVALID_MESSAGE);
        }
        if let Err(error) = Message::parse(&line) {
            prop_assert_eq!(error.code.as_str(), RUNNER_INVALID_OUTPUT);
        }
    }

    /// What programs print is read whatever it is: `nvidia-smi`'s lines,
    /// conda's `environments.txt`, the probe's answer.
    #[test]
    fn what_programs_print_is_read_without_panicking(text in any::<String>()) {
        if let Some(Value::Array(gpus)) = crate::env::gpus_of(&text) {
            prop_assert!(!gpus.is_empty() && gpus.iter().all(|gpu| gpu["name"].is_string()));
        }
        prop_assert!(crate::env::listed_environments(&text).iter().all(|prefix| !prefix.as_os_str().is_empty()));
        let _ = crate::env::answer_of(&text);
    }

    /// Lines are read as `lines()` reads them, however the input arrives,
    /// and one over the limit is dropped and reported, not kept.
    #[test]
    fn lines_are_read_whole_and_bounded(
        lines in prop::collection::vec(("[a-z\r]{0,12}", any::<bool>()), 0..6),
        last_newline in any::<bool>(), max in 0usize..10, capacity in 1usize..8,
    ) {
        let mut input = String::new();
        let mut expected = Vec::new();
        for (index, (text, crlf)) in lines.iter().enumerate() {
            let content = if *crlf { format!("{text}\r") } else { text.clone() };
            input.push_str(&content);
            let last = index + 1 == lines.len();
            if !last || last_newline {
                input.push('\n');
            } else if content.is_empty() {
                break;
            }
            expected.push(if content.len() > max {
                Line::TooLong
            } else {
                Line::Text(content.strip_suffix('\r').unwrap_or(&content).to_string())
            });
        }
        let mut reader = BufReader::with_capacity(capacity, Cursor::new(input.into_bytes()));
        let mut read = Vec::new();
        while let Some(line) = read_line(&mut reader, max).unwrap() {
            read.push(line);
        }
        prop_assert_eq!(read, expected);
    }
}

/// One line of a session's input.
#[derive(Clone, Debug)]
enum Sent {
    Hello(u64, u64),
    Ping(u64),
    Shutdown(u64),
    Unknown(u64),
    List(u64),
    /// An object with an id but no op: answered, the session goes on.
    NoOp(u64),
    /// Not JSON: nothing to answer to, the session ends.
    Garbage,
    Blank,
}

/// Any line of a session's input.
fn sent() -> impl Strategy<Value = Sent> {
    prop_oneof![
        3 => (any::<u64>(), prop_oneof![Just(PROTOCOL), any::<u64>()]).prop_map(|(id, protocol)| Sent::Hello(id, protocol)),
        3 => any::<u64>().prop_map(Sent::Ping),
        1 => any::<u64>().prop_map(Sent::Shutdown),
        2 => any::<u64>().prop_map(Sent::Unknown),
        2 => any::<u64>().prop_map(Sent::List),
        2 => any::<u64>().prop_map(Sent::NoOp),
        1 => Just(Sent::Garbage),
        1 => Just(Sent::Blank),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// A session answers each request with an id exactly once, in order,
    /// writes only lines a caller can read, and nothing after it ended
    /// (shutdown, a first request that is not hello, another protocol, or
    /// a line with nothing to answer to).
    #[test]
    fn a_session_answers_each_request_once_and_stops_when_it_ends(lines in prop::collection::vec(sent(), 0..12)) {
        let mut input = String::new();
        let (mut expected, mut ended, mut greeted) = (Vec::new(), None, false);
        for line in &lines {
            let (text, id, op) = match line {
                Sent::Hello(id, protocol) => (json!({"id": id, "op": "hello", "params": {"protocol": protocol}}).to_string(), Some(*id), Some(("hello", *protocol == PROTOCOL))),
                Sent::Ping(id) => (json!({"id": id, "op": "ping"}).to_string(), Some(*id), Some(("ping", true))),
                Sent::Shutdown(id) => (json!({"id": id, "op": "shutdown"}).to_string(), Some(*id), Some(("shutdown", true))),
                Sent::Unknown(id) => (json!({"id": id, "op": "no.such"}).to_string(), Some(*id), Some(("other", true))),
                Sent::List(id) => (json!({"id": id, "op": "fs.list", "params": {"root": "/no/such/folder"}}).to_string(), Some(*id), Some(("other", true))),
                Sent::NoOp(id) => (json!({"id": id}).to_string(), Some(*id), None),
                Sent::Garbage => ("this is not JSON".to_string(), None, None),
                Sent::Blank => ("   ".to_string(), None, None),
            };
            input.push_str(&text);
            input.push('\n');
            if ended.is_some() {
                continue;
            }
            match (id, op) {
                (None, None) if matches!(line, Sent::Blank) => {}
                (None, _) => ended = Some(false),
                (Some(id), None) => expected.push(id),
                (Some(id), Some((name, fine))) => {
                    expected.push(id);
                    if !greeted && name != "hello" {
                        ended = Some(false);
                    } else if name == "hello" {
                        if fine { greeted = true } else { ended = Some(false) }
                    } else if name == "shutdown" {
                        ended = Some(true);
                    }
                }
            }
        }
        let mut output = Vec::new();
        let outcome = serve(&Runner::PLAIN, Cursor::new(input.into_bytes()), &mut output).unwrap();
        let written = String::from_utf8(output).unwrap();
        let messages: Vec<Message> = written.lines().map(|line| Message::parse(line).unwrap()).collect();
        let answered: Vec<u64> = messages.iter().filter_map(|message| match message {
            Message::Reply { id, .. } => Some(*id),
            Message::Event { .. } => None,
        }).collect();
        prop_assert_eq!(answered, expected);
        prop_assert_eq!(matches!(outcome, Ended::Done), ended.unwrap_or(true));
        if let Some(Message::Reply { outcome: Err(error), .. }) = messages.first() {
            if !greeted && error.code == HELLO_REQUIRED {
                prop_assert_eq!(messages.len(), 1);
            }
        }
    }
}
