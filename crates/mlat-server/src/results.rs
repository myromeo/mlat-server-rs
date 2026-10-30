//! Delivers multilateration results: to the receivers that heard the message
//! (`report_mlat_position_*`), and to Basestation listeners (`BasestationClient`).

use crate::outputs::Outputs;
use crate::state::{log, now, Line, Server};
use tokio::sync::mpsc::error::TrySendError;
use ahash::AHashMap;
use mlat_core::geodesy::ecef2llh;
use mlat_core::output::{sbs_line, KalmanView, SbsAircraft, MS_TO_KTS};
use mlat_core::pairing::round_dec;
use mlat_core::resolve::MlatResult;
use serde_json::{json, Map, Value};
use std::sync::Arc;

const MTOF: f64 = 1.0 / mlat_core::FTOM;

fn f(x: f64) -> Value {
    json!(x)
}

fn round0(x: f64) -> Value {
    json!(x.round_ties_even())
}

/// The result as a receiver with `return_result_format: ecef` gets it.
fn ecef_message(r: &MlatResult) -> String {
    let mut m = Map::new();
    m.insert("@".into(), f(round_dec(r.cluster_utc, 3)));
    m.insert("addr".into(), json!(format!("{:06x}", r.address)));
    m.insert("n".into(), json!(r.group_receivers.len()));
    m.insert("nd".into(), json!(r.distinct));
    let c = &r.ecef_cov;
    m.insert("cov".into(), Value::Array([c[0][0], c[0][1], c[0][2], c[1][1], c[1][2], c[2][2]].iter().map(|v| round0(*v)).collect()));

    if r.kalman.valid && r.kalman.last_update >= r.cluster_utc {
        if let (Some(gs), Some(h)) = (r.kalman.ground_speed, r.kalman.heading) {
            let speed = gs * MS_TO_KTS;
            m.insert("nsvel".into(), f(round_dec(h.to_radians().cos() * speed, 1)));
            m.insert("ewvel".into(), f(round_dec(h.to_radians().sin() * speed, 1)));
        }
    }
    if let (Some(vt), Some(v)) = (r.vrate_time, r.vrate) {
        if vt != 0.0 && r.cluster_utc - vt < 5.0 {
            m.insert("vrate".into(), json!(v));
        }
    }
    m.insert("ecef".into(), Value::Array(r.ecef.iter().map(|v| round0(*v)).collect()));
    json!({ "result": m }).to_string()
}

/// ... and for a receiver using the old format.
fn old_message(r: &MlatResult) -> String {
    let [lat, lon, alt] = ecef2llh(&r.ecef);
    json!({ "result": {
        "@": round_dec(r.cluster_utc, 3),
        "addr": format!("{:06x}", r.address),
        "lat": round_dec(lat, 5),
        "lon": round_dec(lon, 5),
        "alt": (alt * MTOF).round_ties_even(),
        "callsign": r.callsign,
        "squawk": r.squawk,
        "hdop": 0.0, "vdop": 0.0, "tdop": 0.0, "gdop": 0.0,
        "nstations": r.group_receivers.len(),
    }})
    .to_string()
}

/// Consumes results until the engine shuts down.
pub fn run(server: Arc<Server>, outs: Outputs) {
    let rx = server.engine.results_receiver();
    // per aircraft: the time of the last poor result let through (see sbs_line)
    let mut crappy: AHashMap<u32, f64> = AHashMap::new();

    while let Ok(r) = rx.recv() {
        // output handlers first: the Basestation listeners
        {
            let mut clients = server.sbs_clients.lock().unwrap();
            if !clients.is_empty() {
                let view = KalmanView { valid: r.kalman.valid, last_update: r.kalman.last_update, ground_speed: r.kalman.ground_speed, heading: r.kalman.heading };
                let sent = now();
                let mut lines: [Option<Option<Line>>; 2] = [None, None];
                clients.retain(|c| {
                    let slot = &mut lines[c.filtered as usize];
                    let line = slot.get_or_insert_with(|| {
                        let mut ac = SbsAircraft {
                            callsign: &r.callsign,
                            squawk: &r.squawk,
                            altitude: r.altitude,
                            last_altitude_time: r.last_altitude_time,
                            vrate: r.vrate,
                            vrate_time: r.vrate_time,
                            last_crappy_output: crappy.get(&r.address).copied().unwrap_or(0.0),
                        };
                        let l = sbs_line(r.cluster_utc, r.address, &r.ecef, r.receivers.len(), r.dof, &view, Some(r.error), &mut ac, c.filtered, sent);
                        crappy.insert(r.address, ac.last_crappy_output);
                        l.map(Line::from)
                    });
                    match line {
                        // a peer that has stopped reading is dropped
                        Some(l) => match c.tx.try_send(l.clone()) {
                            Ok(()) => true,
                            Err(TrySendError::Full(_)) => {
                                log("basestation", "Send queue full, dropping connection");
                                false
                            }
                            Err(TrySendError::Closed(_)) => false,
                        },
                        None => !c.tx.is_closed(),
                    }
                });
            }
        }

        if !outs.csv.is_empty() {
            let users: Vec<String> = {
                let conns = server.conns.read().unwrap();
                r.receivers.iter().filter_map(|u| conns.get(u).map(|c| c.user.clone())).collect()
            };
            for w in &outs.csv {
                w.write(&r, &users);
            }
        }
        if let Some(p) = &outs.pseudo {
            p.write(&r);
        }

        // then the receivers that heard the message
        // (don't forward if the Kalman filter hasn't locked on and it's only 3 receivers)
        if !r.kalman.valid && r.dof < 1 {
            continue;
        }
        let mut ecef_line: Option<Line> = None;
        let mut old_line: Option<Line> = None;
        let conns = server.conns.read().unwrap();
        for uid in &r.group_receivers {
            let Some(c) = conns.get(uid) else { continue };
            if !c.return_results {
                continue;
            }
            let line = if c.result_format_ecef {
                ecef_line.get_or_insert_with(|| Line::from(ecef_message(&r))).clone()
            } else {
                old_line.get_or_insert_with(|| Line::from(old_message(&r))).clone()
            };
            c.send(line);
        }
    }
}
