//! A connected receiver as seen by the sync engine.

use crate::atomic::AtomicF64;
use crate::clock::Clock;
use crate::geodesy::llh2ecef;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

pub struct Receiver {
    pub uid: u32,
    pub user: String,
    pub pos: [f64; 3],
    pub clock: Clock,
    pub dead: AtomicBool,
    /// Last time a sync aircraft was further than MAX_RANGE from this receiver.
    pub sync_range_exceeded: AtomicF64,
    /// Number of sync peers per distance category (0-50 km, 50-100, 100-150, 150+).
    pub sync_peers: [AtomicI32; 4],
    pub num_syncs: AtomicF64,
    pub bad_syncs: AtomicF64,
    pub last_sync: AtomicF64,
    pub recent_pair_jumps: AtomicI32,
    pub num_outliers: AtomicF64,
    pub outlier_percent_rolling: AtomicF64,
    /// True once the client has sent a rate report (it then gets selective interest).
    pub has_rate_report: AtomicBool,
}

impl Receiver {
    pub fn new(uid: u32, user: String, lat: f64, lon: f64, alt: f64, clock: Clock) -> Receiver {
        Receiver {
            uid,
            user,
            pos: llh2ecef(lat, lon, alt),
            clock,
            dead: AtomicBool::new(false),
            sync_range_exceeded: AtomicF64::new(0.0),
            sync_peers: Default::default(),
            num_syncs: AtomicF64::new(0.0),
            bad_syncs: AtomicF64::new(0.0),
            last_sync: AtomicF64::new(0.0),
            recent_pair_jumps: AtomicI32::new(0),
            num_outliers: AtomicF64::new(0.0),
            outlier_percent_rolling: AtomicF64::new(0.0),
            has_rate_report: AtomicBool::new(false),
        }
    }

    /// A receiver whose ECEF position is already known (tests, replays).
    pub fn from_ecef(uid: u32, user: String, pos: [f64; 3], clock: Clock) -> Receiver {
        let mut r = Receiver::new(uid, user, 0.0, 0.0, 0.0, clock);
        r.pos = pos;
        r
    }

    #[inline]
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }

    pub fn total_peers(&self) -> i32 {
        self.sync_peers.iter().map(|p| p.load(Ordering::Relaxed)).sum()
    }

    /// Records a clock jump seen by one of this receiver's pairings. Returns
    /// true when so many pairings jumped that the receiver's clock sync
    /// should be reset (the caller does the reset).
    pub fn increment_jumps(&self) -> bool {
        let jumps = self.recent_pair_jumps.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.total_peers();
        if total > 0 && jumps as f64 / total as f64 > 0.25 && jumps > 3 {
            self.recent_pair_jumps.store(0, Ordering::Relaxed);
            self.bad_syncs.add(0.1);
            return true;
        }
        false
    }
}
