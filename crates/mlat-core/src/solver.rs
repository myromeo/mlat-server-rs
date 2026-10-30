//! Position solver: least-squares multilateration of one set of receive
//! timestamps, ported from mlat/solver.py (scipy `leastsq` underneath).

use crate::geodesy::{ecef2llh, ecef_distance, llh2ecef};
use crate::minpack::{lmdif, LmResult};
use crate::{C_AIR, FTOM};

pub const MIN_ALT: f64 = -1500.0 * FTOM;
pub const MAX_ALT: f64 = 75000.0 * FTOM;
pub const SOLVER_MAXFEV: i32 = 165;

/// One receiver's view of the transmission.
#[derive(Clone, Copy, Debug)]
pub struct Measurement {
    /// Receiver position, ECEF metres.
    pub pos: [f64; 3],
    /// Normalised receive time, seconds.
    pub timestamp: f64,
    /// Estimated variance of the timestamp.
    pub variance: f64,
}

/// Raw output of the least-squares fit, as scipy's `leastsq` reports it.
pub struct Fit {
    pub x: [f64; 4],
    pub cov: Option<[[f64; 4]; 4]>,
    pub nfev: i32,
    pub info: i32,
}

/// The residuals minimised by the solver (`_residuals` in solver.py).
pub fn residuals(x: &[f64], pseudorange_data: &[([f64; 3], f64, f64)], altitude: Option<(f64, f64)>, out: &mut [f64]) {
    let position = [x[0], x[1], x[2]];
    let offset = x[3];
    let mut k = 0;
    for (rpos, pseudorange, error) in pseudorange_data {
        let guess = ecef_distance(rpos, &position) - offset;
        out[k] = (pseudorange - guess) / error;
        k += 1;
    }
    if let Some((alt, alt_err)) = altitude {
        let alt_guess = ecef2llh(&position)[2];
        out[k] = (alt - alt_guess) / alt_err;
    }
}

/// Inverse of an upper-triangular matrix (LAPACK dtrti2, upper, non-unit).
fn invert_upper(r: &[[f64; 4]; 4]) -> Option<[[f64; 4]; 4]> {
    let n = 4;
    let mut a = *r;
    if (0..n).any(|i| a[i][i] == 0.0) {
        return None;
    }
    for j in 0..n {
        a[j][j] = 1.0 / a[j][j];
        let ajj = -a[j][j];
        for jj in 0..j {
            if a[jj][j] != 0.0 {
                let temp = a[jj][j];
                for i in 0..jj {
                    a[i][j] += temp * a[i][jj];
                }
                a[jj][j] *= a[jj][jj];
            }
        }
        for i in 0..j {
            a[i][j] *= ajj;
        }
    }
    Some(a)
}

/// `cov_x` the way scipy builds it from the final QR factorisation.
fn covariance(res: &LmResult) -> Option<[[f64; 4]; 4]> {
    let n = 4;
    let mut r = [[0.0; 4]; 4];
    for j in 0..n {
        for i in 0..=j {
            r[i][j] = res.fjac[i + j * res.m];
        }
    }
    let inv = invert_upper(&r)?;
    // invR[perm] = invR.copy()
    let mut p = [[0.0; 4]; 4];
    for i in 0..n {
        p[res.ipvt[i]] = inv[i];
    }
    let mut cov = [[0.0; 4]; 4];
    for i in 0..n {
        for j in 0..n {
            let mut s = 0.0;
            for k in 0..n {
                s += p[i][k] * p[j][k];
            }
            cov[i][j] = s;
        }
    }
    Some(cov)
}

/// `scipy.optimize.leastsq(..., full_output=True, maxfev=...)` on the
/// pseudorange residuals, with scipy's default tolerances.
pub fn fit(x0: [f64; 4], pseudorange_data: &[([f64; 3], f64, f64)], altitude: Option<(f64, f64)>, maxfev: i32) -> Fit {
    let m = pseudorange_data.len() + altitude.is_some() as usize;
    let res = lmdif(
        |x, out| residuals(x, pseudorange_data, altitude, out),
        m,
        &x0,
        1.49012e-8,
        1.49012e-8,
        0.0,
        maxfev,
        f64::EPSILON,
        100.0,
    );
    let cov = if matches!(res.info, 1..=4) { covariance(&res) } else { None };
    Fit { x: [res.x[0], res.x[1], res.x[2], res.x[3]], cov, nfev: res.nfev, info: res.info }
}

/// Multilaterates a transmitter position. `altitude` is (altitude, error) in
/// metres. Returns the ECEF position and, if available, its covariance, or
/// None when the solver fails or the answer is implausible.
pub fn solve(measurements: &[Measurement], altitude: Option<(f64, f64)>, initial_guess: [f64; 3]) -> Option<([f64; 3], Option<[[f64; 3]; 3]>)> {
    if measurements.len() + (altitude.is_some() as usize) < 4 {
        return None; // the Python server raises here
    }

    let mut guess = initial_guess;
    let [glat, glng, galt] = ecef2llh(&guess);
    if galt < MIN_ALT {
        guess = llh2ecef(glat, glng, MIN_ALT);
    }
    if galt > MAX_ALT {
        guess = llh2ecef(glat, glng, MAX_ALT);
    }

    let base = measurements[0].timestamp;
    let data: Vec<([f64; 3], f64, f64)> = measurements
        .iter()
        .map(|m| (m.pos, (m.timestamp - base) * C_AIR, m.variance.sqrt() * C_AIR))
        .collect();

    let f = fit([guess[0], guess[1], guess[2], 0.0], &data, altitude, SOLVER_MAXFEV);
    if !matches!(f.info, 1..=4) {
        return None;
    }
    let offset = f.x[3];
    if !(0.0..=500e3).contains(&offset) {
        return None;
    }
    let pos = [f.x[0], f.x[1], f.x[2]];
    let cov = f.cov.map(|c| {
        let mut o = [[0.0; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                o[i][j] = c[i][j];
            }
        }
        o
    });
    Some((pos, cov))
}
