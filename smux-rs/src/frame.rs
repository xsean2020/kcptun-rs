//! SMUX frame types and encoding/decoding.
//!
//! Wire format (matching Go xtaci/smux):
//!   ver(1) | cmd(1) | length(2) LE | stream_id(4) LE | data

use std::fmt;

use bytes::{BufMut, Bytes, BytesMut};

/// SMUX protocol version (matching Go smux v2 default).
pub(crate) const SMUX_VER: u8 = 2;

/// Frame header size in bytes (ver|cmd|length|sid = 1+1+2+4 = 8).
pub const FRAME_HEADER_SIZE: usize = 8;

/// Maximum frame payload size.
pub const MAX_FRAME_SIZE: usize = 60000;

/// SMUX command codes matching Go xtaci/smux.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Stream open.         (Go cmdSYN = 0)
    Syn = 0,
    /// Stream close / EOF.  (Go cmdFIN = 1)
    Fin = 1,
    /// Data push.           (Go cmdPSH = 2)
    Psh = 2,
    /// No operation (keepalive). (Go cmdNOP = 3)
    Nop = 3,
    /// Window update (v2).  (Go cmdUPD = 4)
    Upd = 4,
}

impl Cmd {
    /// Convert a u8 to a Cmd variant.
    #[inline]
    pub fn from_u8(v: u8) -> Option<Cmd> {
        match v {
            0 => Some(Cmd::Syn),
            1 => Some(Cmd::Fin),
            2 => Some(Cmd::Psh),
            3 => Some(Cmd::Nop),
            4 => Some(Cmd::Upd),
            _ => None,
        }
    }
}

/// A SMUX frame on the wire.
#[derive(Clone)]
pub struct Frame {
    /// Protocol version.
    pub ver: u8,
    /// Command.
    pub cmd: Cmd,
    /// Payload length.
    pub length: u32,
    /// Stream ID.
    pub stream_id: u32,
    /// Payload data.
    pub data: Bytes,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("ver", &self.ver)
            .field("cmd", &self.cmd)
            .field("length", &self.length)
            .field("stream_id", &self.stream_id)
            .field("data.len", &self.data.len())
            .finish()
    }
}

impl Frame {
    /// Create a new frame.
    #[inline]
    pub fn new(cmd: Cmd, stream_id: u32, data: Bytes) -> Self {
        Frame {
            ver: SMUX_VER,
            cmd,
            length: data.len() as u32,
            stream_id,
            data,
        }
    }

    /// Set the protocol version for this frame.
    /// Go smux validates: hdr.Version() != config.Version → reject.
    pub fn with_ver(mut self, ver: u8) -> Self {
        self.ver = ver;
        self
    }

    /// Encode only the 8-byte SMUX header into `buf`.
    ///
    /// Use when payload is already (or will be) written after the header so
    /// callers can avoid allocating a temporary `Frame` + `Bytes` clone.
    #[inline]
    pub fn encode_header_into<B: BufMut>(
        buf: &mut B,
        ver: u8,
        cmd: Cmd,
        stream_id: u32,
        length: u16,
    ) {
        buf.put_u8(ver);
        buf.put_u8(cmd as u8);
        buf.put_u16_le(length);
        buf.put_u32_le(stream_id);
    }

    /// Patch the length field of a header previously written at `header_pos`
    /// (bytes `[header_pos+2 .. header_pos+4)`).
    #[inline]
    pub fn patch_header_length(buf: &mut BytesMut, header_pos: usize, length: u16) {
        buf[header_pos + 2..header_pos + 4].copy_from_slice(&length.to_le_bytes());
    }

    /// Encode this frame — Go smux wire format:
    /// ver(1) | cmd(1) | length(2) LE | stream_id(4) LE | data
    pub fn encode<B: BufMut>(&self, buf: &mut B) -> usize {
        Self::encode_header_into(buf, self.ver, self.cmd, self.stream_id, self.length as u16);
        if !self.data.is_empty() {
            buf.put_slice(&self.data);
        }
        FRAME_HEADER_SIZE + self.data.len()
    }

    /// Try to decode a frame — Go smux wire format:
    /// ver(1) | cmd(1) | length(2) LE | stream_id(4) LE | data
    pub fn decode(data: &[u8]) -> Option<(Frame, usize)> {
        if data.len() < FRAME_HEADER_SIZE {
            return None;
        }

        let ver = data[0];
        let cmd_byte = data[1];
        let length = u16::from_le_bytes([data[2], data[3]]) as u32;
        let stream_id = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);

        let total_len = FRAME_HEADER_SIZE + length as usize;
        if data.len() < total_len {
            return None;
        }

        let cmd = Cmd::from_u8(cmd_byte)?;

        let payload = if length > 0 {
            Bytes::copy_from_slice(&data[FRAME_HEADER_SIZE..total_len])
        } else {
            Bytes::new()
        };

        Some((
            Frame {
                ver,
                cmd,
                length,
                stream_id,
                data: payload,
            },
            total_len,
        ))
    }
}

/// A codec for reading/writing SMUX frames from a byte stream.
pub struct FrameCodec {
    buf: BytesMut,
}

impl FrameCodec {
    /// Create a new FrameCodec.
    #[inline]
    pub fn new(capacity: usize) -> Self {
        FrameCodec {
            buf: BytesMut::with_capacity(capacity),
        }
    }

    /// Feed incoming bytes into the codec.
    #[inline]
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Try to decode a frame from the buffered data.
    ///
    /// Uses `split_to + slice` for zero-copy payload extraction — the
    /// returned `Frame.data` is a reference-counted slice of the codec
    /// buffer, not a copy.
    pub fn decode(&mut self) -> Option<Frame> {
        if self.buf.len() < FRAME_HEADER_SIZE {
            return None;
        }

        // Peek at the length field to determine total frame size
        let length = u16::from_le_bytes([self.buf[2], self.buf[3]]) as usize;
        let total_len = FRAME_HEADER_SIZE + length;

        if self.buf.len() < total_len {
            return None;
        }

        // Extract frame bytes (zero-copy: split_to advances the buffer)
        let frame_bytes = self.buf.split_to(total_len).freeze();

        // Parse header fields
        let ver = frame_bytes[0];
        let cmd_byte = frame_bytes[1];
        let stream_id = u32::from_le_bytes([
            frame_bytes[4],
            frame_bytes[5],
            frame_bytes[6],
            frame_bytes[7],
        ]);
        let cmd = Cmd::from_u8(cmd_byte)?;

        // Slice payload (zero-copy: reference-counted view into frame_bytes)
        let payload = if length > 0 {
            frame_bytes.slice(FRAME_HEADER_SIZE..)
        } else {
            Bytes::new()
        };

        Some(Frame {
            ver,
            cmd,
            length: length as u32,
            stream_id,
            data: payload,
        })
    }

    /// Encode a frame and return the bytes.
    pub fn encode(frame: &Frame) -> BytesMut {
        let mut buf = BytesMut::with_capacity(FRAME_HEADER_SIZE + frame.data.len());
        frame.encode(&mut buf);
        buf
    }

    /// Get remaining buffer length.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns `true` if the buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Clear the buffer.
    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// Errors related to SMUX frame processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Frame too short.
    Truncated,
    /// Unknown command.
    UnknownCommand(u8),
    /// Invalid protocol version.
    InvalidVersion(u8),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Truncated => write!(f, "frame too short"),
            FrameError::UnknownCommand(c) => write!(f, "unknown command: {:#04x}", c),
            FrameError::InvalidVersion(v) => write!(f, "invalid version: {}", v),
        }
    }
}

impl std::error::Error for FrameError {}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_decode_roundtrip(cmd: Cmd, sid: u32, data: &[u8]) {
        let frame = Frame::new(cmd, sid, Bytes::copy_from_slice(data));
        let mut buf = Vec::new();
        frame.encode(&mut buf);
        let (decoded, consumed) = Frame::decode(&buf).unwrap();
        assert_eq!(consumed, FRAME_HEADER_SIZE + data.len());
        assert_eq!(decoded.ver, SMUX_VER);
        assert_eq!(decoded.cmd, cmd);
        assert_eq!(decoded.stream_id, sid);
        assert_eq!(&decoded.data[..], data);
    }

    #[test]
    fn encode_header_into_matches_frame_encode() {
        let data = b"hello";
        let mut via_frame = Vec::new();
        Frame::new(Cmd::Psh, 42, Bytes::copy_from_slice(data)).encode(&mut via_frame);

        let mut via_header = BytesMut::new();
        let pos = via_header.len();
        Frame::encode_header_into(&mut via_header, SMUX_VER, Cmd::Psh, 42, 0);
        via_header.extend_from_slice(data);
        Frame::patch_header_length(&mut via_header, pos, data.len() as u16);

        assert_eq!(&via_frame[..], &via_header[..]);
    }

    #[test]
    fn frame_syn() {
        encode_decode_roundtrip(Cmd::Syn, 1, &[]);
    }

    #[test]
    fn frame_fin() {
        encode_decode_roundtrip(Cmd::Fin, 1, &[]);
    }

    #[test]
    fn frame_psh() {
        encode_decode_roundtrip(Cmd::Psh, 1, b"hello");
    }

    #[test]
    fn frame_nop() {
        encode_decode_roundtrip(Cmd::Nop, 0, &[]);
    }

    #[test]
    fn frame_upd() {
        let mut upd = vec![0u8; 8];
        // consumed=100, window=65536
        upd[..4].copy_from_slice(&100u32.to_le_bytes());
        upd[4..8].copy_from_slice(&65536u32.to_le_bytes());
        encode_decode_roundtrip(Cmd::Upd, 1, &upd);
    }

    #[test]
    fn frame_decode_truncated() {
        assert!(Frame::decode(&[0u8; 5]).is_none());
    }

    #[test]
    fn frame_empty_payload() {
        let frame = Frame::new(Cmd::Syn, 42, Bytes::new());
        let mut buf = Vec::new();
        frame.encode(&mut buf);

        let (decoded, consumed) = Frame::decode(&buf).unwrap();
        assert_eq!(consumed, FRAME_HEADER_SIZE);
        assert_eq!(decoded.cmd, Cmd::Syn);
        assert_eq!(decoded.stream_id, 42);
        assert!(decoded.data.is_empty());
    }

    #[test]
    fn frame_cmd_conversion() {
        assert_eq!(Cmd::from_u8(0), Some(Cmd::Syn));
        assert_eq!(Cmd::from_u8(1), Some(Cmd::Fin));
        assert_eq!(Cmd::from_u8(2), Some(Cmd::Psh));
        assert_eq!(Cmd::from_u8(3), Some(Cmd::Nop));
        assert_eq!(Cmd::from_u8(4), Some(Cmd::Upd));
        assert_eq!(Cmd::from_u8(5), None);
        assert_eq!(Cmd::from_u8(0xFF), None);
    }

    #[test]
    fn frame_codec_buffer() {
        let mut codec = FrameCodec::new(1024);
        let frame = Frame::new(Cmd::Psh, 1, Bytes::from("test"));
        let bytes = FrameCodec::encode(&frame);
        codec.feed(&bytes);
        let decoded = codec.decode().unwrap();
        assert_eq!(decoded.cmd, Cmd::Psh);
        assert_eq!(decoded.stream_id, 1);
    }

    #[test]
    fn frame_codec_partial_feed() {
        let mut codec = FrameCodec::new(1024);
        let frame = Frame::new(Cmd::Psh, 5, Bytes::from("hello world"));
        let bytes = FrameCodec::encode(&frame);
        codec.feed(&bytes[..6]);
        assert!(codec.decode().is_none());
        codec.feed(&bytes[6..]);
        let decoded = codec.decode().unwrap();
        assert_eq!(decoded.stream_id, 5);
    }

    #[test]
    fn frame_max_size() {
        let data = vec![0u8; 65535];
        let frame = Frame::new(Cmd::Psh, 1, Bytes::from(data));
        let mut buf = Vec::new();
        frame.encode(&mut buf);
        let (decoded, _) = Frame::decode(&buf).unwrap();
        assert_eq!(decoded.length, 65535);
        assert_eq!(decoded.data.len(), 65535);
    }

    /// Verify Go-compatible wire format:
    /// [ver][cmd][len_lo][len_hi][sid_0][sid_1][sid_2][sid_3]
    #[test]
    fn frame_wire_format_matches_go() {
        let frame = Frame::new(Cmd::Psh, 0x12345678, Bytes::from("ab"));
        let mut buf = Vec::new();
        frame.encode(&mut buf);
        assert_eq!(buf.len(), 10);
        assert_eq!(buf[0], 2); // ver
        assert_eq!(buf[1], 2); // cmd=PSH
        assert_eq!(buf[2], 2); // length low byte
        assert_eq!(buf[3], 0); // length high byte
        assert_eq!(buf[4], 0x78); // sid byte 0
        assert_eq!(buf[5], 0x56); // sid byte 1
        assert_eq!(buf[6], 0x34); // sid byte 2
        assert_eq!(buf[7], 0x12); // sid byte 3
        assert_eq!(&buf[8..], b"ab"); // data
    }
}
