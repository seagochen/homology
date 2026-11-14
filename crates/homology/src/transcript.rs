//! What a process printed, kept as text to its last bytes and read back by
//! absolute offset: shared by [`crate::console`] (line by line, through
//! pipes) and [`crate::terminal`] (a PTY), so both keep and decode output the
//! same way.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock `mutex`, going on with what a panicked holder left: the shared state
/// here (transcripts, registries) stays usable after a reader thread panics.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Output kept as whole UTF-8 characters, at most `keep` bytes of it; offsets
/// count from the very first byte, so a reader goes on where it stopped even
/// after the start was dropped.
pub(crate) struct Transcript {
    /// The text kept: the end of what was printed.
    pub(crate) text: String,
    /// Bytes dropped from the start.
    pub(crate) dropped: u64,
    /// The most bytes kept.
    keep: usize,
}

impl Transcript {
    /// An empty transcript keeping at most `keep` bytes.
    pub(crate) fn new(keep: usize) -> Self {
        Self { text: String::new(), dropped: 0, keep }
    }

    /// Add `text`, dropping the oldest beyond `keep` at a character boundary.
    pub(crate) fn push(&mut self, text: &str) {
        self.text.push_str(text);
        if self.text.len() > self.keep {
            let mut cut = self.text.len() - self.keep;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut as u64;
        }
    }

    /// What came after `offset` (from the next whole character), and the
    /// offset after it.
    pub(crate) fn since(&self, offset: u64) -> (String, u64) {
        let end = self.dropped + self.text.len() as u64;
        let from = offset.clamp(self.dropped, end) - self.dropped;
        let mut from = usize::try_from(from).unwrap_or(0);
        while from < self.text.len() && !self.text.is_char_boundary(from) {
            from += 1;
        }
        (self.text[from..].to_string(), end)
    }
}

/// Take the text at the start of `pending`: every whole character, an invalid
/// sequence as U+FFFD (so one bad byte never holds back what follows), and
/// leave a character split across reads for the next one.
pub(crate) fn take_text(pending: &mut Vec<u8>) -> String {
    let mut text = String::new();
    loop {
        let (valid, invalid) = match std::str::from_utf8(pending) {
            Ok(_) => (pending.len(), None),
            Err(error) => (error.valid_up_to(), error.error_len()),
        };
        text.push_str(&String::from_utf8_lossy(&pending[..valid]));
        match invalid {
            Some(length) => {
                text.push(char::REPLACEMENT_CHARACTER);
                pending.drain(..valid + length);
            }
            None => {
                pending.drain(..valid);
                return text;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_keeps_its_end_and_reads_on_by_offset() {
        let mut transcript = Transcript::new(8);
        transcript.push("abc");
        let (text, offset) = transcript.since(0);
        assert_eq!((text.as_str(), offset), ("abc", 3));
        transcript.push("字字字");
        assert!(transcript.text.len() <= 8 && transcript.dropped > 0);
        // An offset inside a character starts at the next whole one.
        let (rest, end) = transcript.since(transcript.dropped + 1);
        assert!(rest.chars().all(|character| character == '字'));
        assert_eq!(end, 12);
    }

    #[test]
    fn decoding_keeps_split_characters_and_replaces_invalid_bytes() {
        let mut pending = "中".as_bytes()[..2].to_vec();
        assert_eq!(take_text(&mut pending), "");
        pending.extend_from_slice(&"中".as_bytes()[2..]);
        assert_eq!(take_text(&mut pending), "中");
        assert!(pending.is_empty());
        let mut pending = vec![b'a', 0xff, b'b', 0xe4];
        assert_eq!(take_text(&mut pending), "a\u{fffd}b");
        assert_eq!(pending, [0xe4]);
    }
}
