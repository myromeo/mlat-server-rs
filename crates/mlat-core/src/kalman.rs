//! Kalman tracking filter that smooths multilateration results and derives
//! speed and heading: a port of mlat/kalman.py on top of pykalman's unscented
//! transform (additive-noise form, alpha = 1, beta = 0, kappa = 3 - n).

use crate::geodesy::ecef2llh;
use crate::linalg::{cholesky_lower, inverse, pinv_symmetric, Mat};
use crate::solver::Measurement;
use crate::C_AIR;

const MIN_ACQUIRING_DOF: f64 = 1.0;
const MIN_TRACKING_DOF: f64 = 0.0;
const OUTLIER_MAHALANOBIS_DISTANCE: f64 = 15.0;
const MIN_ACQUIRING_POSITION_ERROR: f64 = 3e3;
const MIN_ACQUIRING_VELOCITY_ERROR: f64 = 50.0;
const MAX_TRACKING_POSITION_ERROR: f64 = 5e3;
const MAX_TRACKING_VELOCITY_ERROR: f64 = 75.0;
const PROCESS_NOISE: f64 = 0.10;
const DTOR: f64 = std::f64::consts::PI / 180.0;

/// Motion model: constant velocity (6 states) or constant acceleration (9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    ConstantVelocity,
    ConstantAcceleration,
}

impl Model {
    fn dim(self) -> usize {
        match self {
            Model::ConstantVelocity => 6,
            Model::ConstantAcceleration => 9,
        }
    }

    fn transition(self, s: &[f64], dt: f64) -> Vec<f64> {
        match self {
            Model::ConstantVelocity => vec![
                s[0] + s[3] * dt,
                s[1] + s[4] * dt,
                s[2] + s[5] * dt,
                s[3],
                s[4],
                s[5],
            ],
            Model::ConstantAcceleration => {
                let dt2 = dt.powf(2.0);
                vec![
                    s[0] + s[3] * dt + 0.5 * s[6] * dt2,
                    s[1] + s[4] * dt + 0.5 * s[7] * dt2,
                    s[2] + s[5] * dt + 0.5 * s[8] * dt2,
                    s[3] + s[6] * dt,
                    s[4] + s[7] * dt,
                    s[5] + s[8] * dt,
                    s[6],
                    s[7],
                    s[8],
                ]
            }
        }
    }

    fn transition_covariance(self, dt: f64) -> Mat {
        let n = self.dim();
        let mut t = Mat::zeros(n, n);
        let sym = |t: &mut Mat, i: usize, j: usize, v: f64| {
            t.set(i, j, v);
            t.set(j, i, v);
        };
        for i in 0..3 {
            t.set(i, i, 0.25 * dt.powf(4.0));
            t.set(i + 3, i + 3, dt.powf(2.0));
            sym(&mut t, i, i + 3, 0.5 * dt.powf(3.0));
        }
        if self == Model::ConstantAcceleration {
            for i in 0..3 {
                t.set(i + 6, i + 6, 1.0);
                sym(&mut t, i, i + 6, 0.5 * dt.powf(2.0));
                sym(&mut t, i + 3, i + 6, dt);
            }
        }
        let q = PROCESS_NOISE.powf(2.0);
        Mat { r: n, c: n, d: t.d.iter().map(|v| v * q * dt).collect() }
    }

    fn initial_state(self, pos: [f64; 3], cov: &[[f64; 3]; 3]) -> (Vec<f64>, Mat) {
        let n = self.dim();
        let mut mean = vec![0.0; n];
        mean[..3].copy_from_slice(&pos);
        let mut c = Mat::zeros(n, n);
        if self == Model::ConstantVelocity {
            for i in 0..3 {
                for j in 0..3 {
                    c.set(i, j, cov[i][j] * 4.0);
                }
            }
            for i in 3..6 {
                c.set(i, i, 200.0_f64.powf(2.0));
            }
        } else {
            for i in 0..3 {
                for j in 0..3 {
                    c.set(i, j, cov[i][j] * 4.0);
                }
            }
            for i in 3..6 {
                c.set(i, i, 200.0_f64.powf(2.0));
            }
            for i in 6..9 {
                c.set(i, i, 1.0);
            }
        }
        (mean, c)
    }
}

struct SigmaPoints {
    /// One point per row.
    points: Mat,
    weights_mean: Vec<f64>,
    weights_cov: Vec<f64>,
}

/// `moments2points` with alpha = 1, beta = 0, kappa = 3 - n.
fn moments_to_points(mu: &[f64], sigma: &Mat) -> Option<SigmaPoints> {
    let n = mu.len();
    if !sigma.is_finite() {
        return None;
    }
    let l = cholesky_lower(sigma)?;
    let (alpha, beta, kappa) = (1.0_f64, 0.0_f64, 3.0 - n as f64);
    let lamda = (alpha * alpha) * (n as f64 + kappa) - n as f64;
    let c = n as f64 + lamda;
    let sc = c.sqrt();

    let mut points = Mat::zeros(2 * n + 1, n);
    for i in 0..2 * n + 1 {
        for j in 0..n {
            points.set(i, j, mu[j]);
        }
    }
    for col in 0..n {
        for row in 0..n {
            let delta = l.at(row, col) * sc;
            points.d[(1 + col) * n + row] += delta;
            points.d[(n + 1 + col) * n + row] -= delta;
        }
    }
    let mut wm = vec![0.5 / c; 2 * n + 1];
    wm[0] = lamda / c;
    let mut wc = wm.clone();
    wc[0] = lamda / c + (1.0 - alpha * alpha + beta);
    Some(SigmaPoints { points, weights_mean: wm, weights_cov: wc })
}

/// `unscented_transform` with additive noise: returns the transformed points
/// and their mean and covariance.
fn transform(sp: &SigmaPoints, f: &dyn Fn(&[f64]) -> Vec<f64>, noise: &Mat) -> (Mat, Vec<f64>, Mat) {
    let np = sp.points.r;
    let rows: Vec<Vec<f64>> = (0..np).map(|i| f(&sp.points.d[i * sp.points.c..(i + 1) * sp.points.c])).collect();
    let pts = Mat::from_rows(&rows);
    let dim = pts.c;

    let mut mu = vec![0.0; dim];
    for j in 0..dim {
        for i in 0..np {
            mu[j] += pts.at(i, j) * sp.weights_mean[i];
        }
    }
    // sigma = diff * diag(w) * diff^T, diff = points^T - mu
    let mut sigma = Mat::zeros(dim, dim);
    for a in 0..dim {
        for b in 0..dim {
            let mut s = 0.0;
            for i in 0..np {
                s += ((pts.at(i, a) - mu[a]) * sp.weights_cov[i]) * (pts.at(i, b) - mu[b]);
            }
            sigma.set(a, b, s);
        }
    }
    (pts, mu, sigma.add(noise))
}

#[derive(Clone, Debug)]
pub struct KalmanState {
    pub icao: u32,
    model: Model,
    mean: Option<Vec<f64>>,
    cov: Option<Mat>,
    acquiring: bool,
    outliers: i32,
    pub last_update: f64,
    pub valid: bool,
    pub position: Option<[f64; 3]>,
    pub velocity: Option<[f64; 3]>,
    pub position_error: Option<f64>,
    pub velocity_error: Option<f64>,
    pub position_llh: Option<[f64; 3]>,
    pub heading: Option<f64>,
    pub ground_speed: Option<f64>,
    pub vertical_speed: Option<f64>,
}

impl KalmanState {
    pub fn new(icao: u32, model: Model) -> KalmanState {
        KalmanState {
            icao,
            model,
            mean: None,
            cov: None,
            acquiring: true,
            outliers: 0,
            last_update: 0.0,
            valid: false,
            position: None,
            velocity: None,
            position_error: None,
            velocity_error: None,
            position_llh: None,
            heading: None,
            ground_speed: None,
            vertical_speed: None,
        }
    }

    fn reset(&mut self) {
        *self = KalmanState::new(self.icao, self.model);
    }

    /// Puts the filter into a given internal state (used to test one update at a time).
    pub fn restore(&mut self, mean: Option<Vec<f64>>, cov: Option<Mat>, acquiring: bool, outliers: i32, last_update: f64, valid: bool) {
        self.mean = mean;
        self.cov = cov;
        self.acquiring = acquiring;
        self.outliers = outliers;
        self.last_update = last_update;
        self.valid = valid;
    }

    pub fn mean(&self) -> Option<&[f64]> {
        self.mean.as_deref()
    }

    pub fn cov(&self) -> Option<&Mat> {
        self.cov.as_ref()
    }

    pub fn acquiring(&self) -> bool {
        self.acquiring
    }

    pub fn outliers(&self) -> i32 {
        self.outliers
    }

    fn update_derived(&mut self) {
        let mean = self.mean.as_ref().unwrap();
        let cov = self.cov.as_ref().unwrap();
        let position = [mean[0], mean[1], mean[2]];
        let velocity = [mean[3], mean[4], mean[5]];
        self.position = Some(position);
        self.velocity = Some(velocity);

        let pe = cov.at(0, 0) + cov.at(1, 1) + cov.at(2, 2);
        self.position_error = Some(if pe < 0.0 { 1e6 } else { pe.sqrt() });
        let ve = cov.at(3, 3) + cov.at(4, 4) + cov.at(5, 5);
        self.velocity_error = Some(if ve < 0.0 { 1e6 } else { ve.sqrt() });

        let llh = ecef2llh(&position);
        self.position_llh = Some(llh);
        let lat_r = llh[0] * DTOR;
        let lon_r = llh[1] * DTOR;
        let c = [
            [-lon_r.sin(), lon_r.cos(), 0.0],
            [(-lat_r).sin() * lon_r.cos(), (-lat_r).sin() * lon_r.sin(), (-lat_r).cos()],
            [(-lat_r).cos() * lon_r.cos(), (-lat_r).cos() * lon_r.sin(), -(-lat_r).sin()],
        ];
        let enu: Vec<f64> = c.iter().map(|row| row[0] * velocity[0] + row[1] * velocity[1] + row[2] * velocity[2]).collect();
        let (east, north, up) = (enu[0], enu[1], enu[2]);
        let mut heading = east.atan2(north) * 180.0 / std::f64::consts::PI;
        if heading < 0.0 {
            heading += 360.0;
        }
        self.heading = Some(heading);
        self.ground_speed = Some((north.powf(2.0) + east.powf(2.0)).sqrt());
        self.vertical_speed = Some(up);
        self.valid = true;
    }

    /// Feeds one multilateration result into the filter. Returns whether the
    /// filter is locked on (the track is valid).
    ///
    /// `altitude`/`altitude_error` are metres; `ls_cov` is the least-squares
    /// position covariance; `dof` the solution's degrees of freedom.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        position_time: f64,
        measurements: &[Measurement],
        altitude: Option<f64>,
        altitude_error: Option<f64>,
        ls_pos: [f64; 3],
        ls_cov: &[[f64; 3]; 3],
        dof: f64,
    ) -> bool {
        let dt = position_time - self.last_update;
        if dt < 0.0 {
            return false;
        }
        if dt > 300.0 {
            self.reset();
        }
        if self.acquiring && dof < MIN_ACQUIRING_DOF {
            return false;
        }
        if self.mean.is_none() {
            self.last_update = position_time;
            let (m, c) = self.model.initial_state(ls_pos, ls_cov);
            self.mean = Some(m);
            self.cov = Some(c);
            return false;
        }
        if dof < MIN_TRACKING_DOF {
            return false;
        }

        let zero_pr = measurements[0].timestamp * C_AIR;
        let n = measurements.len();
        let positions: Vec<[f64; 3]> = measurements.iter().map(|m| m.pos).collect();
        let with_alt = altitude.is_some();
        let nobs = if with_alt { n } else { n - 1 };
        let mut obs = vec![0.0; nobs];
        let mut obs_var = vec![0.0; nobs];
        let off = if with_alt { 1 } else { 0 };
        if let Some(a) = altitude {
            obs[0] = a;
            obs_var[0] = altitude_error.unwrap().powf(2.0);
        }
        for i in 1..n {
            obs[i - 1 + off] = measurements[i].timestamp * C_AIR - zero_pr;
            obs_var[i - 1 + off] = (measurements[i].variance + measurements[0].variance) * C_AIR.powf(2.0);
        }
        let mut obs_covar = Mat::zeros(nobs, nobs);
        for i in 0..nobs {
            obs_covar.set(i, i, obs_var[i]);
        }

        let model = self.model;
        let observation = |s: &[f64]| -> Vec<f64> {
            let (x, y, z) = (s[0], s[1], s[2]);
            let range = |p: &[f64; 3]| ((p[0] - x).powf(2.0) + (p[1] - y).powf(2.0) + (p[2] - z).powf(2.0)).powf(0.5);
            let zero_range = range(&positions[0]);
            let mut o = Vec::with_capacity(nobs);
            if with_alt {
                o.push(ecef2llh(&[x, y, z])[2]);
            }
            for p in &positions[1..] {
                o.push(range(p) - zero_range);
            }
            o
        };

        // Any failure inside the filter (not positive definite, singular, ...)
        // resets the track, as the exception handler in the Python does.
        match self.try_update(dt, model, &obs, &obs_covar, &observation, position_time) {
            Some(result) => result,
            None => {
                self.reset();
                false
            }
        }
    }

    fn try_update(
        &mut self,
        dt: f64,
        model: Model,
        obs: &[f64],
        obs_covar: &Mat,
        observation: &dyn Fn(&[f64]) -> Vec<f64>,
        position_time: f64,
    ) -> Option<bool> {
        let mean = self.mean.clone().unwrap();
        let cov = self.cov.clone().unwrap();
        let trans_covar = model.transition_covariance(dt);

        let sp_state = moments_to_points(&mean, &cov)?;
        let (_, mu_pred, sigma_pred) = transform(&sp_state, &|s| model.transition(s, dt), &trans_covar);
        let pts_pred = moments_to_points(&mu_pred, &sigma_pred)?;
        let (obs_pts, obs_mu, obs_sigma) = transform(&pts_pred, observation, obs_covar);

        let innovation: Vec<f64> = obs.iter().zip(&obs_mu).map(|(a, b)| a - b).collect();
        let vi = inverse(&obs_sigma)?;
        // numpy.dot(numpy.dot(innovation.T, vi), innovation)
        let row: Vec<f64> = (0..vi.c).map(|j| (0..vi.r).map(|i| innovation[i] * vi.at(i, j)).sum()).collect();
        let md2: f64 = row.iter().zip(&innovation).map(|(a, b)| a * b).sum();
        if !(md2 >= 0.0) {
            return None; // math.sqrt of a negative number raises
        }
        let md = md2.sqrt();
        if md > OUTLIER_MAHALANOBIS_DISTANCE {
            self.outliers += 1;
            if self.outliers < 3 || (position_time - self.last_update) < 15.0 {
                return Some(false);
            }
            self.reset();
            return Some(false);
        }
        self.outliers = 0;

        // correct
        let np = pts_pred.points.r;
        let n = mu_pred.len();
        let m = obs_mu.len();
        let mut cross = Mat::zeros(n, m);
        for a in 0..n {
            for b in 0..m {
                let mut s = 0.0;
                for i in 0..np {
                    s += ((pts_pred.points.at(i, a) - mu_pred[a]) * pts_pred.weights_mean[i]) * (obs_pts.at(i, b) - obs_mu[b]);
                }
                cross.set(a, b, s);
            }
        }
        let gain = cross.matmul(&pinv_symmetric(&obs_sigma));
        let corr = gain.matvec(&innovation);
        let new_mean: Vec<f64> = mu_pred.iter().zip(&corr).map(|(a, b)| a + b).collect();
        let new_cov = sigma_pred.sub(&gain.matmul(&cross.t()));
        self.mean = Some(new_mean);
        self.cov = Some(new_cov);

        self.last_update = position_time;
        self.update_derived();

        let pe = self.position_error.unwrap();
        let ve = self.velocity_error.unwrap();
        if self.acquiring && pe < MIN_ACQUIRING_POSITION_ERROR && ve < MIN_ACQUIRING_VELOCITY_ERROR {
            self.acquiring = false;
        } else if !self.acquiring && (pe > MAX_TRACKING_POSITION_ERROR || ve > MAX_TRACKING_VELOCITY_ERROR) {
            self.acquiring = true;
        }
        self.valid = !self.acquiring;
        Some(self.valid)
    }
}
