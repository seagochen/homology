//! Exercise shared protocol framing without launching a process or touching a user's files.
#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let mut reader = std::io::Cursor::new(data);
    while let Ok(Some(_)) = homology::framing::content_length(&mut reader, 4096) {}
});
