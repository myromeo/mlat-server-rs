use mlat_core::solver::residuals;
use serde_json::Value;
use std::io::{BufRead, BufReader};
fn num(v: &Value) -> f64 { v.as_f64().unwrap_or(f64::NAN) }
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let idx: usize = args[2].parse().unwrap();
    let line = BufReader::new(std::fs::File::open(&args[1]).unwrap()).lines().nth(idx).unwrap().unwrap();
    let r: Value = serde_json::from_str(&line).unwrap();
    let x0: Vec<f64> = r["x0"].as_array().unwrap().iter().map(num).collect();
    let pr: Vec<([f64; 3], f64, f64)> = r["pr"].as_array().unwrap().iter().map(|e| {
        let p = e[0].as_array().unwrap();
        ([num(&p[0]), num(&p[1]), num(&p[2])], num(&e[1]), num(&e[2]))
    }).collect();
    let alt = (!r["alt"].is_null()).then(|| (num(&r["alt"]), num(&r["alt_err"])));
    let mut out = vec![0.0; pr.len() + alt.is_some() as usize];
    residuals(&x0, &pr, alt, &mut out);
    for v in out { println!("{:.17e}", v); }
}
