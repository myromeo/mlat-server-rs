//! Times the server-side ingest path (inflate, split, parse, hex-decode) on
//! recorded client lines: `ingest_bench lines.txt`.
use mlat_proto::{FrameEncoder, LineDecoder};
use serde::Deserialize;
use std::time::Instant;

#[derive(Deserialize)]
struct Sync<'a> {
    et: f64,
    ot: f64,
    em: &'a str,
    om: &'a str,
}
#[derive(Deserialize)]
struct Mlat<'a> {
    t: f64,
    m: &'a str,
}
#[derive(Deserialize)]
enum Msg<'a> {
    #[serde(rename = "sync")]
    #[serde(borrow)]
    Sync(Sync<'a>),
    #[serde(rename = "mlat")]
    #[serde(borrow)]
    Mlat(Mlat<'a>),
}

fn hex14(s: &str) -> [u8; 14] {
    let mut out = [0u8; 14];
    for (i, b) in s.as_bytes().chunks(2).take(14).enumerate() {
        let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
        out[i] = h(b[0]) << 4 | h(b[1]);
    }
    out
}

fn main() {
    let text = std::fs::read_to_string(std::env::args().nth(1).expect("lines file")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let mut enc = FrameEncoder::new();
    let mut wire = Vec::new();
    for chunk in lines.chunks(20) {
        let body: String = chunk.iter().map(|l| format!("{l}\n")).collect();
        wire.extend(enc.encode(body.as_bytes()).unwrap());
    }
    let mut best = f64::MAX;
    let mut n = 0usize;
    let mut sink = 0f64;
    for _ in 0..3 {
        let t = Instant::now();
        let mut dec = LineDecoder::new(true);
        let mut out = Vec::new();
        dec.push(&wire, &mut out).unwrap();
        n = out.len();
        for l in &out {
            match serde_json::from_str::<Msg>(l) {
                Ok(Msg::Sync(s)) => sink += s.et + s.ot + hex14(s.em)[0] as f64 + hex14(s.om)[0] as f64,
                Ok(Msg::Mlat(m)) => sink += m.t + hex14(m.m)[0] as f64,
                Err(_) => {}
            }
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!("rust:   {n} msgs in {best:.3}s = {:.2} us/msg  ({:.0} msg/s per core)  [{sink:.0}]", best / n as f64 * 1e6, n as f64 / best);
}
