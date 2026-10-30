//! Relative clock model of one pair of receivers, ported from ClockPairing in
//! clocktrack.pyx. Floating-point operations are kept in the original order so
//! results match the Python server.

use crate::atomic::AtomicF64;
use crate::normalize::PairParams;
use crate::receiver::Receiver;
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::Arc;

const CP_SIZE: usize = 32;
const DRIFT_N_STABLE: i32 = 12;

/// Per-aircraft values that pairing updates write and other stages read.
#[derive(Debug, Default)]
pub struct AcShared {
    pub sync_good: AtomicF64,
    pub sync_bad: AtomicF64,
    pub last_adsb_time: AtomicF64,
    pub last_altitude_time: AtomicF64,
    pub altitude: AtomicI32,
    /// Set by the 15 s health evaluation when this aircraft's sync data is unreliable.
    pub sync_dont_use: AtomicBool,
}

/// Python's `round(x, ndigits)`: the nearest double to the correctly rounded decimal.
pub fn round_dec(x: f64, ndigits: usize) -> f64 {
    format!("{x:.ndigits$}").parse().unwrap_or(x)
}

/// What the health evaluation reads from one pairing.
#[derive(Clone, Copy, Debug)]
pub struct PairHealth {
    pub n: i32,
    pub error_us: f64,
    pub jumped: bool,
    pub outlier_percent: f64,
    pub age: i64,
    pub updated: f64,
    /// `round(drift * 1e6)` from the base side, and from the peer side.
    pub drift_ppm: i64,
    pub i_drift_ppm: i64,
}

/// Receivers whose whole clock sync must be reset after an update.
#[derive(Debug, Default)]
pub struct Resets(pub [Option<u32>; 2]);

pub struct ClockPairing {
    base: Arc<Receiver>,
    peer: Arc<Receiver>,
    pub cat: usize,

    pub valid: bool,
    pub updated: f64,
    pub variance: f64,
    pub n: i32,
    pub update_attempted: f64,

    pub factor: f64,
    pub i_factor: f64,
    pub base_avg: f64,
    pub peer_avg: f64,

    base_freq: f64,
    peer_freq: f64,
    raw_drift: f64,
    drift: f64,
    i_drift: f64,

    drift_n: i32,
    drift_outliers: i32,
    outlier_reset_cooldown: i32,
    outlier_total: f64,
    update_total: f64,
    ts_base: [f64; CP_SIZE],
    ts_peer: [f64; CP_SIZE],
    var: [f64; CP_SIZE],
    var_sum: f64,
    outliers: i32,
    cumulative_error: f64,
    error: f64,
    pub jumped: i32,

    relative_freq: f64,
    i_relative_freq: f64,
    drift_max: f64,
    drift_max_delta: f64,
    outlier_threshold: f64,
    update_last_sync: f64,
}

impl ClockPairing {
    pub fn new(base: Arc<Receiver>, peer: Arc<Receiver>, cat: usize) -> ClockPairing {
        let relative_freq = peer.clock.freq / base.clock.freq;
        let i_relative_freq = base.clock.freq / peer.clock.freq;
        let drift_max = 0.75 * (base.clock.max_freq_error + peer.clock.max_freq_error);
        ClockPairing {
            base_freq: base.clock.freq,
            peer_freq: peer.clock.freq,
            base,
            peer,
            cat,
            valid: false,
            updated: 0.0,
            variance: -1e-6,
            n: 0,
            update_attempted: 0.0,
            factor: 0.0,
            i_factor: 0.0,
            base_avg: 0.0,
            peer_avg: 0.0,
            raw_drift: 0.0,
            drift: 0.0,
            i_drift: 0.0,
            drift_n: 0,
            drift_outliers: 0,
            outlier_reset_cooldown: 5,
            outlier_total: 0.0,
            update_total: 1e-3,
            ts_base: [0.0; CP_SIZE],
            ts_peer: [0.0; CP_SIZE],
            var: [0.0; CP_SIZE],
            var_sum: 0.0,
            outliers: 0,
            cumulative_error: 0.0,
            error: -1e-6,
            jumped: 0,
            relative_freq,
            i_relative_freq,
            drift_max,
            drift_max_delta: drift_max / 20.0,
            outlier_threshold: 1.5 * 1e-6,
            update_last_sync: 0.0,
        }
    }

    /// The model parameters the normaliser reads.
    pub fn params(&self) -> PairParams {
        PairParams {
            valid: self.valid,
            variance: self.variance,
            updated: self.updated,
            n: self.n,
            factor: self.factor,
            i_factor: self.i_factor,
            base_avg: self.base_avg,
            peer_avg: self.peer_avg,
        }
    }

    pub fn base(&self) -> &Arc<Receiver> {
        &self.base
    }

    pub fn peer(&self) -> &Arc<Receiver> {
        &self.peer
    }

    pub fn involves(&self, uid: u32) -> bool {
        self.base.uid == uid || self.peer.uid == uid
    }

    pub fn check_valid(&mut self, now: f64) -> bool {
        if self.n < 2 || self.drift_n < 2 || self.outlier_total / self.update_total > 0.5 {
            self.variance = -1e-6;
            self.error = -1e-6;
            self.valid = false;
            return false;
        }

        self.variance = self.var_sum / self.n as f64;
        self.error = self.variance.sqrt();

        self.valid = self.outlier_reset_cooldown < 1
            && self.n > 4
            && self.drift_n > 4
            && self.variance < 16e-12
            && now - self.updated < 35.0;

        if self.valid && now > self.update_last_sync {
            self.update_last_sync = now + 5.0;
            self.base.last_sync.store(now);
            self.peer.last_sync.store(now);
        }
        self.valid
    }

    /// One pairing's contribution to the 15 s health evaluation
    /// (`dump_receiver_state` in clocktrack.pyx). Halves the outlier counters
    /// and clears the jump flag as a side effect; None while the pairing has
    /// fewer than two points.
    pub fn dump_state(&mut self, now: f64) -> Option<PairHealth> {
        if self.n < 2 {
            return None;
        }
        let outlier_percent = if self.update_total < 4.0 {
            50.0 * self.outlier_total / self.update_total
        } else {
            100.0 * self.outlier_total / self.update_total
        };
        self.outlier_total /= 2.0;
        self.update_total /= 2.0;
        let health = PairHealth {
            n: self.n,
            error_us: round_dec(self.error * 1e6, 1),
            jumped: self.jumped != 0,
            outlier_percent: round_dec(outlier_percent, 1),
            age: (now - self.updated).round_ties_even() as i64,
            updated: self.updated,
            drift_ppm: (self.drift * 1e6).round_ties_even() as i64,
            i_drift_ppm: (self.i_drift * 1e6).round_ties_even() as i64,
        };
        self.jumped = 0;
        Some(health)
    }

    pub fn reset_offsets(&mut self) {
        self.valid = false;
        self.n = 0;
        self.var_sum = 0.0;
        self.error = -1e-6;
        self.variance = -1e-6;
        self.outliers = 0;
        self.cumulative_error = 0.0;
        self.base_avg = 0.0;
        self.peer_avg = 0.0;
    }

    pub fn predict_peer(&self, base_ts: f64) -> f64 {
        self.peer_avg + (base_ts - self.base_avg) * self.factor
    }

    fn prune_old_data(&mut self, now: f64) {
        if self.n == 0 {
            return;
        }
        let n = self.n as usize;
        let new_max = CP_SIZE - 12;
        let mut i = if n > new_max { n - new_max } else { 0 };

        let latest_base_ts = self.ts_base[n - 1];
        let limit = 45.0 * self.base_freq;
        while i < n && (latest_base_ts - self.ts_base[i]) > limit {
            i += 1;
        }

        if i > 0 {
            let m = n - i;
            self.n = m as i32;
            self.ts_base.copy_within(i..i + m, 0);
            self.ts_peer.copy_within(i..i + m, 0);
            self.var.copy_within(i..i + m, 0);
            self.var_sum = 0.0;
            for k in 0..m {
                self.var_sum += self.var[k];
            }
            self.check_valid(now);
        }
    }

    fn update_drift(&mut self, base_interval: f64, peer_interval: f64) -> bool {
        let adjusted_base_interval = base_interval * self.relative_freq;
        let new_drift = (peer_interval - adjusted_base_interval) / adjusted_base_interval;

        if new_drift.abs() > self.drift_max {
            return false;
        }

        if self.drift_n <= 0 || self.drift_outliers > 15 {
            self.raw_drift = new_drift;
            self.drift = new_drift;
            self.i_drift = -1.0 * self.drift / (1.0 + self.drift);
            self.cumulative_error = 0.0;
            self.drift_outliers = 0;
            self.drift_n = 1;
            return true;
        }

        let drift_error = new_drift - self.raw_drift;
        if drift_error.abs() > self.drift_max_delta {
            self.drift_outliers += 1;
            return false;
        }

        self.drift_outliers = (self.drift_outliers - 2).max(0);

        let mut kp = 0.06;
        let ki = 0.008;

        let unstable = DRIFT_N_STABLE - self.drift_n;
        if unstable > 0 {
            // Integer division, exactly as the Cython build computes it:
            // `unstable / drift_n_stable` is 0 for every unstable < 12, so the
            // intended fast-adjustment boost never applies in the Python server.
            let adjustment_factor = 1.0 + (0.2 / kp) * ((unstable / DRIFT_N_STABLE) as f64);
            kp *= adjustment_factor;
        }

        self.drift_n += 1;

        self.raw_drift += drift_error * kp;
        self.drift = self.raw_drift - ki * self.cumulative_error;
        self.i_drift = -1.0 * self.drift / (1.0 + self.drift);
        true
    }

    fn update_offset(&mut self, base_ts: f64, peer_ts: f64, prediction_error: f64) {
        let p_var = prediction_error * prediction_error;
        let n = self.n as usize;

        self.ts_base[n] = base_ts;
        self.ts_peer[n] = peer_ts;
        self.var[n] = p_var;

        if n == 0 {
            self.base_avg = base_ts;
            self.peer_avg = peer_ts;
        } else {
            // The Python code reads ts_base[n] straight after storing base_ts
            // there, so `elapsed` is always zero; kept for identical results.
            let elapsed = base_ts - self.ts_base[n];
            let max_elapsed = 20.0;
            if elapsed < max_elapsed {
                let keep = ((1.0 - elapsed / max_elapsed).powi(2)) * 0.75;
                self.base_avg = self.base_avg * keep + base_ts * (1.0 - keep);
                self.peer_avg = self.peer_avg * keep + peer_ts * (1.0 - keep);
            } else {
                self.base_avg = base_ts;
                self.peer_avg = peer_ts;
            }
        }

        self.n += 1;
        self.var_sum += p_var;
    }

    /// Feeds one pair of simultaneous readings into the model. Returns whether
    /// the update was used, and any receivers whose clock sync must be reset.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        base_ts: f64,
        peer_ts: f64,
        base_interval: f64,
        peer_interval: f64,
        now: f64,
        ac: &AcShared,
        sync_dont_use: bool,
    ) -> (bool, Resets) {
        let mut resets = Resets::default();
        let mut prediction_error = 0.0;
        let mut outlier = false;
        let mut do_reset = false;

        if self.n as usize > CP_SIZE - 1 || base_ts - self.ts_base[0] > 50.0 * self.base_freq {
            self.prune_old_data(now);
        }

        if !sync_dont_use {
            self.update_total += 1.0;
        }
        self.update_attempted = now;

        if self.n > 0 && !outlier {
            let last = self.n as usize - 1;
            if peer_ts <= self.ts_peer[last] || base_ts <= self.ts_base[last] {
                if peer_ts < self.ts_peer[last] && base_ts < self.ts_base[last] {
                    return (false, resets);
                }
                if peer_ts == self.ts_peer[last] || base_ts == self.ts_base[last] {
                    return (false, resets);
                }

                self.valid = false;
                self.outliers += 10;
                outlier = true;
                self.outlier_total += 1.0;

                if self.outliers <= 10 {
                    return (false, resets);
                }
            }
        }

        if self.n > 0 && !outlier {
            let prediction = self.predict_peer(base_ts);
            prediction_error = (prediction - peer_ts) / self.peer_freq;

            let outlier_threshold = if self.n >= 4 { self.outlier_threshold } else { 2.0 * self.outlier_threshold };

            let abs_error = prediction_error.abs();
            self.base.num_syncs.add(1.0);
            self.peer.num_syncs.add(1.0);
            if abs_error > outlier_threshold {
                if self.peer.bad_syncs.load() < 0.01 && self.base.bad_syncs.load() < 0.01 {
                    ac.sync_bad.add(1.0);
                }

                if sync_dont_use {
                    return (false, resets);
                }

                outlier = true;
                self.outlier_total += 1.0;
                if abs_error > 3.0 * outlier_threshold {
                    self.outliers += 20;
                } else {
                    self.outliers += 6;
                }

                if self.outliers <= 77 && abs_error < 5.0 * outlier_threshold {
                    return (false, resets);
                }

                if abs_error > 3.0 * outlier_threshold {
                    do_reset = true;
                    if self.jumped == 0 {
                        if self.peer.bad_syncs.load() < 0.01 && self.base.increment_jumps() {
                            resets.0[0] = Some(self.base.uid);
                        }
                        if self.base.bad_syncs.load() < 0.01 && self.peer.increment_jumps() {
                            resets.0[1] = Some(self.peer.uid);
                        }
                    }
                    self.jumped = 1;
                }
            } else {
                ac.sync_good.add(1.0);
                ac.last_adsb_time.store(now);
            }
        }

        if sync_dont_use {
            return (false, resets);
        }

        if outlier && do_reset {
            self.reset_offsets();
            self.outlier_reset_cooldown = 15;
            prediction_error = 0.0;
        }

        self.outliers = (self.outliers - 18).max(0);

        if !outlier {
            self.cumulative_error = (-50e-6_f64).max(50e-6_f64.min(self.cumulative_error + prediction_error));
        }

        self.outlier_reset_cooldown = (self.outlier_reset_cooldown - 1).max(0);

        if !self.update_drift(base_interval, peer_interval) {
            self.check_valid(now);
            return (false, resets);
        }

        self.factor = self.relative_freq * (1.0 + self.drift);
        self.i_factor = self.i_relative_freq * (1.0 + self.i_drift);

        self.update_offset(base_ts, peer_ts, prediction_error);

        self.updated = now;
        self.check_valid(now);
        (true, resets)
    }
}
