//! Cross-check helper: `xcheck decode` reads wire frames on stdin and prints
//! the lines; `xcheck encode` reads lines on stdin and writes one frame per
//! line to stdout. Used to verify compatibility with Python's zlib.
use mlat_proto::{FrameEncoder, LineDecoder};
use std::io::{Read, Write};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    let mut out = std::io::stdout().lock();
    match mode.as_str() {
        "decode" => {
            let mut dec = LineDecoder::new(true);
            let mut lines = Vec::new();
            dec.push(&input, &mut lines).unwrap();
            for l in lines {
                writeln!(out, "{l}").unwrap();
            }
        }
        "encode" => {
            let mut enc = FrameEncoder::new();
            for line in String::from_utf8(input).unwrap().lines() {
                out.write_all(&enc.encode(format!("{line}\n").as_bytes()).unwrap()).unwrap();
            }
        }
        _ => eprintln!("usage: xcheck decode|encode"),
    }
}
