//! The Basestation line formatter against the real Python BasestationClient
//! (fixtures from testbed/gen_sbs_golden.py).

use mlat_core::output::{sbs_line, KalmanView, SbsAircraft};
use serde_json::Value;

fn opt(v: &Value) -> Option<f64> {
    v.as_f64()
}

#[test]
fn sbs_lines_match_python() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sbs_golden.json");
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let (mut ok, mut suppressed, mut bad) = (0, 0, 0);
    for c in &cases {
        let k = KalmanView {
            valid: c["kalman"]["valid"].as_bool().unwrap(),
            last_update: c["kalman"]["last_update"].as_f64().unwrap(),
            ground_speed: opt(&c["kalman"]["ground_speed"]),
            heading: opt(&c["kalman"]["heading"]),
        };

        let callsign = c["callsign"].as_str().map(str::to_string);
        let squawk = c["squawk"].as_str().map(str::to_string);
        let mut ac = SbsAircraft {
            callsign: &callsign,
            squawk: &squawk,
            altitude: c["altitude"].as_i64().map(|v| v as i32),
            last_altitude_time: opt(&c["last_altitude_time"]),
            vrate: c["vrate"].as_i64().map(|v| v as i32),
            vrate_time: opt(&c["vrate_time"]),
            last_crappy_output: c["last_crappy_in"].as_f64().unwrap(),
        };
        let ecef = c["ecef"].as_array().unwrap();
        let ecef = [ecef[0].as_f64().unwrap(), ecef[1].as_f64().unwrap(), ecef[2].as_f64().unwrap()];
        let got = sbs_line(
            c["recv_t"].as_f64().unwrap(),
            0x4ca6aa,
            &ecef,
            c["n_receivers"].as_u64().unwrap() as usize,
            c["dof"].as_i64().unwrap() as i32,
            &k,
            c["error"].as_i64(),
            &mut ac,
            c["use_kalman"].as_bool().unwrap(),
            c["now"].as_f64().unwrap(),
        );
        let want = c["line"].as_str();
        let crappy_ok = ac.last_crappy_output == c["last_crappy_out"].as_f64().unwrap();
        if got.as_deref() == want && crappy_ok {
            if want.is_some() {
                ok += 1;
            } else {
                suppressed += 1;
            }
        } else {
            bad += 1;
            if bad <= 5 {
                println!("MISMATCH\n  python {:?}\n  rust   {:?}\n  crappy_ok {crappy_ok}", want, got);
            }
        }
    }
    println!("SBS lines: {ok} identical, {suppressed} correctly suppressed, {bad} mismatches (of {})", cases.len());
    assert_eq!(bad, 0);
}
