//! File outputs: `--write-csv` (LocalCSVWriter) and `--dump-pseudorange`.

use crate::state::log;
use mlat_core::geodesy::ecef2llh;
use mlat_core::output::MS_TO_KTS;
use mlat_core::pairing::round_dec;
use mlat_core::resolve::MlatResult;
use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// The file outputs that were asked for.
#[derive(Default)]
pub struct Outputs {
    pub csv: Vec<CsvWriter>,
    pub pseudo: Option<std::sync::Arc<Pseudorange>>,
}

const MTOF: f64 = 1.0 / mlat_core::FTOM;

fn csv_quote(s: Option<&str>) -> String {
    match s {
        None => String::new(),
        Some(s) if !s.contains(['\n', '"', ',']) => s.to_string(),
        Some(s) => format!("\"{}\"", s.replace('"', "\"\"")),
    }
}

/// Appends lines to a file, rotating at 1 MB and keeping ten backups: Python's
/// `RotatingFileHandler(maxBytes=1 MiB, backupCount=10)`.
struct Rotating {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Rotating {
    const MAX: u64 = 1024 * 1024;
    const BACKUPS: usize = 10;

    fn backup(&self, n: usize) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    fn open(path: PathBuf) -> std::io::Result<Rotating> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Rotating { path, file: Some(file), size })
    }

    fn rollover(&mut self) {
        self.file = None;
        for i in (1..Self::BACKUPS).rev() {
            let (src, dst) = (self.backup(i), self.backup(i + 1));
            if src.exists() {
                let _ = std::fs::remove_file(&dst);
                let _ = std::fs::rename(&src, &dst);
            }
        }
        let _ = std::fs::remove_file(self.backup(1));
        let _ = std::fs::rename(&self.path, self.backup(1));
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.size = 0;
    }

    fn write_line(&mut self, line: &str) {
        let n = line.len() as u64 + 1;
        if self.size > 0 && self.size + n >= Self::MAX {
            self.rollover();
        }
        if let Some(f) = self.file.as_mut() {
            if writeln!(f, "{line}").is_ok() {
                self.size += n;
            }
        }
    }
}

pub struct CsvWriter {
    out: Mutex<Rotating>,
}

impl CsvWriter {
    pub fn new(path: &str) -> std::io::Result<CsvWriter> {
        Ok(CsvWriter { out: Mutex::new(Rotating::open(PathBuf::from(path))?) })
    }

    /// `LocalCSVWriter.write_result`
    pub fn write(&self, r: &MlatResult, users: &[String]) {
        let [lat, lon, _] = ecef2llh(&r.ecef);
        let var_est = r.ecef_cov[0][0] + r.ecef_cov[1][1] + r.ecef_cov[2][2];
        let err_est = if var_est >= 0.0 { var_est.sqrt() } else { -1.0 };
        let t = r.cluster_utc;
        // the solved altitude is too poor to publish: only one heard in the last 5 s
        let altitude = match (r.last_altitude_time, r.altitude) {
            (Some(lt), Some(a)) if lt != 0.0 && t - lt < 5.0 => a.to_string(),
            _ => String::new(),
        };
        let vrate = match (r.vrate_time, r.vrate) {
            (Some(vt), Some(v)) if vt != 0.0 && t - vt < 5.0 => v.to_string(),
            _ => String::new(),
        };
        let callsign = csv_quote(r.callsign.as_deref());
        let squawk = csv_quote(r.squawk.as_deref());
        let receivers = csv_quote(Some(&users.join(",")));
        let head = format!("{t:.3},{:06X},{callsign},{squawk},{lat:.4},{lon:.4},{altitude},{err_est:.0},{},{},{receivers},{}", r.address, r.receivers.len(), r.distinct, r.dof);
        let k = &r.kalman;
        let line = match (k.valid && k.last_update >= t, k.position_llh, k.heading, k.ground_speed, k.position_error) {
            (true, Some(p), Some(h), Some(gs), Some(pe)) => {
                format!("{head},{:.4},{:.4},{:.0},{h:.0},{:.0},{vrate},{pe:.0}", p[0], p[1], p[2] * MTOF, gs * MS_TO_KTS)
            }
            _ => format!("{head},{vrate}"),
        };
        self.out.lock().unwrap().write_line(&line);
    }
}

/// `--dump-pseudorange`: one JSON line per result with the solver's inputs.
pub struct Pseudorange {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Pseudorange {
    pub fn new(path: &str) -> std::io::Result<Pseudorange> {
        let p = Pseudorange { path: PathBuf::from(path), file: Mutex::new(None) };
        p.reopen()?;
        Ok(p)
    }

    /// On SIGHUP, so the file can be rotated from outside.
    pub fn reopen(&self) -> std::io::Result<()> {
        *self.file.lock().unwrap() = Some(OpenOptions::new().create(true).append(true).open(&self.path)?);
        Ok(())
    }

    pub fn write(&self, r: &MlatResult) {
        let r0 = |x: f64| json!(x.round_ties_even());
        let t0 = r.cluster_meas.first().map_or(0.0, |m| m.timestamp);
        let cluster: Vec<Value> = r
            .cluster_meas
            .iter()
            .map(|m| json!([r0(m.pos[0]), r0(m.pos[1]), r0(m.pos[2]), round_dec((m.timestamp - t0) * 1e6, 1), round_dec(m.variance * 1e12, 2)]))
            .collect();
        let mut s = Map::new();
        s.insert("icao".into(), json!(format!("{:06x}", r.address)));
        s.insert("time".into(), json!(round_dec(r.cluster_utc, 3)));
        s.insert("ecef".into(), json!([r0(r.ecef[0]), r0(r.ecef[1]), r0(r.ecef[2])]));
        s.insert("distinct".into(), json!(r.distinct));
        s.insert("dof".into(), json!(r.dof));
        s.insert("cluster".into(), Value::Array(cluster));
        let c = &r.ecef_cov;
        s.insert("ecef_cov".into(), Value::Array(c.iter().flatten().map(|v| r0(*v)).collect()));
        if let (Some(a), Some(e)) = (r.solve_altitude, r.solve_altitude_error) {
            s.insert("altitude".into(), r0(a));
            s.insert("altitude_error".into(), r0(e));
        }
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            if let Err(e) = writeln!(f, "{}", Value::Object(s)) {
                log("coordinator", &format!("pseudorange write failed: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(csv_quote(None), "");
        assert_eq!(csv_quote(Some("BAW1")), "BAW1");
        assert_eq!(csv_quote(Some("a,b")), "\"a,b\"");
        assert_eq!(csv_quote(Some("say \"hi\"")), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn rotation_keeps_backups() {
        let dir = std::env::temp_dir().join(format!("mlat-rot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pos.csv");
        let mut r = Rotating::open(path.clone()).unwrap();
        let line = "x".repeat(1000);
        for _ in 0..3000 {
            r.write_line(&line);
        }
        assert!(dir.join("pos.csv.1").exists() && dir.join("pos.csv.2").exists());
        assert!(std::fs::metadata(&path).unwrap().len() < Rotating::MAX);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
