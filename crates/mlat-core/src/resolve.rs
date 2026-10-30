//! Turns a group of copies of one Mode S message into a position, ported from
//! `MlatTracker._resolve` in mlattrack.py: bookkeeping from the decoded
//! message, clock normalisation, clustering, the least-squares solve and the
//! Kalman update. Behaviour (including unit quirks) is kept identical.

use crate::cluster::{cluster_timestamps, Cluster};
use crate::geodesy::{ecef2llh, ecef_distance, llh2ecef};
use crate::kalman::{KalmanState, Model};
use crate::modes::decode_any;
use crate::normalize::{Normalised, ReceiverStamps};
use crate::receiver::Receiver;
use crate::solver::{solve, Measurement, MAX_ALT, MIN_ALT};
use crate::FTOM;
use ahash::AHashMap;
use std::collections::HashMap;
use std::sync::Arc;

pub const RESOLVE_INTERVAL: f64 = 0.15;
pub const RESOLVE_BACKOFF: f64 = 0.7;

/// All copies of one message, collected over `MLAT_DELAY`.
pub struct Group {
    /// A placeholder (see `Collector::add_placeholder`): never resolves to anything.
    pub dummy: bool,
    pub message: Vec<u8>,
    pub first_seen: f64,
    /// (receiver uid, receiver timestamp, utc received)
    pub copies: Vec<(u32, f64, f64)>,
    /// Every receiver that heard the message (results are returned to these).
    pub receivers: Vec<u32>,
}

/// Where the resolver finds the aircraft it updates.
pub trait AircraftTable {
    fn get_mut(&mut self, icao: u32) -> Option<&mut AircraftMlat>;
}

impl AircraftTable for HashMap<u32, AircraftMlat> {
    fn get_mut(&mut self, icao: u32) -> Option<&mut AircraftMlat> {
        HashMap::get_mut(self, &icao)
    }
}

/// What the resolver needs from the rest of the system.
pub trait ResolveEnv {
    fn receiver(&self, uid: u32) -> Option<Arc<Receiver>>;
    fn normalize(&self, receivers: &[ReceiverStamps], now: f64) -> Vec<Normalised>;
}

/// The multilateration state of one aircraft.
#[derive(Clone, Debug)]
pub struct AircraftMlat {
    pub icao: u32,
    pub allow_mlat: bool,
    pub seen: f64,
    pub mlat_message_count: u64,
    pub mlat_result_count: u64,
    pub mlat_kalman_count: u64,
    pub altitude: Option<i32>,
    pub last_altitude_time: Option<f64>,
    pub alt_history: Vec<(f64, i32)>,
    pub vrate: Option<i32>,
    pub vrate_time: Option<f64>,
    pub squawk: Option<String>,
    pub callsign: Option<String>,
    pub last_result_time: Option<f64>,
    pub last_result_position: Option<[f64; 3]>,
    pub last_result_var: Option<f64>,
    pub last_result_dof: i32,
    pub last_resolve_attempt: f64,
    pub last_force_mlat: f64,
    pub force_mlat: bool,
    pub kalman: KalmanState,
}

impl AircraftMlat {
    pub fn new(icao: u32, now: f64) -> AircraftMlat {
        AircraftMlat {
            icao,
            allow_mlat: true,
            seen: now,
            mlat_message_count: 0,
            mlat_result_count: 0,
            mlat_kalman_count: 0,
            altitude: None,
            last_altitude_time: None,
            alt_history: Vec::new(),
            vrate: None,
            vrate_time: None,
            squawk: None,
            callsign: None,
            last_result_time: None,
            last_result_position: None,
            last_result_var: None,
            last_result_dof: 0,
            last_resolve_attempt: 0.0,
            last_force_mlat: now,
            force_mlat: false,
            kalman: KalmanState::new(icao, Model::ConstantAcceleration),
        }
    }
}

#[derive(Clone, Debug)]
pub struct KalmanOut {
    pub valid: bool,
    pub last_update: f64,
    pub heading: Option<f64>,
    pub ground_speed: Option<f64>,
    pub vertical_speed: Option<f64>,
    pub position_llh: Option<[f64; 3]>,
    pub position_error: Option<f64>,
}

/// A multilateration result, as handed to the output handlers.
#[derive(Clone, Debug)]
pub struct MlatResult {
    pub cluster_utc: f64,
    pub address: u32,
    pub ecef: [f64; 3],
    pub ecef_cov: [[f64; 3]; 3],
    /// Receivers whose timestamps were used, in timestamp order.
    pub receivers: Vec<u32>,
    pub distinct: usize,
    pub dof: i32,
    pub kalman: KalmanOut,
    pub error: i64,
    /// Every receiver that heard the message (results are returned to these).
    pub group_receivers: Vec<u32>,
    /// What the output formats read from the aircraft when the result was made.
    pub callsign: Option<String>,
    pub squawk: Option<String>,
    pub altitude: Option<i32>,
    pub last_altitude_time: Option<f64>,
    pub vrate: Option<i32>,
    pub vrate_time: Option<f64>,
    /// The solver's inputs, for `--dump-pseudorange`.
    pub cluster_meas: Vec<Measurement>,
    /// The altitude constraint used (metres) and its error.
    pub solve_altitude: Option<f64>,
    pub solve_altitude_error: Option<f64>,
}

#[derive(Default, Debug, Clone)]
pub struct ResolveStats {
    pub valid_groups: u64,
    pub normalize: u64,
    pub solve_attempt: u64,
    pub solve_success: u64,
    pub solve_used: u64,
}

/// Resolves one group. `aircraft` is the table of tracked aircraft; a message
/// from an untracked aircraft is dropped. Returns the accepted result, if any.
pub fn resolve(
    group: &Group,
    aircraft: &mut dyn AircraftTable,
    now: f64,
    env: &dyn ResolveEnv,
    stats: &mut ResolveStats,
) -> Option<MlatResult> {
    if group.copies.len() < 3 {
        return None;
    }

    let decoded = decode_any(&group.message)?;
    if decoded.address == 0 {
        return None;
    }
    let ac = aircraft.get_mut(decoded.address)?;
    ac.seen = now;
    ac.mlat_message_count += 1;
    stats.valid_groups += 1;

    if !ac.allow_mlat {
        return None;
    }

    // Having seen a few copies, the message is probably right: take any new
    // altitude, squawk or callsign from it.
    if let Some(alt) = decoded.altitude {
        if alt > -1500 && alt < 75000 {
            let take = match ac.last_altitude_time {
                None => true,
                Some(t) if t == 0.0 => true,
                Some(t) => {
                    group.first_seen > t && (group.first_seen - t > 15.0 || (ac.altitude.unwrap_or(0) - alt).abs() < 4000)
                }
            };
            if take {
                ac.altitude = Some(alt);
                ac.last_altitude_time = Some(group.first_seen);

                let mut hist: Vec<(f64, i32)> = ac.alt_history.iter().copied().filter(|(ts, _)| group.first_seen - ts < 20.0).collect();
                hist.push((group.first_seen, alt));
                ac.alt_history = hist;

                let (first_ts, first_alt) = ac.alt_history[0];
                let ts_diff = group.first_seen - first_ts;
                if ts_diff > 10.0 {
                    // feet per minute
                    let new_vrate = (alt - first_alt) as f64 / (ts_diff / 60.0);
                    match (ac.vrate, ac.vrate_time) {
                        (Some(v), Some(vt)) if v != 0 && group.first_seen - vt < 15.0 => {
                            ac.vrate = Some((v as f64 + 0.3 * (new_vrate - v as f64)) as i32);
                        }
                        _ => ac.vrate = Some(new_vrate as i32),
                    }
                    ac.vrate_time = Some(group.first_seen);
                }
            }
        }
    }
    if let Some(s) = decoded.squawk {
        ac.squawk = Some(s);
    }
    if let Some(c) = decoded.callsign {
        ac.callsign = Some(c);
    }

    if now - ac.last_resolve_attempt < RESOLVE_INTERVAL {
        return None;
    }
    ac.last_resolve_attempt = now;

    // the previous result, if recent enough to be useful
    let (last_result_position, last_result_dof, last_result_time) = match (ac.last_result_position, ac.last_result_time) {
        (Some(p), Some(t)) if group.first_seen - t <= 120.0 => (Some(p), ac.last_result_dof, t),
        _ => (None, 0, group.first_seen - 120.0),
    };

    let mut elapsed = group.first_seen - last_result_time;
    if elapsed < 0.0 {
        elapsed = 0.0;
    }
    if elapsed < RESOLVE_BACKOFF {
        return None;
    }

    // altitude (the limits are in metres and the aircraft altitude in feet,
    // exactly as in the Python server)
    let (altitude, altitude_dof) = match ac.altitude {
        Some(a)
            if !((a as f64) < MIN_ALT || (a as f64) > MAX_ALT || group.first_seen > ac.last_altitude_time.unwrap_or(f64::NAN) + 45.0) =>
        {
            (Some(a as f64 * FTOM), 1)
        }
        _ => (None, 0),
    };

    let len_copies = group.copies.len() as i32;
    let max_dof = len_copies + altitude_dof - 4;
    if max_dof < 0 {
        return None;
    }
    if elapsed < 2.0 * RESOLVE_BACKOFF && (max_dof as f64) < last_result_dof as f64 - elapsed + 0.5 {
        return None;
    }

    // receiver -> timestamps, in order of first appearance
    let mut order: Vec<u32> = Vec::new();
    let mut by_receiver: AHashMap<u32, Vec<(f64, f64)>> = AHashMap::new();
    for &(uid, ts, utc) in &group.copies {
        by_receiver.entry(uid).or_insert_with(|| {
            order.push(uid);
            Vec::new()
        });
        by_receiver.get_mut(&uid).unwrap().push((ts, utc));
    }

    let dof = order.len() as i32 + altitude_dof - 4;
    if dof < 0 {
        return None;
    }
    if elapsed < 2.0 * RESOLVE_BACKOFF && (dof as f64) < last_result_dof as f64 - elapsed + 0.5 {
        return None;
    }

    stats.normalize += 1;

    let mut stamps = Vec::with_capacity(order.len());
    for &uid in &order {
        let r = env.receiver(uid)?;
        stamps.push(ReceiverStamps { uid, freq: r.clock.freq, jitter: r.clock.jitter, stamps: by_receiver.remove(&uid).unwrap_or_default() });
    }
    let components = env.normalize(&stamps, now);

    let distance = |a: u32, b: u32| -> f64 {
        match (env.receiver(a), env.receiver(b)) {
            (Some(ra), Some(rb)) => ecef_distance(&ra.pos, &rb.pos),
            _ => f64::INFINITY,
        }
    };

    let min_component_size = (4 - altitude_dof) as usize;
    // normalize2 returns the single biggest component (or nothing)
    let mut clusters: Vec<Cluster> =
        if components.len() >= min_component_size { cluster_timestamps(&components, min_component_size, &distance) } else { Vec::new() };
    if clusters.is_empty() {
        return None;
    }

    // start from the most recent, largest, cluster
    clusters.sort_by(|a, b| a.distinct.cmp(&b.distinct).then(a.first_seen.partial_cmp(&b.first_seen).unwrap()));

    #[allow(clippy::type_complexity)]
    let mut accepted: Option<(Cluster, Vec<Measurement>, [f64; 3], [[f64; 3]; 3], f64, i64, i32, f64, Option<f64>)> = None;
    while accepted.is_none() {
        let Some(mut cluster) = clusters.pop() else { break };
        let cluster_utc = cluster.first_seen;
        let distinct = cluster.distinct;

        let elapsed = cluster_utc - last_result_time;
        let dof = distinct as i32 + altitude_dof - 4;

        if elapsed < 2.0 && (dof as f64) < last_result_dof as f64 - elapsed + 0.5 {
            return None;
        }

        // 250 ft at the time reported, degrading with age at ~4000 fpm
        let altitude_error = if decoded.altitude.is_some() {
            Some(250.0 * FTOM)
        } else if altitude.is_some() {
            Some((250.0 + (cluster_utc - ac.last_altitude_time.unwrap_or(0.0)) * 70.0) * FTOM)
        } else {
            None
        };

        cluster.members.sort_by(|a, b| a.timestamp.partial_cmp(&b.timestamp).unwrap());

        if elapsed > 30.0 && dof == 0 {
            continue;
        }

        let meas: Vec<Measurement> = cluster
            .members
            .iter()
            .map(|m| Measurement { pos: env.receiver(m.uid).map(|r| r.pos).unwrap_or([0.0; 3]), timestamp: m.timestamp, variance: m.variance })
            .collect();

        let initial_guess = if elapsed < 60.0 { last_result_position.unwrap_or(meas[0].pos) } else { meas[0].pos };

        stats.solve_attempt += 1;
        let Some((ecef, ecef_cov)) = solve(&meas, altitude.zip(altitude_error), initial_guess) else { continue };

        // an unusable covariance means the result is suspect
        let Some(cov) = ecef_cov else { continue };
        let max_error = 10e3;
        let var_est = cov[0][0] + cov[1][1] + cov[2][2];
        let error = var_est.abs().sqrt() as i64;
        if error as f64 > max_error {
            continue;
        }
        stats.solve_success += 1;

        // the higher the accuracy, the higher the frequency of positions output
        if elapsed / 20.0 < error as f64 / max_error {
            continue;
        }
        stats.solve_used += 1;

        accepted = Some((cluster, meas, ecef, cov, var_est, error, dof, cluster_utc, altitude_error));
    }

    let (cluster, meas, mut ecef, ecef_cov, var_est, error, dof, cluster_utc, altitude_error) = accepted?;
    let distinct = cluster.distinct;

    ac.last_result_position = Some(ecef);
    ac.last_result_var = Some(var_est);
    ac.last_result_dof = dof;
    ac.last_result_time = Some(cluster_utc);
    ac.mlat_result_count += 1;

    // end forced mlat once we have a reasonably accurate result
    if error < 1000 {
        ac.last_force_mlat = now;
        ac.force_mlat = false;
    }

    if let Some(alt) = altitude {
        let [lat, lon, _] = ecef2llh(&ecef);
        ecef = llh2ecef(lat, lon, alt);
        if ac.kalman.update(cluster_utc, &meas, Some(alt), altitude_error, ecef, &ecef_cov, dof as f64) {
            ac.mlat_kalman_count += 1;
        }
    } else {
        let solved_alt = ecef2llh(&ecef)[2];
        if ac.kalman.update(cluster_utc, &meas, Some(solved_alt), Some(4000.0 / ((dof + 1) as f64).sqrt()), ecef, &ecef_cov, dof as f64) {
            ac.mlat_kalman_count += 1;
        }
    }

    let k = &ac.kalman;
    Some(MlatResult {
        cluster_utc,
        address: decoded.address,
        ecef,
        ecef_cov,
        receivers: cluster.members.iter().map(|m| m.uid).collect(),
        distinct,
        dof,
        kalman: KalmanOut { valid: k.valid, last_update: k.last_update, heading: k.heading, ground_speed: k.ground_speed, vertical_speed: k.vertical_speed, position_llh: k.position_llh, position_error: k.position_error },
        error,
        group_receivers: group.receivers.clone(),
        callsign: ac.callsign.clone(),
        squawk: ac.squawk.clone(),
        altitude: ac.altitude,
        last_altitude_time: ac.last_altitude_time,
        vrate: ac.vrate,
        vrate_time: ac.vrate_time,
        cluster_meas: meas,
        solve_altitude: altitude,
        solve_altitude_error: altitude.and(altitude_error),
    })
}
