//! Loads the client -> server events of a capture (`mlat-capture extract`
//! output) that the multilateration pipeline consumes.

use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader};

pub enum Ev {
    Connect { conn: u64, user: String, lat: f64, lon: f64, alt: f64, clock: String },
    Close { conn: u64 },
    Sync { conn: u64, et: f64, ot: f64, em: [u8; 14], om: [u8; 14] },
    Mlat { conn: u64, ts: f64, msg: Vec<u8> },
    Seen { conn: u64, icaos: Vec<u32> },
    Lost { conn: u64, icaos: Vec<u32> },
    RateReport { conn: u64 },
    ClockReset { conn: u64 },
}

#[derive(Deserialize)]
struct Line {
    ev: String,
    #[serde(default)]
    c: u64,
    #[serde(default)]
    t: f64,
    #[serde(default)]
    l: String,
    #[serde(default)]
    line: String,
    #[serde(default)]
    start_epoch: f64,
}

fn hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}

fn hex14(s: &str) -> Option<[u8; 14]> {
    hex(s)?.try_into().ok()
}

fn icaos(v: &Value) -> Vec<u32> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(|s| u32::from_str_radix(s, 16).ok()).collect())
        .unwrap_or_default()
}

/// (capture start epoch, events as (seconds since start, event)), in capture order.
pub fn load(path: &str, max_secs: Option<f64>) -> (f64, Vec<(f64, Ev)>) {
    let f = BufReader::with_capacity(1 << 20, std::fs::File::open(path).expect("capture file"));
    let mut start = 0.0;
    let mut out = Vec::new();
    for line in f.lines() {
        let line = line.unwrap();
        if !(line.starts_with("{\"ev\":\"c2s\"")
            || line.starts_with("{\"ev\":\"hs\"")
            || line.starts_with("{\"ev\":\"close\"")
            || line.starts_with("{\"ev\":\"meta\""))
        {
            continue;
        }
        let e: Line = serde_json::from_str(&line).unwrap();
        if e.ev == "meta" {
            start = e.start_epoch;
            continue;
        }
        if max_secs.is_some_and(|m| e.t > m) {
            if e.ev == "c2s" {
                break;
            }
            continue;
        }
        match e.ev.as_str() {
            "hs" => {
                let hs: Value = serde_json::from_str(&e.line).unwrap();
                let mut lon = hs["lon"].as_f64().unwrap_or(0.0);
                if lon > 180.0 {
                    lon -= 180.0;
                }
                out.push((
                    e.t,
                    Ev::Connect {
                        conn: e.c,
                        user: hs["user"].as_str().unwrap_or("").to_string(),
                        lat: hs["lat"].as_f64().unwrap_or(0.0),
                        lon,
                        alt: hs["alt"].as_f64().unwrap_or(0.0),
                        clock: hs["clock_type"].as_str().unwrap_or("dump1090").to_string(),
                    },
                ));
            }
            "close" => out.push((e.t, Ev::Close { conn: e.c })),
            "c2s" => {
                let head = &e.l[..e.l.len().min(16)];
                let kinds = ["\"sync\"", "\"mlat\"", "\"seen\"", "\"lost\"", "\"input_", "\"clock_", "\"rate_"];
                if !kinds.iter().any(|k| head.contains(k)) {
                    continue;
                }
                let Ok(m) = serde_json::from_str::<Value>(&e.l) else { continue };
                if let Some(s) = m.get("sync") {
                    let (Some(em), Some(om)) = (s["em"].as_str().and_then(hex14), s["om"].as_str().and_then(hex14)) else { continue };
                    out.push((e.t, Ev::Sync { conn: e.c, et: s["et"].as_f64().unwrap_or(0.0), ot: s["ot"].as_f64().unwrap_or(0.0), em, om }));
                } else if let Some(s) = m.get("mlat") {
                    let Some(msg) = s["m"].as_str().and_then(hex) else { continue };
                    out.push((e.t, Ev::Mlat { conn: e.c, ts: s["t"].as_f64().unwrap_or(0.0), msg }));
                } else if let Some(s) = m.get("seen") {
                    out.push((e.t, Ev::Seen { conn: e.c, icaos: icaos(s) }));
                } else if let Some(s) = m.get("lost") {
                    out.push((e.t, Ev::Lost { conn: e.c, icaos: icaos(s) }));
                } else if m.get("rate_report").is_some() {
                    out.push((e.t, Ev::RateReport { conn: e.c }));
                } else if m.get("input_connected").is_some()
                    || m.get("input_disconnected").is_some()
                    || m.get("clock_reset").is_some()
                    || m.get("clock_jump").is_some()
                {
                    out.push((e.t, Ev::ClockReset { conn: e.c }));
                }
            }
            _ => {}
        }
    }
    (start, out)
}
