//! Any line a caller might send: a request, or `invalid_message`; a
//! request reads back from its own line as itself.
#![no_main]

use homology_runner::protocol::{Request, INVALID_MESSAGE};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|line: &str| {
    match Request::parse(line) {
        Ok(request) => assert_eq!(Request::parse(&request.to_line()).ok(), Some(request)),
        Err((_, error)) => assert_eq!(error.code, INVALID_MESSAGE),
    }
});
