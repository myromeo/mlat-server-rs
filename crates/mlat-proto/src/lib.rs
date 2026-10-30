//! Wire-protocol primitives of the mlat client <-> server protocol.
//!
//! After the JSON handshake line, both directions are newline-delimited JSON,
//! optionally wrapped in a continuous zlib stream. The zlib stream is cut into
//! frames: a big-endian u16 length, then the deflate bytes of one sync-flushed
//! chunk with the trailing `00 00 ff ff` marker removed.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use std::fmt;

const SYNC_MARKER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// Compression mode negotiated in the handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressMode {
    /// zlib frames in both directions.
    Zlib2,
    /// zlib frames client -> server, plain lines server -> client.
    Zlib,
    /// plain lines in both directions.
    Plain,
}

impl CompressMode {
    pub fn name(self) -> &'static str {
        match self {
            CompressMode::Zlib2 => "zlib2",
            CompressMode::Zlib => "zlib",
            CompressMode::Plain => "none",
        }
    }

    /// The server's choice from the methods a client offers, in the server's
    /// order of preference (mirrors `process_handshake` in jsonclient.py).
    pub fn negotiate<S: AsRef<str>>(offered: &[S]) -> Option<CompressMode> {
        [CompressMode::Zlib2, CompressMode::Zlib, CompressMode::Plain]
            .into_iter()
            .find(|m| offered.iter().any(|o| o.as_ref() == m.name()))
    }

    pub fn from_name(name: &str) -> Option<CompressMode> {
        [CompressMode::Zlib2, CompressMode::Zlib, CompressMode::Plain]
            .into_iter()
            .find(|m| m.name() == name)
    }

    pub fn client_to_server_framed(self) -> bool {
        self != CompressMode::Plain
    }

    pub fn server_to_client_framed(self) -> bool {
        self == CompressMode::Zlib2
    }
}

#[derive(Debug)]
pub enum CodecError {
    Inflate(flate2::DecompressError),
    Deflate(flate2::CompressError),
    /// A frame produced no output or consumed no input (corrupt stream).
    NoProgress,
    /// An encoded frame does not fit the u16 length field.
    FrameTooLarge(usize),
    /// The sync-flush marker was missing from the compressor output.
    MissingSyncMarker,
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Inflate(e) => write!(f, "inflate error: {e}"),
            CodecError::Deflate(e) => write!(f, "deflate error: {e}"),
            CodecError::NoProgress => write!(f, "decompressor made no progress"),
            CodecError::FrameTooLarge(n) => write!(f, "frame of {n} bytes exceeds u16 length"),
            CodecError::MissingSyncMarker => write!(f, "compressor output lacks sync marker"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Turns a byte stream into text lines, undoing the zlib framing when enabled.
pub struct LineDecoder {
    framed: bool,
    z: Decompress,
    buf: Vec<u8>,
    line: Vec<u8>,
}

impl LineDecoder {
    pub fn new(framed: bool) -> Self {
        LineDecoder {
            framed,
            z: Decompress::new(true),
            buf: Vec::new(),
            line: Vec::new(),
        }
    }

    /// Feed received bytes; completed lines (without the newline) are
    /// appended to `out`. A partial trailing line is kept for the next call.
    pub fn push(&mut self, data: &[u8], out: &mut Vec<String>) -> Result<(), CodecError> {
        if !self.framed {
            self.split_lines(data, out);
            return Ok(());
        }

        self.buf.extend_from_slice(data);
        let mut pos = 0;
        loop {
            let (len, frame) = {
                let avail = &self.buf[pos..];
                if avail.len() < 2 {
                    break;
                }
                let len = u16::from_be_bytes([avail[0], avail[1]]) as usize;
                if avail.len() < 2 + len {
                    break;
                }
                (len, [&avail[2..2 + len], &SYNC_MARKER[..]].concat())
            };
            pos += 2 + len;
            let text = self.inflate(&frame)?;
            self.split_lines(&text, out);
        }
        self.buf.drain(..pos);
        Ok(())
    }

    fn split_lines(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        for chunk in bytes.split_inclusive(|&b| b == b'\n') {
            self.line.extend_from_slice(chunk);
            if self.line.last() == Some(&b'\n') {
                self.line.pop();
                out.push(String::from_utf8_lossy(&self.line).into_owned());
                self.line.clear();
            }
        }
    }

    fn inflate(&mut self, mut input: &[u8]) -> Result<Vec<u8>, CodecError> {
        let mut out = Vec::with_capacity(input.len() * 4 + 64);
        loop {
            if out.capacity() - out.len() < 4096 {
                out.reserve(16384);
            }
            let (before_in, before_out) = (self.z.total_in(), self.z.total_out());
            let status = self
                .z
                .decompress_vec(input, &mut out, FlushDecompress::Sync)
                .map_err(CodecError::Inflate)?;
            let consumed = (self.z.total_in() - before_in) as usize;
            let produced = (self.z.total_out() - before_out) as usize;
            input = &input[consumed..];
            if status == Status::StreamEnd {
                break;
            }
            if input.is_empty() && out.len() < out.capacity() {
                break;
            }
            if consumed == 0 && produced == 0 {
                return Err(CodecError::NoProgress);
            }
        }
        if out.is_empty() {
            // the server treats an empty frame as a protocol error
            return Err(CodecError::NoProgress);
        }
        Ok(out)
    }
}

/// Produces length-prefixed zlib frames from text, one sync-flushed chunk each.
pub struct FrameEncoder {
    z: Compress,
}

impl Default for FrameEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameEncoder {
    pub fn new() -> Self {
        FrameEncoder {
            z: Compress::new(Compression::new(1), true),
        }
    }

    /// Compress `lines` (already newline-terminated) into one wire frame.
    pub fn encode(&mut self, lines: &[u8]) -> Result<Vec<u8>, CodecError> {
        let mut out = Vec::with_capacity(lines.len() / 2 + 64);
        out.extend_from_slice(&[0, 0]);
        let mut input = lines;
        loop {
            if out.capacity() - out.len() < 256 {
                out.reserve(4096);
            }
            let before_in = self.z.total_in();
            self.z
                .compress_vec(input, &mut out, FlushCompress::Sync)
                .map_err(CodecError::Deflate)?;
            input = &input[(self.z.total_in() - before_in) as usize..];
            if input.is_empty() && out.len() < out.capacity() {
                break;
            }
        }
        if !out.ends_with(&SYNC_MARKER) {
            return Err(CodecError::MissingSyncMarker);
        }
        out.truncate(out.len() - SYNC_MARKER.len());
        let n = out.len() - 2;
        if n > u16::MAX as usize {
            return Err(CodecError::FrameTooLarge(n));
        }
        out[..2].copy_from_slice(&(n as u16).to_be_bytes());
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lines(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("{{\"mlat\": {{\"t\": {}, \"m\": \"{:028x}\"}}}}", i * 977, i * 7919))
            .collect()
    }

    #[test]
    fn framed_roundtrip_many_frames() {
        let lines = sample_lines(500);
        let mut enc = FrameEncoder::new();
        let mut wire = Vec::new();
        for chunk in lines.chunks(7) {
            let text: String = chunk.iter().map(|l| format!("{l}\n")).collect();
            wire.extend(enc.encode(text.as_bytes()).unwrap());
        }
        let mut dec = LineDecoder::new(true);
        let mut out = Vec::new();
        dec.push(&wire, &mut out).unwrap();
        assert_eq!(out, lines);
    }

    #[test]
    fn framed_decoder_handles_byte_at_a_time() {
        let lines = sample_lines(40);
        let mut enc = FrameEncoder::new();
        let mut wire = Vec::new();
        for l in &lines {
            wire.extend(enc.encode(format!("{l}\n").as_bytes()).unwrap());
        }
        let mut dec = LineDecoder::new(true);
        let mut out = Vec::new();
        for b in &wire {
            dec.push(std::slice::from_ref(b), &mut out).unwrap();
        }
        assert_eq!(out, lines);
    }

    #[test]
    fn large_incompressible_frame() {
        // pseudo-random text so the deflate output approaches the input size
        let mut x: u64 = 0x9e3779b97f4a7c15;
        let mut s = String::new();
        while s.len() < 30_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.push_str(&format!("{x:016x}"));
        }
        let text = format!("{s}\n");
        let mut enc = FrameEncoder::new();
        let wire = enc.encode(text.as_bytes()).unwrap();
        let mut dec = LineDecoder::new(true);
        let mut out = Vec::new();
        dec.push(&wire, &mut out).unwrap();
        assert_eq!(out, vec![s]);
    }

    #[test]
    fn plain_lines_with_partials() {
        let mut dec = LineDecoder::new(false);
        let mut out = Vec::new();
        dec.push(b"{\"a\":1}\n{\"b\"", &mut out).unwrap();
        dec.push(b":2}\n", &mut out).unwrap();
        assert_eq!(out, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn negotiation_matches_server_order() {
        assert_eq!(CompressMode::negotiate(&["none", "zlib"]), Some(CompressMode::Zlib));
        assert_eq!(CompressMode::negotiate(&["zlib", "zlib2"]), Some(CompressMode::Zlib2));
        assert_eq!(CompressMode::negotiate(&["lz4"]), None);
    }
}
