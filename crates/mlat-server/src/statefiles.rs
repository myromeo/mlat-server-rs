//! The 15 s state files (`sync.json`, `clients.json`, `aircraft.json`) and the
//! status line, in the same layout as the Python server writes them.

use crate::interest::InterestSnapshot;
use crate::state::{log, Server};
use mlat_core::engine::HealthReport;
use mlat_core::geodesy::ecef2llh;
use mlat_core::pairing::round_dec;
use serde_json::{json, Map, Value};
use std::sync::atomic::Ordering;

/// Python's `round(x, ndigits)` as a JSON float.
fn r(x: f64, nd: usize) -> Value {
    json!(round_dec(x, nd))
}

/// `bad_syncs` is the integer 0 whenever it has been clamped at zero, and a
/// float otherwise, which shows in the JSON.
fn bad_syncs_json(x: f64, nd: Option<usize>) -> Value {
    if x <= 0.0 {
        json!(0)
    } else {
        match nd {
            Some(n) => r(x, n),
            None => json!(x),
        }
    }
}

/// Writes the file whole, then renames it into place so readers never see half of it.
/// (Serialised in memory first: straight into the file it is one system call per token.)
fn write_atomic(path: std::path::PathBuf, v: &Value) {
    let tmp = path.with_extension("json.tmp");
    let Ok(bytes) = serde_json::to_vec(v) else { return };
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Python's `str()` of a list of strings.
fn py_list_repr(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", "))
}

pub struct StatusState {
    pub next_status: f64,
    pub last_cpu: f64,
}

pub fn write_state(server: &Server, health: &HealthReport, interest: &InterestSnapshot, now: f64, status: &mut StatusState) {
    let work = &server.cfg.work_dir;
    let hex = |v: &Vec<u32>| v.iter().map(|i| format!("{i:06x}")).collect::<Vec<_>>();

    // ---- aircraft.json (the counts are needed for the status line even when the file is not written)
    let write_aircraft = server.cfg.aircraft_json;
    let mut aircraft = Map::new();
    let (mut ac_count_sync, mut ac_total) = (0usize, 0usize);
    for a in &health.aircraft {
        ac_total += 1;
        let sync_i = interest.ac_sync.get(&a.icao).copied().unwrap_or(0);
        if sync_i > 0 {
            ac_count_sync += 1;
        }
        if !write_aircraft {
            continue;
        }
        let seen = a.seen.max(interest.ac_seen.get(&a.icao).copied().unwrap_or(0.0));
        let elapsed_seen = now - seen;
        let mlat_i = interest.ac_mlat.get(&a.icao).copied().unwrap_or(0);
        let name = format!("{:06X}", a.icao);
        let mut s = Map::new();
        s.insert("icao".into(), json!(name));
        s.insert("elapsed_seen".into(), r(elapsed_seen, 1));
        s.insert("interesting".into(), json!(i32::from(sync_i > 0 || (a.allow_mlat && mlat_i > 0))));
        s.insert("allow_mlat".into(), json!(i32::from(a.allow_mlat)));
        s.insert("tracking".into(), json!(a.tracking.len()));
        s.insert("sync_interest".into(), json!(sync_i));
        s.insert("mlat_interest".into(), json!(mlat_i));
        s.insert("adsb_seen".into(), json!(interest.ac_adsb.get(&a.icao).copied().unwrap_or(0)));
        s.insert("mlat_message_count".into(), json!(a.mlat_message_count));
        s.insert("mlat_result_count".into(), json!(a.mlat_result_count));
        s.insert("mlat_kalman_count".into(), json!(a.mlat_kalman_count));
        s.insert("sync_count_1min".into(), json!(a.sync_count));
        s.insert("sync_bad_percent".into(), json!(a.sync_bad_percent));
        if let Some((t, pos)) = a.last_result {
            s.insert("last_result".into(), r(now - t, 1));
            let [lat, lon, _] = ecef2llh(&pos);
            s.insert("lat".into(), r(lat, 4));
            s.insert("lon".into(), r(lon, 4));
            if let Some(alt) = a.altitude {
                s.insert("alt".into(), json!(alt));
            }
            if let Some((heading, speed)) = a.kalman {
                s.insert("heading".into(), json!(heading.round_ties_even()));
                s.insert("speed".into(), json!(speed.round_ties_even()));
            }
        }
        if elapsed_seen > 600.0 {
            s.insert("tracking_receivers".into(), json!(a.tracking));
        }
        aircraft.insert(name, Value::Object(s));
    }

    // ---- sync.json and clients.json
    let conns = server.conns.read().unwrap();
    let mut uids: Vec<u32> = conns.keys().copied().collect();
    uids.sort();
    let user_of = |uid: u32| conns.get(&uid).map(|c| c.user.clone());
    let mut sync = Map::new();
    let mut clients = Map::new();
    for uid in &uids {
        let c = &conns[uid];
        let Some(recv) = server.engine.receiver_info(*uid) else { continue };
        let peers = health.peers.get(uid).map(|v| v.as_slice()).unwrap_or(&[]);

        let mut peer_map = Map::new();
        for p in peers {
            let Some(name) = user_of(p.peer) else { continue };
            peer_map.insert(
                name,
                json!([p.n, p.error_us, p.drift_ppm, bad_syncs_json(p.peer_bad_syncs, Some(2)), i32::from(p.jumped), p.outlier_percent, p.age, p.peer_total]),
            );
        }
        let bad = recv.bad_syncs.load();
        sync.insert(
            c.user.clone(),
            json!({ "peers": peer_map, "bad_syncs": bad_syncs_json(bad, None), "lat": c.map_lat, "lon": c.map_lon }),
        );

        let bad_peer_list: Vec<String> = health.bad_peers.get(uid).map(|v| v.iter().filter_map(|p| user_of(*p)).collect()).unwrap_or_default();
        let counter = c.msg_counter.swap(0, Ordering::Relaxed);
        clients.insert(
            c.user.clone(),
            json!({
                "user": c.user,
                "uid": c.uid,
                "uuid": c.uuid,
                "coords": format!("{:.6},{:.6}", c.lat, c.lon),
                "lat": c.lat,
                "lon": c.lon,
                "alt": c.alt,
                "privacy": c.privacy,
                "connection": c.connection_info,
                "source_ip": c.source_ip,
                "source_port": c.source_port,
                "message_rate": (counter as f64 / 15.0).round_ties_even() as i64,
                "peer_count": peers.len(),
                "bad_sync_timeout": (bad * 15.0 / 0.1).round_ties_even() as i64,
                "outlier_percent": round_dec(recv.outlier_percent_rolling.load(), 1),
                "bad_peer_list": py_list_repr(&bad_peer_list),
                "sync_interest": interest.rx_sync.get(uid).map(hex).unwrap_or_default(),
                "mlat_interest": interest.rx_mlat.get(uid).map(hex).unwrap_or_default(),
            }),
        );

        if c.return_stats {
            let stats = json!({ "stats": {
                "peer_count": peers.len(),
                "bad_sync_timeout": (bad * 15.0 / 0.1).round_ties_even() as i64,
                "outlier_percent": round_dec(recv.outlier_percent_rolling.load(), 1),
            }});
            let _ = c.tx.send(stats.to_string().into());
        }
    }

    write_atomic(work.join("sync.json"), &Value::Object(sync));
    write_atomic(work.join("clients.json"), &Value::Object(clients));
    if write_aircraft {
        write_atomic(work.join("aircraft.json"), &Value::Object(aircraft));
    }

    // ---- status line
    let total_outlier_percent = 100.0 * health.outlier_sum / (health.sync_sum + 0.1);
    let ac_count_mlat = server.engine.mlat_wanted_count();
    if now > status.next_status {
        status.next_status = now + server.cfg.status_interval;
        log(
            "coordinator",
            &if server.cfg.partition.1 > 1 {
                format!(
                    "Status: {}/{} ({} clients) ({} mlat {} sync {} tracked)",
                    server.cfg.partition.0,
                    server.cfg.partition.1,
                    uids.len(),
                    ac_count_mlat,
                    ac_count_sync,
                    ac_total
                )
            } else {
                format!(
                    "Status: ({} clients {} bad sync) ({:.2} outlier_percentage) ({} mlat {} sync {} tracked)",
                    uids.len(),
                    health.bad_receivers,
                    total_outlier_percent,
                    ac_count_mlat,
                    ac_count_sync,
                    ac_total
                )
            },
        );
    }

    // ---- node_exporter textfile, if the directory exists
    if std::path::Path::new("/run/node_exporter").is_dir() {
        let cpu = {
            let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
            let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
            tv(ru.ru_utime) + tv(ru.ru_stime)
        };
        let ppm = ((cpu - status.last_cpu) * (1e6 / 15.0)).round() as i64;
        status.last_cpu = cpu;
        let out = format!(
            "mlat_server_cpu_ppm {ppm}\nmlat_server_receivers {}\nmlat_server_ac_mlat {ac_count_mlat}\nmlat_server_ac_sync {ac_count_sync}\nmlat_server_ac_total {ac_total}\nmlat_server_outlier_ppm {:.0}\n",
            uids.len(),
            total_outlier_percent * 1000.0
        );
        let _ = std::fs::write("/run/node_exporter/mlat-server.prom", out);
    }
}
