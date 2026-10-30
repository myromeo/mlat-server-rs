//! Feeds a capture through the Rust sync engine and compares the resulting
//! clock pairings with the golden output of testbed/py_sync_ref.py.
//!
//!   syncbench capture.jsonl [--golden golden.jsonl] [--threads N] [--max-secs S]

use mlat_core::engine::{Engine, PairRow};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::time::Instant;

enum Ev {
    Connect { conn: u64, user: String, lat: f64, lon: f64, alt: f64, clock: String },
    Close { conn: u64 },
    Sync { conn: u64, et: f64, ot: f64, em: [u8; 14], om: [u8; 14] },
    Seen { conn: u64, icaos: Vec<u32> },
    Lost { conn: u64, icaos: Vec<u32> },
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

fn hex14(s: &str) -> Option<[u8; 14]> {
    if s.len() != 28 {
        return None;
    }
    let mut out = [0u8; 14];
    for i in 0..14 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn icaos(v: &Value) -> Vec<u32> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(|s| u32::from_str_radix(s, 16).ok()).collect())
        .unwrap_or_default()
}

fn load(path: &str, max_secs: Option<f64>) -> (f64, Vec<(f64, Ev)>) {
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
                let kinds = ["\"sync\"", "\"seen\"", "\"lost\"", "\"input_", "\"clock_"];
                if !kinds.iter().any(|k| head.contains(k)) {
                    continue;
                }
                let m: Value = match serde_json::from_str(&e.l) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(s) = m.get("sync") {
                    let (Some(em), Some(om)) =
                        (s["em"].as_str().and_then(hex14), s["om"].as_str().and_then(hex14))
                    else {
                        continue;
                    };
                    out.push((
                        e.t,
                        Ev::Sync { conn: e.c, et: s["et"].as_f64().unwrap_or(0.0), ot: s["ot"].as_f64().unwrap_or(0.0), em, om },
                    ));
                } else if let Some(s) = m.get("seen") {
                    out.push((e.t, Ev::Seen { conn: e.c, icaos: icaos(s) }));
                } else if let Some(s) = m.get("lost") {
                    out.push((e.t, Ev::Lost { conn: e.c, icaos: icaos(s) }));
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

#[derive(Deserialize)]
struct GoldenSnap {
    t: f64,
    pairs: Vec<(u32, u32, i32, bool, f64, f64, f64, f64, f64, f64)>,
}

fn cpu_seconds() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn close(a: f64, b: f64, rel: f64, abs: f64) -> bool {
    (a - b).abs() <= rel * a.abs().max(b.abs()) + abs
}

#[derive(Default)]
struct Diff {
    snapshots: usize,
    rows_py: usize,
    rows_rs: usize,
    only_py: usize,
    only_rs: usize,
    n: usize,
    valid: usize,
    floats: usize,
    max_rel: f64,
    examples: Vec<String>,
}

fn compare(t: f64, start: f64, golden: &GoldenSnap, rows: &[PairRow], d: &mut Diff) {
    d.snapshots += 1;
    d.rows_py += golden.pairs.len();
    d.rows_rs += rows.len();
    let rs: HashMap<(u32, u32), &PairRow> = rows.iter().map(|r| ((r.a, r.b), r)).collect();
    let mut seen = 0;
    for g in &golden.pairs {
        let Some(r) = rs.get(&(g.0, g.1)) else {
            d.only_py += 1;
            if d.examples.len() < 6 {
                d.examples.push(format!("t={t}: pair ({},{}) only in python (n={} valid={})", g.0, g.1, g.2, g.3));
            }
            continue;
        };
        seen += 1;
        let mut bad = false;
        if r.n != g.2 {
            d.n += 1;
            bad = true;
        }
        if r.valid != g.3 {
            d.valid += 1;
            bad = true;
        }
        let upd = ((r.updated - start) * 1e6).round() / 1e6;
        for (a, b, label) in [
            (r.variance, g.4, "variance"),
            (r.factor, g.5, "factor"),
            (r.i_factor, g.6, "i_factor"),
            (r.base_avg, g.7, "base_avg"),
            (r.peer_avg, g.8, "peer_avg"),
            (upd, g.9, "updated"),
        ] {
            let tol_abs = if label == "variance" { 1e-22 } else { 1e-9 };
            if !close(a, b, 1e-9, tol_abs) {
                d.floats += 1;
                bad = true;
                if d.examples.len() < 6 {
                    d.examples.push(format!("t={t}: pair ({},{}) {label}: python {b:e} rust {a:e}", g.0, g.1));
                }
            }
            let rel = (a - b).abs() / a.abs().max(b.abs()).max(1e-300);
            if label != "variance" && rel.is_finite() {
                d.max_rel = d.max_rel.max(rel);
            }
        }
        if bad && d.examples.len() < 6 {
            d.examples.push(format!("t={t}: pair ({},{}) n py {} rs {}, valid py {} rs {}", g.0, g.1, g.2, r.n, g.3, r.valid));
        }
    }
    d.only_rs += rows.len() - seen;
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let capture = args.get(1).expect("usage: syncbench capture.jsonl [--golden f] [--threads N] [--max-secs S]");
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let threads: usize = opt("--threads").and_then(|v| v.parse().ok()).unwrap_or(1);
    let max_secs: Option<f64> = opt("--max-secs").and_then(|v| v.parse().ok());
    let dump_path: Option<String> = opt("--dump");
    let snap_every: f64 = opt("--snap-every").and_then(|v| v.parse().ok()).unwrap_or(60.0);
    let golden: Option<Vec<GoldenSnap>> = opt("--golden").map(|p| {
        BufReader::new(std::fs::File::open(p).unwrap())
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect()
    });

    let t_load = Instant::now();
    let (start, events) = load(capture, max_secs);
    let syncs = events.iter().filter(|(_, e)| matches!(e, Ev::Sync { .. })).count();
    eprintln!("loaded {} events ({} sync) in {:.1}s", events.len(), syncs, t_load.elapsed().as_secs_f64());

    let engine = Engine::new(threads, threads, true, start);
    let mut uids: HashMap<u64, u32> = HashMap::new();
    let mut diff = Diff::default();
    let mut snap_idx = 0usize;
    let (mut next_snap, mut next_cleanup, mut next_clear) = (snap_every, 1.0_f64, 0.0_f64);
    let mut last_t = 0.0;

    let cpu0 = cpu_seconds();
    let wall0 = Instant::now();
    let mut snapshot_wall = 0.0;

    let mut dump = dump_path.as_ref().map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    let mut take_snapshot = |engine: &Engine, t: f64, diff: &mut Diff, snap_idx: &mut usize| {
        let s0 = Instant::now();
        let rows = engine.snapshot();
        if let Some(w) = dump.as_mut() {
            use std::io::Write;
            let pairs: Vec<_> = rows
                .iter()
                .map(|r| serde_json::json!([r.a, r.b, r.n, r.valid, r.variance, r.factor, r.i_factor, r.base_avg, r.peer_avg, ((r.updated - start) * 1e6).round() / 1e6]))
                .collect();
            let _ = writeln!(w, "{}", serde_json::json!({"t": t, "pairs": pairs}));
        }
        if let Some(g) = &golden {
            if let Some(gs) = g.get(*snap_idx) {
                compare(gs.t, start, gs, &rows, diff);
            }
        }
        *snap_idx += 1;
        let _ = t;
        s0.elapsed().as_secs_f64()
    };

    for (t, ev) in &events {
        let t = *t;
        // timers that would have fired before this event (same order as the reference)
        loop {
            let due = next_cleanup.min(next_clear);
            if due > t {
                break;
            }
            if next_clear <= next_cleanup {
                engine.clear_sync_points();
                next_clear += 15.0;
            } else {
                engine.cleanup(start + due);
                next_cleanup += 10.0;
            }
        }
        while next_snap <= t {
            snapshot_wall += take_snapshot(&engine, next_snap, &mut diff, &mut snap_idx);
            next_snap += snap_every;
        }
        last_t = t;
        let now = start + t;
        match ev {
            Ev::Connect { conn, user, lat, lon, alt, clock } => {
                if let Some(uid) = engine.connect(user, *lat, *lon, *alt, clock) {
                    uids.insert(*conn, uid);
                }
            }
            Ev::Close { conn } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.disconnect(uid);
                }
            }
            Ev::Sync { conn, et, ot, em, om } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.sync(now, uid, *et, *ot, *em, *om);
                }
            }
            Ev::Seen { conn, icaos } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.seen(now, uid, icaos);
                }
            }
            Ev::Lost { conn, icaos } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.lost(now, uid, icaos);
                }
            }
            Ev::ClockReset { conn } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.clock_reset(uid);
                }
            }
        }
    }
    // final snapshot, as in the reference
    snapshot_wall += take_snapshot(&engine, last_t, &mut diff, &mut snap_idx);
    engine.barrier();

    let wall = wall0.elapsed().as_secs_f64() - snapshot_wall;
    let cpu = cpu_seconds() - cpu0;
    let st = &engine.stats;
    use std::sync::atomic::Ordering::Relaxed;
    println!(
        "threads={threads}  sync events={syncs}  wall={wall:.3}s  cpu={cpu:.3}s  ({:.2} us cpu/sync event, {:.2} us wall/sync event)",
        cpu / syncs as f64 * 1e6,
        wall / syncs as f64 * 1e6
    );
    println!(
        "syncpoints={}  pair_updates={}  pairs_created={}  pairs_deleted={}",
        st.syncpoints_created.load(Relaxed),
        st.pair_updates.load(Relaxed),
        st.pairs_created.load(Relaxed),
        st.pairs_deleted.load(Relaxed)
    );
    engine.shutdown();

    if golden.is_some() {
        println!(
            "\ncompared {} snapshots: {} python pairing rows, {} rust rows",
            diff.snapshots, diff.rows_py, diff.rows_rs
        );
        println!(
            "pairs only in python: {}   only in rust: {}   n differs: {}   valid differs: {}   float fields differing: {}   max relative error: {:.2e}",
            diff.only_py, diff.only_rs, diff.n, diff.valid, diff.floats, diff.max_rel
        );
        for e in &diff.examples {
            println!("  {e}");
        }
    }
}
