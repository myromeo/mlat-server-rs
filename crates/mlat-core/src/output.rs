//! Result output formats: the Basestation (SBS) line that readsb reads,
//! ported from BasestationClient.write_result in mlat/output.py.

use crate::geodesy::ecef2llh;

/// Metres per second to knots / feet per minute, as in mlat/constants.py.
pub const MS_TO_KTS: f64 = 1.9438;
pub const MS_TO_FPM: f64 = (1.0 / crate::FTOM) * 60.0;

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY/MM/DD` in UTC.
pub fn format_date(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}/{m:02}/{d:02}")
}

/// `HH:MM:SS.mmm` in UTC. The milliseconds are rounded from the fraction, so a
/// fraction of 0.9996 prints as `.1000`, exactly as the Python server does.
pub fn format_time(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let sod = secs.rem_euclid(86_400);
    let frac = ts - ts.trunc(); // math.modf(ts)[0]
    format!("{:02}:{:02}:{:02}.{:03.0}", sod / 3600, (sod / 60) % 60, sod % 60, frac * 1000.0)
}

fn csv_quote(s: &Option<String>) -> String {
    match s {
        None => String::new(),
        Some(s) if !s.contains('\n') && !s.contains('"') && !s.contains(',') => s.clone(),
        Some(s) => format!("\"{}\"", s.replace('"', "\"\"")),
    }
}

/// Python's `repr(round(x, 6))`.
fn round6(x: f64) -> String {
    let r: f64 = format!("{x:.6}").parse().unwrap_or(x);
    format!("{r:?}")
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

/// The part of the Kalman track the output formats read.
#[derive(Clone, Copy, Debug)]
pub struct KalmanView {
    pub valid: bool,
    pub last_update: f64,
    pub ground_speed: Option<f64>,
    pub heading: Option<f64>,
}

/// What the SBS formatter needs from the aircraft.
pub struct SbsAircraft<'a> {
    pub callsign: &'a Option<String>,
    pub squawk: &'a Option<String>,
    pub altitude: Option<i32>,
    pub last_altitude_time: Option<f64>,
    pub vrate: Option<i32>,
    pub vrate_time: Option<f64>,
    /// Updated by the formatter, as the Python server keeps it on the aircraft.
    pub last_crappy_output: f64,
}

/// One Basestation line for a result, or None when the output is suppressed.
#[allow(clippy::too_many_arguments)]
pub fn sbs_line(
    receive_timestamp: f64,
    address: u32,
    ecef: &[f64; 3],
    n_receivers: usize,
    dof: i32,
    kalman: &KalmanView,
    error: Option<i64>,
    ac: &mut SbsAircraft,
    use_kalman_data: bool,
    send_timestamp: f64,
) -> Option<String> {
    let mut speed = String::new();
    let mut heading = String::new();

    let [lat, lon, _alt] = ecef2llh(ecef);

    if use_kalman_data {
        if !kalman.valid && dof < 1 {
            if receive_timestamp - ac.last_crappy_output > 60.0 {
                // ignore the first crappy output
                ac.last_crappy_output = receive_timestamp - 1.0;
                return None;
            }
            if receive_timestamp - ac.last_crappy_output > 30.0 || receive_timestamp == ac.last_crappy_output {
                ac.last_crappy_output = receive_timestamp;
            } else {
                return None;
            }
        }
        // positions always come from the least-squares fit; only speed and
        // heading are taken from the filter
        if kalman.valid && kalman.last_update >= receive_timestamp {
            if let (Some(gs), Some(h)) = (kalman.ground_speed, kalman.heading) {
                speed = ((gs * MS_TO_KTS).round_ties_even() as i64).to_string();
                heading = (h.round_ties_even() as i64).to_string();
            }
        }
    }

    // MLAT altitude is never used: it is too inaccurate to be useful
    let altitude = match (ac.last_altitude_time, ac.altitude) {
        (Some(t), Some(a)) if t != 0.0 && receive_timestamp - t < 5.0 => a.to_string(),
        _ => String::new(),
    };
    let vrate = match (ac.vrate_time, ac.vrate) {
        (Some(t), Some(v)) if t != 0.0 && receive_timestamp - t < 5.0 => v.to_string(),
        _ => String::new(),
    };

    Some(format!(
        "MSG,3,1,1,{:06X},1,{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        address,
        format_date(receive_timestamp),
        format_time(receive_timestamp),
        format_date(send_timestamp),
        format_time(send_timestamp),
        csv_quote(ac.callsign),
        altitude,
        speed,
        heading,
        round6(lat),
        round6(lon),
        vrate,
        csv_quote(ac.squawk),
        n_receivers,
        opt(error),
        "",
        ""
    ))
}
