//! Prints every point at which the residuals are evaluated for one recorded
//! leastsq problem: `trace_leastsq leastsq.jsonl INDEX` (for comparing with Python).
use mlat_core::minpack::lmdif;
use mlat_core::solver::residuals;
use serde_json::Value;
use std::io::{BufRead, BufReader};

fn num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap(),
        _ => f64::NAN,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let idx: usize = args[2].parse().unwrap();
    let line = BufReader::new(std::fs::File::open(&args[1]).unwrap()).lines().nth(idx).unwrap().unwrap();
    let r: Value = serde_json::from_str(&line).unwrap();
    let x0: Vec<f64> = r["x0"].as_array().unwrap().iter().map(num).collect();
    let pr: Vec<([f64; 3], f64, f64)> = r["pr"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let p = e[0].as_array().unwrap();
            ([num(&p[0]), num(&p[1]), num(&p[2])], num(&e[1]), num(&e[2]))
        })
        .collect();
    let alt = (!r["alt"].is_null()).then(|| (num(&r["alt"]), num(&r["alt_err"])));
    let m = pr.len() + alt.is_some() as usize;
    let mut k = 0;
    let res = lmdif(
        |x, out| {
            residuals(x, &pr, alt, out);
            println!("{k} {:.17e} {:.17e} {:.17e} {:.17e} | {:.17e}", x[0], x[1], x[2], x[3], out.iter().map(|v| v * v).sum::<f64>());
            k += 1;
        },
        m,
        &x0,
        1.49012e-8,
        1.49012e-8,
        0.0,
        r["maxfev"].as_i64().unwrap_or(165) as i32,
        f64::EPSILON,
        100.0,
    );
    println!("final info={} nfev={}", res.info, res.nfev);
}
