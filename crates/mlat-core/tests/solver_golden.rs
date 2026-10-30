//! The solver against recorded real calls of scipy's `leastsq` and of
//! `solver.solve`, from a replay of a capture (testbed/record_wrap.py).
//! Set MLAT_REC_DIR to the directory holding leastsq.jsonl / solve.jsonl;
//! the tests do nothing when the recordings are absent.

use mlat_core::solver::{fit, solve, Measurement, SOLVER_MAXFEV};
use serde_json::Value;
use std::io::{BufRead, BufReader};

fn rec_dir() -> Option<String> {
    let dir = std::env::var("MLAT_REC_DIR")
        .unwrap_or_else(|_| format!("{}/Downloads/baseline/rec1/rec", std::env::var("HOME").unwrap_or_default()));
    std::path::Path::new(&dir).join("leastsq.jsonl").exists().then_some(dir)
}

/// A JSON number, or one of the strings the recorder uses for non-finite values.
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

fn vec3(v: &Value) -> [f64; 3] {
    let a = v.as_array().unwrap();
    [num(&a[0]), num(&a[1]), num(&a[2])]
}

fn lines(path: String) -> impl Iterator<Item = Value> {
    BufReader::new(std::fs::File::open(path).unwrap()).lines().filter_map(|l| {
        let l = l.ok()?;
        serde_json::from_str(&l).ok() // a partially written last line is skipped
    })
}

fn pct(v: &mut Vec<f64>, q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
}

#[test]
fn leastsq_matches_scipy() {
    let Some(dir) = rec_dir() else {
        assert!(std::env::var_os("MLAT_REQUIRE_GOLDEN").is_none(), "MLAT_REQUIRE_GOLDEN is set but the recordings are missing");
        eprintln!("no recordings, skipping");
        return;
    };
    let (mut total, mut same_path, mut info_diff, mut cov_none_diff) = (0, 0, 0, 0);
    let mut dx = Vec::new();
    let mut dcov = Vec::new();
    let mut examples = Vec::new();
    let mut shape: std::collections::BTreeMap<(bool, usize), (u32, u32)> = Default::default();

    let file = std::env::var("MLAT_LEASTSQ_FILE").unwrap_or_else(|_| format!("{dir}/leastsq.jsonl"));
    println!("reference: {file}");
    for r in lines(file) {
        total += 1;
        let x0 = r["x0"].as_array().unwrap();
        let x0 = [num(&x0[0]), num(&x0[1]), num(&x0[2]), num(&x0[3])];
        let pr: Vec<([f64; 3], f64, f64)> = r["pr"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| (vec3(&e[0]), num(&e[1]), num(&e[2])))
            .collect();
        let alt = (!r["alt"].is_null()).then(|| (num(&r["alt"]), num(&r["alt_err"])));
        let got = fit(x0, &pr, alt, r["maxfev"].as_i64().unwrap_or(SOLVER_MAXFEV as i64) as i32);

        let want_info = r["info"].as_i64().unwrap() as i32;
        let want_nfev = r["nfev"].as_i64().unwrap() as i32;
        {
            let e = shape.entry((alt.is_some(), pr.len())).or_insert((0, 0));
            e.0 += 1;
            if got.info != want_info || got.nfev != want_nfev {
                e.1 += 1;
            }
        }
        if got.info != want_info {
            info_diff += 1;
            if examples.len() < 5 {
                examples.push(format!("info: python {want_info} rust {} (nfev py {want_nfev} rs {})", got.info, got.nfev));
            }
            continue;
        }
        if got.nfev == want_nfev {
            same_path += 1;
        } else if examples.len() < 5 {
            examples.push(format!("nfev: python {want_nfev} rust {}", got.nfev));
        }
        let want_x = r["x"].as_array().unwrap();
        let d = (0..4).map(|i| (got.x[i] - num(&want_x[i])).abs()).fold(0.0, f64::max);
        dx.push(d);

        match (&r["cov"], &got.cov) {
            (Value::Null, None) => {}
            (Value::Array(c), Some(g)) => {
                let tr_want = num(&c[0][0]) + num(&c[1][1]) + num(&c[2][2]);
                let tr_got = g[0][0] + g[1][1] + g[2][2];
                dcov.push((tr_got - tr_want).abs() / tr_want.abs().max(tr_got.abs()).max(1e-300));
            }
            _ => cov_none_diff += 1,
        }
    }

    println!("leastsq calls: {total}");
    println!("  same info code:        {} of {total}", total - info_diff);
    println!("  identical eval count:  {same_path}");
    println!("  covariance present/absent mismatches: {cov_none_diff}");
    println!(
        "  |x diff| (m/units): median {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut dx.clone(), 0.5),
        pct(&mut dx.clone(), 0.99),
        pct(&mut dx.clone(), 1.0)
    );
    println!(
        "  covariance trace relative diff: median {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut dcov.clone(), 0.5),
        pct(&mut dcov.clone(), 0.99),
        pct(&mut dcov.clone(), 1.0)
    );
    println!("  path divergence by (altitude used, receivers): total/diverged");
    for ((a, m), (t, d)) in &shape {
        println!("    alt={a:<5} receivers={m:<2} {t:>4} / {d:<4}");
    }
    for e in &examples {
        println!("  e.g. {e}");
    }
    assert!(total > 100);
    assert!(info_diff * 100 <= total * 10, "more than 10% of solves end differently");
}

#[test]
fn solve_matches_python() {
    let Some(dir) = rec_dir() else {
        assert!(std::env::var_os("MLAT_REQUIRE_GOLDEN").is_none(), "MLAT_REQUIRE_GOLDEN is set but the recordings are missing");
        eprintln!("no recordings, skipping");
        return;
    };
    let (mut total, mut none_ok, mut some_ok, mut mismatch) = (0, 0, 0, 0);
    let mut dpos = Vec::new();
    let mut rel = Vec::new(); // position difference as a fraction of the solution's own error estimate
    for r in lines(format!("{dir}/solve.jsonl")) {
        total += 1;
        let m: Vec<Measurement> = r["m"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| Measurement { pos: vec3(&e[0]), timestamp: num(&e[1]), variance: num(&e[2]) })
            .collect();
        let alt = (!r["alt"].is_null()).then(|| (num(&r["alt"]), num(&r["alt_err"])));
        let got = solve(&m, alt, vec3(&r["guess"]));
        match (&r["out"], got) {
            (Value::Null, None) => none_ok += 1,
            (Value::Array(o), Some((pos, _))) => {
                let want = vec3(&o[0]);
                let d = (0..3).map(|i| (pos[i] - want[i]).powi(2)).sum::<f64>().sqrt();
                dpos.push(d);
                // the server's own error estimate: sqrt(trace(cov)), as in _resolve
                if let Value::Array(c) = &o[1] {
                    let tr = num(&c[0][0]) + num(&c[1][1]) + num(&c[2][2]);
                    rel.push(d / tr.abs().sqrt().max(1e-9));
                }
                some_ok += 1;
            }
            _ => mismatch += 1,
        }
    }
    println!("solve calls: {total}  both failed: {none_ok}  both solved: {some_ok}  disagree on success: {mismatch}");
    println!(
        "  position diff (m): median {:.2e}  p90 {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut dpos.clone(), 0.5),
        pct(&mut dpos.clone(), 0.9),
        pct(&mut dpos.clone(), 0.99),
        pct(&mut dpos.clone(), 1.0)
    );
    let over = |t: f64| dpos.iter().filter(|&&d| d > t).count();
    println!("  solves differing by more than 1 m: {}   10 m: {}   100 m: {}   1 km: {}", over(1.0), over(10.0), over(100.0), over(1000.0));
    println!(
        "  difference relative to the solution's own error estimate: median {:.2e}  p99 {:.2e}  max {:.2e}",
        pct(&mut rel.clone(), 0.5),
        pct(&mut rel.clone(), 0.99),
        pct(&mut rel.clone(), 1.0)
    );
    assert!(total > 100);
    assert!(mismatch * 100 <= total);
}
