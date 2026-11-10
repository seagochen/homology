//! What the programs a runner starts print, read whatever it is:
//! `nvidia-smi`'s lines, conda's `environments.txt`, the probe's answer.
#![no_main]

use homology_runner::env::{answer_of, gpus_of, listed_environments};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let _ = gpus_of(text);
    let _ = listed_environments(text);
    let _ = answer_of(text);
});
