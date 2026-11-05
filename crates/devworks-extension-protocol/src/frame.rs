//! The local process transport (protocol version 1): every message is a four-byte unsigned
//! little-endian length followed by exactly that many bytes of one UTF-8 JSON object, at most
//! [`MAX_FRAME_BYTES`]. A zero or oversized length is refused before any payload storage is
//! allocated; a truncated frame is an error, never a partial message. JSON interpretation
//! (duplicate members, batches, envelope shape) belongs to the reader of the payload.
use std::io::{self, Read, Write};

/// Largest control payload of protocol version 1 (1 MiB).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    /// The stream ended cleanly before a new frame began.
    Closed,
    /// A zero-length frame.
    Empty,
    /// A declared length above the limit (nothing of it was read).
    TooLarge(usize),
    /// The stream ended inside a frame.
    Truncated,
    /// The payload is not UTF-8.
    NotUtf8,
    Io(io::Error),
}
impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("stream closed"),
            Self::Empty => f.write_str("zero-length frame"),
            Self::TooLarge(n) => write!(f, "frame of {n} bytes exceeds {MAX_FRAME_BYTES}"),
            Self::Truncated => f.write_str("truncated frame"),
            Self::NotUtf8 => f.write_str("frame is not UTF-8"),
            Self::Io(e) => write!(f, "transport: {e}"),
        }
    }
}

fn fill(r: &mut impl Read, buf: &mut [u8]) -> Result<usize, FrameError> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    Ok(got)
}

/// Reads one frame of at most `max` bytes (itself at most [`MAX_FRAME_BYTES`]).
pub fn read_frame(r: &mut impl Read, max: usize) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; 4];
    match fill(r, &mut header)? {
        0 => return Err(FrameError::Closed),
        4 => {}
        _ => return Err(FrameError::Truncated),
    }
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > max.min(MAX_FRAME_BYTES) {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    if fill(r, &mut payload)? != len {
        return Err(FrameError::Truncated);
    }
    if std::str::from_utf8(&payload).is_err() {
        return Err(FrameError::NotUtf8);
    }
    Ok(payload)
}

/// Writes one frame and flushes it.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> Result<(), FrameError> {
    if payload.is_empty() {
        return Err(FrameError::Empty);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(payload.len()));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())
        .and_then(|_| w.write_all(payload))
        .and_then(|_| w.flush())
        .map_err(FrameError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_bounds_are_checked_before_reading() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{\"a\":1}").unwrap();
        write_frame(&mut buf, b"{}").unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r, MAX_FRAME_BYTES).unwrap(), b"{\"a\":1}");
        assert_eq!(read_frame(&mut r, MAX_FRAME_BYTES).unwrap(), b"{}");
        assert!(matches!(
            read_frame(&mut r, MAX_FRAME_BYTES),
            Err(FrameError::Closed)
        ));

        // An oversized header is refused without the payload being present at all.
        let huge = ((MAX_FRAME_BYTES + 1) as u32).to_le_bytes();
        assert!(matches!(
            read_frame(&mut &huge[..], MAX_FRAME_BYTES),
            Err(FrameError::TooLarge(_))
        ));
        assert!(matches!(
            read_frame(&mut &[0u8, 0, 0, 0][..], MAX_FRAME_BYTES),
            Err(FrameError::Empty)
        ));
        assert!(matches!(
            read_frame(&mut &[5u8, 0, 0, 0, b'{'][..], MAX_FRAME_BYTES),
            Err(FrameError::Truncated)
        ));
        assert!(matches!(
            read_frame(&mut &[2u8, 0][..], MAX_FRAME_BYTES),
            Err(FrameError::Truncated)
        ));
        assert!(matches!(
            read_frame(&mut &[2u8, 0, 0, 0, 0xff, 0xfe][..], MAX_FRAME_BYTES),
            Err(FrameError::NotUtf8)
        ));
        // A tighter negotiated limit applies.
        assert!(matches!(
            read_frame(&mut &buf[..], 3),
            Err(FrameError::TooLarge(7))
        ));
        assert!(write_frame(&mut Vec::new(), &[]).is_err());
    }
}
