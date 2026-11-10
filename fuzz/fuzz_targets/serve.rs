//! Any bytes as a session's input: no panic; each line written is a
//! message a caller can read; no more replies than lines; nothing after
//! the session ended (`serve` returns there).
//!
//! Requests are read with the same parser `serve` uses, and every one but
//! `hello`, `ping` and `shutdown` is renamed to an operation no runner has
//! before the session sees it: what the input asks is never done to this
//! computer's files (an `fs.delete` from random bytes would be).
#![no_main]

use homology_runner::protocol::{Message, Request};
use homology_runner::server::{serve, Runner};
use libfuzzer_sys::fuzz_target;

/// `data` with every request that would touch this computer renamed,
/// line by line as `serve` splits it.
fn harmless(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for (index, raw) in data.split(|byte| *byte == b'\n').enumerate() {
        if index > 0 {
            out.push(b'\n');
        }
        let text = raw.strip_suffix(b"\r").unwrap_or(raw);
        match std::str::from_utf8(text).ok().and_then(|text| Request::parse(text).ok()) {
            Some(mut request) if !matches!(request.op.as_str(), "hello" | "ping" | "shutdown") => {
                request.op = "fuzz.not_done".into();
                out.extend_from_slice(request.to_line().as_bytes());
            }
            _ => out.extend_from_slice(raw),
        }
    }
    out
}

fuzz_target!(|data: &[u8]| {
    let input = harmless(data);
    let lines = input.split(|byte| *byte == b'\n').count();
    let mut output = Vec::new();
    if serve(&Runner::PLAIN, input.as_slice(), &mut output).is_err() {
        return; // not UTF-8: the session ends with an I/O error, as it should
    }
    let written = String::from_utf8(output).expect("the runner writes UTF-8");
    let mut replies = 0;
    for line in written.lines() {
        match Message::parse(line) {
            Ok(Message::Reply { .. }) => replies += 1,
            Ok(Message::Event { .. }) => {}
            Err(error) => panic!("an unreadable line {line:?}: {error}"),
        }
    }
    assert!(replies <= lines, "{replies} replies to {lines} lines");
});
