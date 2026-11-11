//! Framed wire protocol for `herdr-core` (`hc connect` after `hello {"framed": true}`).
//! Frame header: `[channel u32 LE][kind u8][len u32 LE][payload]`.

use std::io::{self, Read};

pub const HEADER: usize = 9;
pub const MAX_PAYLOAD: usize = 16 << 20;

/// Payload bytes: JSON-RPC on channel 0, terminal bytes on stream channels.
pub const DATA: u8 = 0;
/// u32 LE: receiver consumed this many bytes; sender may send more.
pub const CREDIT: u8 = 1;
/// Channel closed (JSON reason from core, empty from client).
pub const CLOSE: u8 = 2;
/// u16 LE cols, u16 LE rows: viewer -> core, core -> observer.
pub const RESIZE: u8 = 3;
/// Viewer -> core: 1 = gained focus, 0 = lost focus.
pub const FOCUS: u8 = 4;
/// Viewer -> core: JSON display facts.
#[allow(dead_code)]
pub const FACTS: u8 = 5;
/// Viewer -> core: terminal-generated replies.
#[allow(dead_code)]
pub const AUTOMATIC_INPUT: u8 = 6;

pub fn encode_header(channel: u32, kind: u8, len: usize) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[0..4].copy_from_slice(&channel.to_le_bytes());
    h[4] = kind;
    h[5..9].copy_from_slice(&(len as u32).to_le_bytes());
    h
}

#[allow(dead_code)]
pub fn encode(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(&encode_header(channel, kind, payload.len()));
    out.extend_from_slice(payload);
    out
}

pub fn decode_header(h: &[u8; HEADER]) -> io::Result<(u32, u8, usize)> {
    let channel = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    let kind = h[4];
    let len = u32::from_le_bytes([h[5], h[6], h[7], h[8]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds {MAX_PAYLOAD}"),
        ));
    }
    Ok((channel, kind, len))
}

pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u32, u8, Vec<u8>)>> {
    let mut header = [0u8; HEADER];
    match r.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let (channel, kind, len) = decode_header(&header)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((channel, kind, payload)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trip() {
        let frame = encode(7, DATA, b"hello world");
        let mut cursor = Cursor::new(frame);
        let parsed = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(parsed, (7, DATA, b"hello world".to_vec()));
        assert_eq!(read_frame(&mut cursor).unwrap(), None);
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut header = [0u8; HEADER];
        header[0..4].copy_from_slice(&1u32.to_le_bytes());
        header[4] = DATA;
        header[5..9].copy_from_slice(&((MAX_PAYLOAD as u32) + 1).to_le_bytes());
        assert!(decode_header(&header).is_err());
    }
}
