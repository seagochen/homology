//! Any line a runner might write: a message, or `runner_invalid_output`;
//! a message reads back from its own line as itself.
#![no_main]

use homology_runner::protocol::{Message, RUNNER_INVALID_OUTPUT};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|line: &str| {
    match Message::parse(line) {
        Ok(message) => assert_eq!(Message::parse(&message.to_line()).ok(), Some(message)),
        Err(error) => assert_eq!(error.code, RUNNER_INVALID_OUTPUT),
    }
});
