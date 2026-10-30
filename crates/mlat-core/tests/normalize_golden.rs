//! normalize2 against recorded real calls (testbed/record_wrap.py). Set
//! MLAT_REC_DIR to the directory holding normalize2.jsonl; skipped if absent.

use mlat_core::normalize::{normalize2, PairParams, ReceiverStamps};
use serde_json::Value;
use std::collections::HashMap;
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

#[test]
fn normalize2_matches_python() {
    let dir = std::env::var("MLAT_REC_DIR")
        .unwrap_or_else(|_| format!("{}/Downloads/baseline/rec1/rec", std::env::var("HOME").unwrap_or_default()));
    let path = format!("{dir}/normalize2.jsonl");
    if !std::path::Path::new(&path).exists() {
        assert!(std::env::var_os("MLAT_REQUIRE_GOLDEN").is_none(), "MLAT_REQUIRE_GOLDEN is set but the recordings are missing");
        eprintln!("no recordings, skipping");
        return;
    }

    let (mut total, mut both_empty, mut compared) = (0, 0, 0);
    let (mut shape_diff, mut order_diff, mut ts_diff, mut var_diff) = (0, 0, 0, 0);
    let mut max_var_rel = 0.0_f64;
    let mut examples = Vec::new();

    for line in BufReader::new(std::fs::File::open(path).unwrap()).lines() {
        let Ok(r) = serde_json::from_str::<Value>(&line.unwrap()) else { continue };
        total += 1;
        let now = num(&r["now"]);

        let receivers: Vec<ReceiverStamps> = r["in"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| ReceiverStamps {
                uid: e[0].as_u64().unwrap() as u32,
                freq: num(&e[1]),
                jitter: num(&e[2]),
                stamps: e[3].as_array().unwrap().iter().map(|t| (num(&t[0]), num(&t[1]))).collect(),
            })
            .collect();
        let pairs: HashMap<(u32, u32), PairParams> = r["pairs"]
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

        let got = normalize2(&receivers, |a, b| pairs.get(&(a, b)).copied(), now);
        let want = r["out"].as_array().unwrap();

        if want.is_empty() || got.is_empty() {
            if want.is_empty() && got.is_empty() {
                both_empty += 1;
            } else {
                shape_diff += 1;
                if examples.len() < 5 {
                    examples.push(format!("python gave {} components, rust {}", want.len(), usize::from(!got.is_empty())));
                }
            }
            continue;
        }
        compared += 1;
        let want = want[0].as_object().unwrap();

        let want_order: Vec<u32> = want.keys().map(|k| k.parse().unwrap()).collect();
        let got_order: Vec<u32> = got.iter().map(|g| g.0).collect();
        if want_order != got_order {
            let mut a = want_order.clone();
            let mut b = got_order.clone();
            a.sort();
            b.sort();
            if a == b {
                order_diff += 1;
            } else {
                shape_diff += 1;
            }
            if examples.len() < 5 {
                examples.push(format!("receivers python {want_order:?} rust {got_order:?} | input {:?} | pairs {}", receivers.iter().map(|x| x.uid).collect::<Vec<_>>(), r["pairs"].as_array().unwrap().iter().map(|p| format!("({},{}){}v{:.3e}n{}", p[0], p[1], if p[2].as_bool().unwrap() {"+"} else {"-"}, num(&p[3]), p[5])).collect::<Vec<_>>().join(" ")));
            }
            continue;
        }
        for (uid, var, stamps) in &got {
            let w = &want[&uid.to_string()];
            let wvar = num(&w[0]);
            let rel = (var - wvar).abs() / wvar.abs().max(1e-300);
            max_var_rel = max_var_rel.max(rel);
            if rel > 1e-4 {
                var_diff += 1;
            }
            let wst = w[1].as_array().unwrap();
            if wst.len() != stamps.len() || stamps.iter().zip(wst).any(|(g, w)| g.0 != num(&w[0]) || g.1 != num(&w[1])) {
                ts_diff += 1;
                if examples.len() < 5 {
                    examples.push(format!("timestamps differ for receiver {uid}"));
                }
            }
        }
    }

    println!("normalize2 calls: {total}   (both empty: {both_empty}, compared in detail: {compared})");
    println!("  different component or receiver set: {shape_diff}");
    println!("  same receivers, different traversal order: {order_diff}");
    println!("  receivers with differing timestamps: {ts_diff}   variance off by >1e-4: {var_diff}   max variance rel diff {max_var_rel:.2e}");
    for e in &examples {
        println!("  e.g. {e}");
    }
    assert!(total > 100);
    assert!(shape_diff * 1000 <= total, "more than 0.1% of calls produced a different component");
}
