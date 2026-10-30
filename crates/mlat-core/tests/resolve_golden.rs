//! `resolve` against recorded real `_resolve` calls (testbed/record_wrap.py).
//! Set MLAT_REC_DIR (resolve.jsonl); skipped if absent.

use mlat_core::clock::Clock;
use mlat_core::linalg::Mat;
use mlat_core::normalize::{normalize2, Normalised, PairParams, ReceiverStamps};
use mlat_core::receiver::Receiver;
use mlat_core::resolve::{resolve, AircraftMlat, Group, ResolveEnv, ResolveStats};
use mlat_core::C_AIR;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader};
use std::sync::Arc;

fn num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap(),
        Value::String(s) => match s.as_str() {
            "nan" => f64::NAN,
            "inf" => f64::INFINITY,
            "-inf" => f64::NEG_INFINITY,
            _ => panic!("bad number {s}"),
        },
        _ => panic!("not a number: {v}"),
    }
}
fn opt(v: &Value) -> Option<f64> {
    (!v.is_null()).then(|| num(v))
}
fn vec_of(v: &Value) -> Option<Vec<f64>> {
    v.as_array().map(|a| a.iter().map(num).collect())
}
fn mat_of(v: &Value) -> Option<Mat> {
    v.as_array().map(|rows| Mat::from_rows(&rows.iter().map(|r| r.as_array().unwrap().iter().map(num).collect()).collect::<Vec<_>>()))
}
fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap()).collect()
}
fn close(a: f64, b: f64, rel: f64) -> bool {
    (a - b).abs() <= rel * a.abs().max(b.abs()) + 1e-12
}

struct Env {
    receivers: HashMap<u32, Arc<Receiver>>,
    norm: Option<Value>,
    norm_calls: Cell<u32>,
    norm_input_matches: Cell<bool>,
    norm_seen: RefCell<Vec<u32>>,
}

impl ResolveEnv for Env {
    fn receiver(&self, uid: u32) -> Option<Arc<Receiver>> {
        self.receivers.get(&uid).cloned()
    }
    fn normalize(&self, recv: &[ReceiverStamps], _now: f64) -> Vec<Normalised> {
        self.norm_calls.set(self.norm_calls.get() + 1);
        let Some(n) = &self.norm else { return Vec::new() };
        // the stamps built from the group must be exactly what Python passed in
        let want = n["in"].as_array().unwrap();
        let same = want.len() == recv.len()
            && want.iter().zip(recv).all(|(w, r)| {
                w[0].as_u64().unwrap() as u32 == r.uid
                    && w[3].as_array().unwrap().len() == r.stamps.len()
                    && w[3].as_array().unwrap().iter().zip(&r.stamps).all(|(a, b)| num(&a[0]) == b.0 && num(&a[1]) == b.1)
            });
        self.norm_input_matches.set(same);
        self.norm_seen.borrow_mut().extend(recv.iter().map(|r| r.uid));
        let pairs: HashMap<(u32, u32), PairParams> = n["pairs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    (p[0].as_u64().unwrap() as u32, p[1].as_u64().unwrap() as u32),
                    PairParams {
                        valid: p[2].as_bool().unwrap(),
                        variance: num(&p[3]),
                        updated: num(&p[4]),
                        n: p[5].as_i64().unwrap() as i32,
                        factor: num(&p[6]),
                        i_factor: num(&p[7]),
                        base_avg: num(&p[8]),
                        peer_avg: num(&p[9]),
                    },
                )
            })
            .collect();
        normalize2(recv, |a, b| pairs.get(&(a, b)).copied(), num(&n["now"]))
    }
}

thread_local! {
    static MAGN: RefCell<BTreeMap<&'static str, Vec<f64>>> = RefCell::new(BTreeMap::new());
}

fn aircraft_from(icao: u32, s: &Value) -> AircraftMlat {
    let mut ac = AircraftMlat::new(icao, 0.0);
    ac.allow_mlat = s["allow_mlat"].as_bool().unwrap();
    ac.seen = num(&s["seen"]);
    ac.mlat_message_count = s["mmc"].as_u64().unwrap();
    ac.mlat_result_count = s["mrc"].as_u64().unwrap();
    ac.mlat_kalman_count = s["mkc"].as_u64().unwrap();
    ac.altitude = s["altitude"].as_i64().map(|v| v as i32);
    ac.last_altitude_time = opt(&s["last_altitude_time"]);
    ac.alt_history = s["alt_history"].as_array().unwrap().iter().map(|e| (num(&e[0]), e[1].as_i64().unwrap() as i32)).collect();
    ac.vrate = s["vrate"].as_i64().map(|v| v as i32);
    ac.vrate_time = opt(&s["vrate_time"]);
    ac.squawk = s["squawk"].as_str().map(str::to_string);
    ac.callsign = s["callsign"].as_str().map(str::to_string);
    ac.last_result_time = opt(&s["last_result_time"]);
    ac.last_result_position = vec_of(&s["last_result_position"]).map(|v| [v[0], v[1], v[2]]);
    ac.last_result_var = opt(&s["last_result_var"]);
    ac.last_result_dof = s["last_result_dof"].as_i64().unwrap_or(0) as i32;
    ac.last_resolve_attempt = num(&s["last_resolve_attempt"]);
    ac.last_force_mlat = num(&s["last_force_mlat"]);
    ac.force_mlat = s["force_mlat"].as_bool().unwrap();
    let k = &s["kalman"];
    ac.kalman.restore(
        vec_of(&k["mean"]),
        mat_of(&k["cov"]),
        k["acquiring"].as_bool().unwrap(),
        k["outliers"].as_i64().unwrap() as i32,
        opt(&k["last_update"]).unwrap_or(0.0),
        k["valid"].as_bool().unwrap(),
    );
    ac
}

/// Names of the fields of the aircraft state that differ from the recording.
fn state_diffs(ac: &AircraftMlat, s: &Value) -> Vec<&'static str> {
    let mut d = Vec::new();
    let f = |name: &'static str, ok: bool, d: &mut Vec<&'static str>| {
        if !ok {
            d.push(name)
        }
    };
    f("seen", ac.seen == num(&s["seen"]), &mut d);
    f("counters", ac.mlat_message_count == s["mmc"].as_u64().unwrap() && ac.mlat_result_count == s["mrc"].as_u64().unwrap() && ac.mlat_kalman_count == s["mkc"].as_u64().unwrap(), &mut d);
    f("altitude", ac.altitude == s["altitude"].as_i64().map(|v| v as i32), &mut d);
    f("last_altitude_time", ac.last_altitude_time == opt(&s["last_altitude_time"]), &mut d);
    let hist: Vec<(f64, i32)> = s["alt_history"].as_array().unwrap().iter().map(|e| (num(&e[0]), e[1].as_i64().unwrap() as i32)).collect();
    f("alt_history", ac.alt_history == hist, &mut d);
    f("vrate", ac.vrate == s["vrate"].as_i64().map(|v| v as i32) && ac.vrate_time == opt(&s["vrate_time"]), &mut d);
    f("squawk/callsign", ac.squawk.as_deref() == s["squawk"].as_str() && ac.callsign.as_deref() == s["callsign"].as_str(), &mut d);
    f("last_result_time", ac.last_result_time == opt(&s["last_result_time"]), &mut d);
    let lp = vec_of(&s["last_result_position"]);
    f("last_result_position", match (&ac.last_result_position, &lp) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            let dm = (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt();
            MAGN.with(|m| m.borrow_mut().entry("last_result_position_m").or_default().push(dm));
            dm < 2e-3
        }
        _ => false,
    }, &mut d);
    f("last_result_var", match (ac.last_result_var, opt(&s["last_result_var"])) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            MAGN.with(|m| m.borrow_mut().entry("last_result_var_rel").or_default().push((a - b).abs() / a.abs().max(b.abs()).max(1e-300)));
            close(a, b, 1e-4)
        }
        _ => false,
    }, &mut d);
    f("last_result_dof", ac.last_result_dof == s["last_result_dof"].as_i64().unwrap_or(0) as i32, &mut d);
    f("last_resolve_attempt", ac.last_resolve_attempt == num(&s["last_resolve_attempt"]), &mut d);
    f("force_mlat", ac.force_mlat == s["force_mlat"].as_bool().unwrap() && ac.last_force_mlat == num(&s["last_force_mlat"]), &mut d);
    let k = &s["kalman"];
    f("kalman_state", ac.kalman.acquiring() == k["acquiring"].as_bool().unwrap()
        && ac.kalman.outliers() as i64 == k["outliers"].as_i64().unwrap()
        && ac.kalman.valid == k["valid"].as_bool().unwrap()
        && ac.kalman.mean().is_some() == !k["mean"].is_null(), &mut d);
    if let (Some(gm), Some(wm)) = (ac.kalman.mean(), vec_of(&k["mean"])) {
        let dm = (0..3).map(|i| (gm[i] - wm[i]).powi(2)).sum::<f64>().sqrt();
        MAGN.with(|m| m.borrow_mut().entry("kalman_position_m").or_default().push(dm));
        f("kalman_mean", dm < 2e-3, &mut d);
    }
    d
}

#[test]
fn resolve_matches_python() {
    let dir = std::env::var("MLAT_REC_DIR")
        .unwrap_or_else(|_| format!("{}/Downloads/baseline/rec2/rec", std::env::var("HOME").unwrap_or_default()));
    let path = format!("{dir}/resolve.jsonl");
    if !std::path::Path::new(&path).exists() {
        assert!(std::env::var_os("MLAT_REQUIRE_GOLDEN").is_none(), "MLAT_REQUIRE_GOLDEN is set but the recordings are missing");
        eprintln!("no recordings, skipping");
        return;
    }

    let (mut total, mut untracked, mut resolved_py, mut resolved_rs, mut result_mismatch) = (0, 0, 0, 0, 0);
    let mut norm_input_bad = 0;
    let mut state_bad: BTreeMap<&'static str, u32> = BTreeMap::new();
    let mut state_bad_records = 0;
    let mut max_pos_diff = 0.0_f64;
    let mut max_cov_rel = 0.0_f64;
    let mut examples: Vec<String> = Vec::new();
    let mut result_examples: Vec<String> = Vec::new();

    for line in BufReader::new(std::fs::File::open(path).unwrap()).lines() {
        let Ok(r) = serde_json::from_str::<Value>(&line.unwrap()) else { continue };
        total += 1;
        if r["pre"].is_null() {
            untracked += 1;
            continue;
        }

        let receivers: HashMap<u32, Arc<Receiver>> = r["receivers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let uid: u32 = k.parse().unwrap();
                let p = vec_of(&v[0]).unwrap();
                let freq = num(&v[1]);
                let clock = Clock { freq, max_freq_error: 0.0, jitter: num(&v[2]), delay_factor: freq / C_AIR };
                (uid, Arc::new(Receiver::from_ecef(uid, String::new(), [p[0], p[1], p[2]], clock)))
            })
            .collect();
        let env = Env {
            receivers,
            norm: r["norm"].as_array().and_then(|a| a.first().cloned()),
            norm_calls: Cell::new(0),
            norm_input_matches: Cell::new(true),
            norm_seen: RefCell::new(Vec::new()),
        };

        let msg = hex(r["msg"].as_str().unwrap());
        let group = Group {
            dummy: false,
            message: msg.clone(),
            first_seen: num(&r["first_seen"]),
            copies: r["copies"].as_array().unwrap().iter().map(|c| (c[0].as_u64().unwrap() as u32, num(&c[1]), num(&c[2]))).collect(),
            receivers: r["group_receivers"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect(),
        };

        let icao = r["emitted"].as_array().and_then(|e| e.first()).map(|e| e["addr"].as_u64().unwrap() as u32).or_else(|| {
            mlat_core::modes::decode_any(&msg).map(|d| d.address)
        });
        let Some(icao) = icao else { continue };
        let mut table = HashMap::new();
        table.insert(icao, aircraft_from(icao, &r["pre"]));

        let mut stats = ResolveStats::default();
        let got = resolve(&group, &mut table, num(&r["now"]), &env, &mut stats);

        if !env.norm_input_matches.get() {
            norm_input_bad += 1;
        }

        let want = r["emitted"].as_array().unwrap();
        if !want.is_empty() {
            resolved_py += 1;
        }
        if got.is_some() {
            resolved_rs += 1;
        }
        match (want.first(), &got) {
            (None, None) => {}
            (Some(w), Some(g)) => {
                let wecef = vec_of(&w["ecef"]).unwrap();
                let pd = (0..3).map(|i| (g.ecef[i] - wecef[i]).powi(2)).sum::<f64>().sqrt();
                max_pos_diff = max_pos_diff.max(pd);
                let wc = mat_of(&w["cov"]).unwrap();
                let tr_w = wc.at(0, 0) + wc.at(1, 1) + wc.at(2, 2);
                let tr_g = g.ecef_cov[0][0] + g.ecef_cov[1][1] + g.ecef_cov[2][2];
                max_cov_rel = max_cov_rel.max((tr_g - tr_w).abs() / tr_w.abs().max(1e-300));
                let wrecv: Vec<u32> = w["receivers"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
                let ok = num(&w["t"]) == g.cluster_utc
                    && w["addr"].as_u64().unwrap() as u32 == g.address
                    && wrecv == g.receivers
                    && w["distinct"].as_u64().unwrap() as usize == g.distinct
                    && w["dof"].as_i64().unwrap() as i32 == g.dof
                    && w["error"].as_i64().unwrap() == g.error
                    && w["kalman"]["valid"].as_bool().unwrap() == g.kalman.valid
                    && pd < 1e-3;
                if !ok {
                    result_mismatch += 1;
                    if result_examples.len() < 5 {
                        result_examples.push(format!("{icao:06x}: result differs (pos diff {pd:.3e} m, py dof {} rs {}, py error {} rs {}, py n {} rs {})", w["dof"], g.dof, w["error"], g.error, wrecv.len(), g.receivers.len()));
                    }
                }
            }
            (w, g) => {
                result_mismatch += 1;
                if result_examples.len() < 5 {
                    result_examples.push(format!("{icao:06x}: python {} result, rust {}", if w.is_some() { "emitted a" } else { "no" }, if g.is_some() { "emitted a" } else { "no" }));
                }
            }
        }

        let diffs = state_diffs(&table[&icao], &r["post"]);
        if !diffs.is_empty() {
            state_bad_records += 1;
            for d in &diffs {
                *state_bad.entry(d).or_default() += 1;
            }
            if examples.len() < 5 {
                examples.push(format!("{icao:06x}: aircraft state differs in {diffs:?}"));
            }
        }
    }

    println!("resolve calls recorded: {total} (aircraft not tracked: {untracked})");
    println!("  results emitted: python {resolved_py}, rust {resolved_rs}   results that differ: {result_mismatch}");
    println!("  emitted position max diff {max_pos_diff:.3e} m, covariance trace max rel diff {max_cov_rel:.2e}");
    println!("  records where the normalize2 input built from the group differs: {norm_input_bad}");
    println!("  records with a different aircraft state afterwards: {state_bad_records}  {state_bad:?}");
    MAGN.with(|m| {
        for (k, v) in m.borrow().iter() {
            let mut v = v.clone();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |f: f64| v[((v.len() as f64 * f) as usize).min(v.len() - 1)];
            println!("  {k}: n={} median {:.2e} p90 {:.2e} max {:.2e}", v.len(), q(0.5), q(0.9), q(1.0));
        }
    });
    for e in result_examples.iter().chain(&examples) {
        println!("  e.g. {e}");
    }
    assert!(total > 500);
}
