//! Runs the whole Rust multilateration pipeline over a capture, with the same
//! event and timer ordering as testbed/py_pipeline_ref.py, and writes every
//! result in the same format so the two can be compared.
//!
//!   pipebench capture.jsonl OUT.jsonl [--threads N] [--max-secs S] [--fast]
//!
//! By default the engine runs in deterministic mode (every observation waits
//! for the pipeline to catch up). --fast lets it run free, which is how it
//! would run in production.

use mlat_core::engine::Engine;
use mlat_core::resolve::MlatResult;
use mlat_tools::capture_events::{load, Ev};
use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

fn cpu_seconds() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn num(x: f64) -> serde_json::Value {
    if x.is_finite() {
        serde_json::json!(x)
    } else {
        serde_json::json!(x.to_string())
    }
}

fn result_json(r: &MlatResult, start: f64) -> serde_json::Value {
    let rnd = |x: f64| (x * 1e6).round() / 1e6;
    serde_json::json!({
        "now": 0.0,
        "t": rnd(r.cluster_utc - start),
        "addr": r.address,
        "ecef": r.ecef.iter().map(|v| num(*v)).collect::<Vec<_>>(),
        "cov": r.ecef_cov.iter().map(|row| row.iter().map(|v| num(*v)).collect::<Vec<_>>()).collect::<Vec<_>>(),
        "receivers": r.receivers,
        "distinct": r.distinct,
        "dof": r.dof,
        "error": r.error,
        "kalman_valid": r.kalman.valid,
        "speed": r.kalman.ground_speed.map(num),
        "heading": r.kalman.heading.map(num),
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let capture = &args[1];
    let out_path = &args[2];
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let threads: usize = opt("--threads").and_then(|v| v.parse().ok()).unwrap_or(1);
    let max_secs: Option<f64> = opt("--max-secs").and_then(|v| v.parse().ok());
    let deterministic = !args.iter().any(|a| a == "--fast");

    let t_load = Instant::now();
    let (start, events) = load(capture, max_secs);
    eprintln!("loaded {} events in {:.1}s", events.len(), t_load.elapsed().as_secs_f64());

    let engine = Engine::new(threads, threads, deterministic, start);
    let mut uids: HashMap<u64, u32> = HashMap::new();
    let mut results: Vec<MlatResult> = Vec::new();

    let (mut next_15, mut next_cleanup) = (0.0_f64, 1.0_f64);
    // free-running mode: keep the workers within 10 ms of the replayed clock, as real time would
    let mut last_pace = 0.0_f64;
    let cpu0 = cpu_seconds();
    let wall0 = Instant::now();

    // fire every timer due at or before `limit` (seconds since start), in time order
    let run_timers = |limit: f64, next_cleanup: &mut f64| {
        loop {
            let cohort = engine.next_due_before(start + limit).map(|d| d - start);
            let cleanup = (*next_cleanup <= limit).then_some(*next_cleanup);
            match (cohort, cleanup) {
                (Some(c), Some(k)) if c <= k => engine.process_next_cohort(),
                (_, Some(k)) => {
                    engine.cleanup(start + k);
                    *next_cleanup += 10.0;
                }
                (Some(_), None) => engine.process_next_cohort(),
                (None, None) => break,
            }
        }
    };

    for (t, ev) in &events {
        let t = *t;
        while next_15 <= t {
            run_timers(next_15, &mut next_cleanup);
            engine.health(start + next_15);
            engine.clear_sync_points();
            next_15 += 15.0;
        }
        if !deterministic && t - last_pace >= 0.01 {
            engine.barrier();
            last_pace = t;
        }
        run_timers(t, &mut next_cleanup);
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
            Ev::Mlat { conn, ts, msg } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.mlat(now, uid, *ts, msg.clone());
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
            Ev::RateReport { conn } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.rate_report(now, uid);
                }
            }
            Ev::ClockReset { conn } => {
                if let Some(&uid) = uids.get(conn) {
                    engine.clock_reset(uid);
                }
            }
        }
        results.extend(engine.take_results());
    }
    engine.flush();
    results.extend(engine.take_results());

    let wall = wall0.elapsed().as_secs_f64();
    let cpu = cpu_seconds() - cpu0;
    let mut out = std::io::BufWriter::new(std::fs::File::create(out_path).unwrap());
    results.sort_by(|a, b| a.cluster_utc.partial_cmp(&b.cluster_utc).unwrap().then(a.address.cmp(&b.address)));
    for r in &results {
        writeln!(out, "{}", result_json(r, start)).unwrap();
    }
    println!(
        "threads={threads} mode={}  events={}  results={}  wall={wall:.2}s  cpu={cpu:.2}s",
        if deterministic { "deterministic" } else { "free-running" },
        events.len(),
        results.len()
    );
    engine.shutdown();
}
