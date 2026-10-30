//! The Kalman filter against recorded real updates (testbed/record_wrap.py),
//! replayed per aircraft. Set MLAT_REC_DIR; skipped if the recording is absent.

use mlat_core::kalman::{KalmanState, Model};
use mlat_core::linalg::Mat;
use mlat_core::solver::Measurement;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};

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

fn vec_of(v: &Value) -> Option<Vec<f64>> {
    v.as_array().map(|a| a.iter().map(num).collect())
}

fn mat_of(v: &Value) -> Option<Mat> {
    v.as_array().map(|rows| Mat::from_rows(&rows.iter().map(|r| r.as_array().unwrap().iter().map(num).collect()).collect::<Vec<_>>()))
}

fn pct(v: &mut Vec<f64>, q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
}

#[derive(Default)]
struct Stats {
    steps: usize,
    discrete_diff: usize,
    ret_diff: usize,
    pos_err_m: Vec<f64>,
    vel_err: Vec<f64>,
    cov_rel: Vec<f64>,
    derived_rel: Vec<f64>,
    examples: Vec<String>,
}

fn run(records: &BTreeMap<u64, Vec<Value>>, teacher_forced: bool) -> Stats {
    let mut st = Stats::default();
    for (icao, recs) in records {
        let mut k = KalmanState::new(*icao as u32, Model::ConstantAcceleration);
        // state before the first recorded update: fresh
        let (mut prev_mean, mut prev_cov, mut prev_acq, mut prev_out, mut prev_last, mut prev_valid): (Option<Vec<f64>>, Option<Mat>, bool, i32, f64, bool) =
            (None, None, true, 0, 0.0, false);
        for r in recs {
            if teacher_forced {
                k.restore(prev_mean.clone(), prev_cov.clone(), prev_acq, prev_out, prev_last, prev_valid);
            }
            let meas: Vec<Measurement> = r["m"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    let p = vec_of(&e[0]).unwrap();
                    Measurement { pos: [p[0], p[1], p[2]], timestamp: num(&e[1]), variance: num(&e[2]) }
                })
                .collect();
            let alt = (!r["alt"].is_null()).then(|| num(&r["alt"]));
            let alt_err = (!r["alt_err"].is_null()).then(|| num(&r["alt_err"]));
            let lp = vec_of(&r["ls_pos"]).unwrap();
            let lc = mat_of(&r["ls_cov"]).unwrap();
            let ls_cov = [[lc.at(0, 0), lc.at(0, 1), lc.at(0, 2)], [lc.at(1, 0), lc.at(1, 1), lc.at(1, 2)], [lc.at(2, 0), lc.at(2, 1), lc.at(2, 2)]];

            let ret = k.update(num(&r["t"]), &meas, alt, alt_err, [lp[0], lp[1], lp[2]], &ls_cov, num(&r["dof"]));

            st.steps += 1;
            let want_ret = r["ret"].as_bool().unwrap();
            if ret != want_ret {
                st.ret_diff += 1;
            }
            let discrete_ok = k.acquiring() == r["acquiring"].as_bool().unwrap()
                && k.outliers() as i64 == r["outliers"].as_i64().unwrap()
                && k.valid == r["valid"].as_bool().unwrap()
                && (k.last_update - num(&r["last_update"])).abs() < 1e-9
                && k.mean().is_some() == !r["mean"].is_null();
            if !discrete_ok {
                st.discrete_diff += 1;
                if st.examples.len() < 6 {
                    st.examples.push(format!(
                        "{icao:06x} t={}: acquiring rs {} py {}, outliers rs {} py {}, valid rs {} py {}, has_state rs {} py {}",
                        r["t"], k.acquiring(), r["acquiring"], k.outliers(), r["outliers"], k.valid, r["valid"], k.mean().is_some(), !r["mean"].is_null()
                    ));
                }
            } else if let (Some(wm), Some(wc), Some(gm), Some(gc)) = (vec_of(&r["mean"]), mat_of(&r["cov"]), k.mean(), k.cov()) {
                let pos_err = (0..3).map(|i| (gm[i] - wm[i]).powi(2)).sum::<f64>().sqrt();
                let vel_err = (3..6).map(|i| (gm[i] - wm[i]).powi(2)).sum::<f64>().sqrt();
                st.pos_err_m.push(pos_err);
                st.vel_err.push(vel_err);
                let mut worst = 0.0_f64;
                for i in 0..wc.r {
                    for j in 0..wc.c {
                        let scale = (wc.at(i, i).abs() * wc.at(j, j).abs()).sqrt().max(1e-300);
                        worst = worst.max((gc.at(i, j) - wc.at(i, j)).abs() / scale);
                    }
                }
                st.cov_rel.push(worst);
                if let (Some(h), Some(s)) = (k.heading, k.ground_speed) {
                    if !r["heading"].is_null() {
                        let dh = (h - num(&r["heading"])).abs().min(360.0 - (h - num(&r["heading"])).abs());
                        st.derived_rel.push(dh.max((s - num(&r["ground_speed"])).abs() / num(&r["ground_speed"]).abs().max(1.0)));
                    }
                }
            }

            prev_mean = vec_of(&r["mean"]);
            prev_cov = mat_of(&r["cov"]);
            prev_acq = r["acquiring"].as_bool().unwrap();
            prev_out = r["outliers"].as_i64().unwrap() as i32;
            prev_last = num(&r["last_update"]);
            prev_valid = r["valid"].as_bool().unwrap();
        }
    }
    st
}

fn report(name: &str, st: Stats) -> Stats {
    println!("{name}: {} updates over the recorded aircraft", st.steps);
    println!("  return value differs: {}   internal state (acquiring/outliers/valid/last_update) differs: {}", st.ret_diff, st.discrete_diff);
    println!(
        "  position error vs python (m):   median {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut st.pos_err_m.clone(), 0.5),
        pct(&mut st.pos_err_m.clone(), 0.99),
        pct(&mut st.pos_err_m.clone(), 1.0)
    );
    println!(
        "  velocity error vs python (m/s): median {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut st.vel_err.clone(), 0.5),
        pct(&mut st.vel_err.clone(), 0.99),
        pct(&mut st.vel_err.clone(), 1.0)
    );
    println!(
        "  covariance error (rel. to its own scale): median {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut st.cov_rel.clone(), 0.5),
        pct(&mut st.cov_rel.clone(), 0.99),
        pct(&mut st.cov_rel.clone(), 1.0)
    );
    println!(
        "  heading (deg) / speed (rel) error: median {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut st.derived_rel.clone(), 0.5),
        pct(&mut st.derived_rel.clone(), 0.99),
        pct(&mut st.derived_rel.clone(), 1.0)
    );
    for e in &st.examples {
        println!("  e.g. {e}");
    }
    st
}

#[test]
fn kalman_matches_python() {
    let dir = std::env::var("MLAT_REC_DIR")
        .unwrap_or_else(|_| format!("{}/Downloads/baseline/rec1/rec", std::env::var("HOME").unwrap_or_default()));
    let path = std::env::var("MLAT_KALMAN_FILE").unwrap_or_else(|_| format!("{dir}/kalman.jsonl"));
    println!("reference: {path}");
    if !std::path::Path::new(&path).exists() {
        eprintln!("no recordings, skipping");
        return;
    }
    let mut by_icao: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
    for line in BufReader::new(std::fs::File::open(path).unwrap()).lines() {
        let Ok(r) = serde_json::from_str::<Value>(&line.unwrap()) else { continue };
        assert_eq!(r["kind"].as_str(), Some("KalmanStateCA"));
        by_icao.entry(r["icao"].as_u64().unwrap()).or_default().push(r);
    }

    let forced = report("teacher-forced (each update starts from Python's recorded state)", run(&by_icao, true));
    let free = report("free-running (the whole sequence from a fresh filter)", run(&by_icao, false));

    assert!(forced.steps > 500);
    assert!(forced.discrete_diff * 200 <= forced.steps, "more than 0.5% of single-step decisions differ");
    let _ = free;
}
