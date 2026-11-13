//! Shared byte framing for language and debugging protocols, independent of JSON and processes.

use std::io::{self, BufRead, Read};

/// Read one Content-Length payload, enforcing limits before allocating its body.
/// Empty lines between messages are accepted. Clean EOF returns None; partial messages fail.
/// Callers choose their payload limit and decide how malformed JSON inside a frame is handled.
pub fn content_length(reader: &mut impl BufRead, maximum: usize) -> io::Result<Option<Vec<u8>>> {
    let mut length = None;
    let mut headers = 0;
    let mut started = false;
    loop {
        let mut line = Vec::new();
        let count = reader.by_ref().take((8193 - headers) as u64).read_until(b'\n', &mut line)?;
        if count == 0 {
            return if started {
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Incomplete protocol headers"))
            } else {
                Ok(None)
            };
        }
        headers += count;
        if headers > 8192 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Protocol headers exceed 8 KiB"));
        }
        let line = std::str::from_utf8(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if line.trim().is_empty() {
            if length.is_some() {
                break;
            }
            if started {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Missing Content-Length"));
            }
            continue;
        }
        started = true;
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid protocol header"));
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "Duplicate Content-Length"));
            }
            length =
                Some(value.trim().parse::<usize>().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?);
        }
    }
    let length = length.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Missing Content-Length"))?;
    if length > maximum {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Protocol frame exceeds its limit"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_keep_unicode_bytes_and_protocol_boundaries() {
        let input = b"\r\nContent-Type: application/json\r\ncontent-length: 6\r\n\r\n\xe4\xb8\xad\xe6\x96\x87Content-Length: 0\r\n\r\n";
        let mut reader = io::BufReader::with_capacity(1, &input[..]);
        assert_eq!(content_length(&mut reader, 6).unwrap(), Some("中文".as_bytes().to_vec()));
        assert_eq!(content_length(&mut reader, 6).unwrap(), Some(Vec::new()));
        assert_eq!(content_length(&mut reader, 6).unwrap(), None);
    }
    #[test]
    fn invalid_and_truncated_frames_are_rejected_before_unbounded_reads() {
        for input in [
            b"Content-Length: 7\r\n\r\n".as_slice(),
            b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            b"Content-Length: 2\r\n\r\n{",
            b"Content-Length: 2\r\n",
            b"Other: value\r\n\r\n",
        ] {
            assert!(content_length(&mut io::Cursor::new(input), 6).is_err());
        }
        let input = vec![b'x'; 32768];
        let mut reader = io::Cursor::new(input);
        assert!(content_length(&mut reader, 6).is_err());
        assert!(reader.position() <= 8193);
    }
}
