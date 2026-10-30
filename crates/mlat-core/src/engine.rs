//! Multithreaded clock-synchronisation engine.
//!
//! Two kinds of worker threads, mirroring how the work divides:
//!
//! * **Matchers**, sharded by aircraft address. A sync point is the same pair
//!   of ADS-B messages heard by several receivers; every copy of a pair has the
//!   same address, so one matcher sees all of them. Matchers decode the pair,
//!   work out the sync aircraft's position and, for each receiver already
//!   holding the sync point, emit a pair update.
//! * **Pairers**, sharded by receiver pair. Each owns the `ClockPairing` state
//!   of its pairs and applies updates in arrival order.
//!
//! With one matcher and one pairer every message is handled in order, which is
//! the configuration compared exactly against the Python server.

use crate::atomic::AtomicF64;
use crate::clock::Clock;
use crate::collector::Collector;
use crate::geodesy::{ecef_distance, llh2ecef};
use crate::modes::{decode_any, decode_cpr, decode_df17, downlink_format, EsType};
use crate::normalize::{normalize2, Normalised, PairParams, ReceiverStamps};
use crate::pairing::{round_dec, AcShared, ClockPairing, PairHealth};
use crate::receiver::Receiver;
use crate::resolve::{resolve, AircraftMlat, AircraftTable, Group, MlatResult, ResolveEnv, ResolveStats};
use crate::FTOM;
use ahash::{AHashMap, AHashSet};
use crossbeam_channel::{bounded, unbounded, Receiver as Rx, Sender};
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

const FORCE_MLAT_INTERVAL: f64 = 600.0;
const NO_ADSB_MLAT_SECONDS: f64 = 120.0;

const MAX_RANGE: f64 = 500e3;
const MAX_INTERMESSAGE_RANGE: f64 = 10e3;

fn get_limit(cat: usize) -> f64 {
    if cat <= 1 {
        32.0
    } else {
        16.0
    }
}

/// One row of a pairing snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct PairRow {
    pub a: u32,
    pub b: u32,
    pub n: i32,
    pub valid: bool,
    pub variance: f64,
    pub factor: f64,
    pub i_factor: f64,
    pub base_avg: f64,
    pub peer_avg: f64,
    pub updated: f64,
}

#[derive(Default)]
pub struct Stats {
    pub sync_in: AtomicU64,
    pub syncpoints_created: AtomicU64,
    pub pair_updates: AtomicU64,
    pub pairs_created: AtomicU64,
    pub pairs_deleted: AtomicU64,
}

pub struct SyncMsg {
    pub now: f64,
    pub recv: Arc<Receiver>,
    pub et: f64,
    pub ot: f64,
    pub em: [u8; 14],
    pub om: [u8; 14],
}

#[derive(Clone, Copy)]
enum PairControl {
    Cleanup(f64),
    ResetReceiver(u32),
    Disconnect(u32),
}

enum MatchMsg {
    Sync { seq: u64, m: SyncMsg },
    Seen { seq: u64, now: f64, uid: u32, icaos: Vec<u32> },
    Lost { seq: u64, uid: u32, icaos: Vec<u32> },
    Disconnect { seq: u64, uid: u32 },
    /// Forwarded (by matcher 0 only) to every pairer.
    Control { seq: u64, c: PairControl },
    ClearSyncPoints { seq: u64 },
    /// Advances the matcher's progress when it has no work of its own.
    Tick { seq: u64 },
    Barrier { seq: u64, done: Sender<()> },
    /// A message heard by a receiver and sent as mlat input.
    Mlat { seq: u64, uid: u32, ts: f64, msg: Vec<u8>, now: f64 },
    /// Resolve a group whose collection delay has run out.
    Resolve { seq: u64, group: Group, now: f64 },
    /// Work out which aircraft want multilateration (`update_interest`'s first block).
    RecomputeWanted { seq: u64, now: f64 },
    /// The per-aircraft part of the 15 s health evaluation.
    AircraftHealth { seq: u64, reply: Sender<Vec<AcReport>> },
    Shutdown,
}

impl MatchMsg {
    fn seq(&self) -> Option<u64> {
        match self {
            MatchMsg::Sync { seq, .. }
            | MatchMsg::Seen { seq, .. }
            | MatchMsg::Lost { seq, .. }
            | MatchMsg::Disconnect { seq, .. }
            | MatchMsg::Control { seq, .. }
            | MatchMsg::ClearSyncPoints { seq }
            | MatchMsg::Tick { seq }
            | MatchMsg::Mlat { seq, .. }
            | MatchMsg::Resolve { seq, .. }
            | MatchMsg::RecomputeWanted { seq, .. }
            | MatchMsg::AircraftHealth { seq, .. }
            | MatchMsg::Barrier { seq, .. } => Some(*seq),
            MatchMsg::Shutdown => None,
        }
    }
}

struct PairUpdate {
    r0: Arc<Receiver>,
    r1: Arc<Receiver>,
    td0: f64,
    i0: f64,
    td1: f64,
    i1: f64,
    now: f64,
    ac: Arc<AcShared>,
    /// The aircraft's `sync_dont_use` flag when the update was emitted.
    dont_use: bool,
}

enum PairMsg {
    /// Work stamped with the global sequence number of the event that caused it.
    Update { from: usize, seq: u64, u: PairUpdate },
    Control { from: usize, seq: u64, c: PairControl },
    /// Matcher `from` has handled everything up to `seq`.
    Watermark { from: usize, seq: u64 },
    /// A clock reset caused by another pairer, applied on arrival.
    ResetNow(u32),
    Snapshot(Sender<Vec<PairRow>>),
    /// Apply everything released so far, then acknowledge.
    Flush(Sender<()>),
    /// The pairings' contribution to the 15 s health evaluation.
    Dump { now: f64, reply: Sender<Vec<(u32, u32, PairHealth)>> },
    Shutdown,
}

fn pair_shard(a: u32, b: u32, n: usize) -> usize {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let h = (lo as u64) << 32 | hi as u64;
    (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 33) as usize % n
}

fn icao_shard(icao: u32, n: usize) -> usize {
    (icao.wrapping_mul(0x9E37_79B1) >> 8) as usize % n
}

/// Every receiver ever connected, by uid (disconnected ones stay, marked dead,
/// because groups still in flight refer to them).
#[derive(Default)]
pub struct Registry {
    v: RwLock<Vec<Option<Arc<Receiver>>>>,
}

impl Registry {
    pub fn get(&self, uid: u32) -> Option<Arc<Receiver>> {
        self.v.read().unwrap().get(uid as usize).and_then(|r| r.clone())
    }

    fn push(&self, r: Arc<Receiver>) {
        self.v.write().unwrap().push(Some(r));
    }

    /// Receivers that are still connected, in uid order.
    pub fn live(&self) -> Vec<Arc<Receiver>> {
        self.v.read().unwrap().iter().flatten().filter(|r| !r.is_dead()).cloned().collect()
    }

    fn len(&self) -> usize {
        self.v.read().unwrap().len()
    }
}

/// The clock pairings' model parameters, published by the pairers for the
/// resolvers to read.
pub struct PairStore {
    shards: Vec<RwLock<AHashMap<(u32, u32), PairParams>>>,
}

impl PairStore {
    fn new(n: usize) -> PairStore {
        PairStore { shards: (0..n).map(|_| RwLock::new(AHashMap::new())).collect() }
    }

    fn key(a: u32, b: u32) -> (u32, u32) {
        if a < b {
            (a, b)
        } else {
            (b, a)
        }
    }

    pub fn get(&self, a: u32, b: u32) -> Option<PairParams> {
        let k = Self::key(a, b);
        self.shards[pair_shard(k.0, k.1, self.shards.len())].read().unwrap().get(&k).copied()
    }

    fn put(&self, k: (u32, u32), p: PairParams) {
        self.shards[pair_shard(k.0, k.1, self.shards.len())].write().unwrap().insert(k, p);
    }

    fn remove(&self, k: (u32, u32)) {
        self.shards[pair_shard(k.0, k.1, self.shards.len())].write().unwrap().remove(&k);
    }
}

/// An aircraft's line in the 15 s state report.
#[derive(Clone, Debug)]
pub struct AcReport {
    pub icao: u32,
    pub seen: f64,
    pub tracking: Vec<u32>,
    pub allow_mlat: bool,
    pub mlat_message_count: u64,
    pub mlat_result_count: u64,
    pub mlat_kalman_count: u64,
    pub sync_count: i64,
    pub sync_bad_percent: f64,
    pub last_result: Option<(f64, [f64; 3])>,
    pub altitude: Option<i32>,
    /// (heading, ground speed) when the Kalman track is valid.
    pub kalman: Option<(f64, f64)>,
}

/// One pairing as seen from one of its two receivers.
#[derive(Clone, Debug)]
pub struct PeerReport {
    pub peer: u32,
    pub n: i32,
    pub error_us: f64,
    pub drift_ppm: i64,
    pub peer_bad_syncs: f64,
    pub jumped: bool,
    pub outlier_percent: f64,
    pub age: i64,
    pub peer_total: i32,
}

/// Everything the 15 s evaluation learned, for the state files and the log.
#[derive(Default)]
pub struct HealthReport {
    pub aircraft: Vec<AcReport>,
    pub peers: AHashMap<u32, Vec<PeerReport>>,
    /// Peers each receiver judged to have bad sync with it.
    pub bad_peers: AHashMap<u32, Vec<u32>>,
    pub bad_receivers: usize,
    pub outlier_sum: f64,
    pub sync_sum: f64,
}

/// What interest selection needs to know about an aircraft, published by the
/// shard that owns it whenever the set of wanted aircraft is recomputed.
#[derive(Clone, Copy, Debug)]
pub struct AcInfo {
    pub altitude: Option<i32>,
    pub sync_dont_use: bool,
    pub do_mlat: bool,
}

pub struct AcInfoStore {
    shards: Vec<RwLock<AHashMap<u32, AcInfo>>>,
}

impl AcInfoStore {
    fn new(n: usize) -> AcInfoStore {
        AcInfoStore { shards: (0..n).map(|_| RwLock::new(AHashMap::new())).collect() }
    }

    pub fn get(&self, icao: u32) -> Option<AcInfo> {
        self.shards[icao_shard(icao, self.shards.len())].read().unwrap().get(&icao).copied()
    }
}

// ---------------------------------------------------------------- matcher

/// Everything the server tracks about one aircraft, owned by the shard that
/// its address hashes to.
struct Aircraft {
    tracking: AHashSet<u32>,
    shared: Arc<AcShared>,
    mlat: AircraftMlat,
    /// Is this aircraft currently wanted for multilateration?
    do_mlat: bool,
    sync_bad_percent: f64,
}

/// The forced-mlat phase of a new aircraft: random in the server, a function
/// of the address in deterministic (test) mode.
fn force_phase(icao: u32, now: f64, deterministic: bool) -> f64 {
    let h = if deterministic {
        (icao as u64).wrapping_mul(2_654_435_761) % (1 << 32)
    } else {
        let mut x = (icao as u64) ^ now.to_bits() ^ 0x9E37_79B9_7F4A_7C15;
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        x ^= x >> 33;
        x % (1 << 32)
    };
    h as f64 / (1u64 << 32) as f64
}

/// `Tracker.in_local_partition`: whether this server does mlat for the address
/// (`partition` is the 1-based index and the count of `--partition I/N`).
pub fn in_local_partition(icao: u32, partition: (u32, u32)) -> bool {
    if partition.1 <= 1 {
        return true;
    }
    let mut h = icao;
    h = ((h >> 16) ^ h).wrapping_mul(0x45d9f3b);
    h = ((h >> 16) ^ h).wrapping_mul(0x45d9f3b);
    h = (h >> 16) ^ h;
    h % partition.1 == partition.0 - 1
}

impl Aircraft {
    fn new(icao: u32, now: f64, deterministic: bool, partition: (u32, u32)) -> Aircraft {
        let mut mlat = AircraftMlat::new(icao, now);
        mlat.allow_mlat = in_local_partition(icao, partition);
        mlat.last_force_mlat = now - FORCE_MLAT_INTERVAL * force_phase(icao, now, deterministic);
        Aircraft { tracking: AHashSet::new(), shared: Arc::new(AcShared::default()), mlat, do_mlat: false, sync_bad_percent: 0.0 }
    }
}

struct ShardTable<'a>(&'a mut AHashMap<u32, Aircraft>);

impl AircraftTable for ShardTable<'_> {
    fn get_mut(&mut self, icao: u32) -> Option<&mut AircraftMlat> {
        self.0.get_mut(&icao).map(|a| &mut a.mlat)
    }
}

struct ShardEnv<'a> {
    registry: &'a Registry,
    store: &'a PairStore,
}

impl ResolveEnv for ShardEnv<'_> {
    fn receiver(&self, uid: u32) -> Option<Arc<Receiver>> {
        self.registry.get(uid)
    }

    fn normalize(&self, receivers: &[ReceiverStamps], now: f64) -> Vec<Normalised> {
        normalize2(receivers, |a, b| self.store.get(a, b), now)
    }
}

struct SyncRecv {
    recv: Arc<Receiver>,
    td: f64,
    i: f64,
}

struct SyncPoint {
    interval: f64,
    receivers: Vec<SyncRecv>,
}

struct Details {
    pos_a: [f64; 3],
    pos_b: [f64; 3],
    ac: Arc<AcShared>,
}

struct Entry {
    /// Did the aircraft want multilateration when this pair was first seen?
    /// Later copies of the pair are then also fed to the mlat grouping.
    do_mlat: bool,
    /// None once the message pair was examined and found unusable.
    details: Option<Details>,
    list: Vec<SyncPoint>,
}

struct Matcher {
    idx: usize,
    deterministic: bool,
    partition: (u32, u32),
    aircraft: AHashMap<u32, Aircraft>,
    points: AHashMap<[u8; 28], Entry>,
    pairers: Vec<Sender<PairMsg>>,
    stats: Arc<Stats>,
    registry: Arc<Registry>,
    store: Arc<PairStore>,
    collector: Arc<Mutex<Collector>>,
    /// Wakes the clock thread when the collector goes from empty to holding a group.
    kick: Sender<()>,
    results: Sender<MlatResult>,
    rstats: ResolveStats,
    info: Arc<AcInfoStore>,
}

/// Changes the collector, and wakes the clock thread if the new group is now the earliest deadline.
fn collect(collector: &Mutex<Collector>, kick: &Sender<()>, f: impl FnOnce(&mut Collector)) {
    let was_empty = {
        let mut c = collector.lock().unwrap();
        let empty = c.next_due().is_none();
        f(&mut c);
        empty
    };
    if was_empty {
        let _ = kick.try_send(());
    }
}

impl Matcher {
    fn run(mut self, rx: Rx<MatchMsg>) {
        let mut last_seq = 0u64;
        let mut announced = 0u64;
        'outer: loop {
            let Ok(first) = rx.recv() else { break };
            let mut next = Some(first);
            let mut batch = 0;
            while let Some(msg) = next.take() {
                if let Some(s) = msg.seq() {
                    last_seq = last_seq.max(s);
                }
                if !self.handle(msg) {
                    break 'outer;
                }
                batch += 1;
                if batch < 512 {
                    next = rx.try_recv().ok();
                }
            }
            // tell the pairers how far this matcher has got, once per batch
            if last_seq > announced {
                announced = last_seq;
                for p in &self.pairers {
                    let _ = p.send(PairMsg::Watermark { from: self.idx, seq: last_seq });
                }
            }
        }
    }

    /// Returns false on shutdown.
    fn handle(&mut self, msg: MatchMsg) -> bool {
        match msg {
            MatchMsg::Sync { seq, m } => self.sync(seq, m),
            MatchMsg::Seen { now, uid, icaos, .. } => {
                let det = self.deterministic;
                let part = self.partition;
                for icao in icaos {
                    let ac = self.aircraft.entry(icao).or_insert_with(|| Aircraft::new(icao, now, det, part));
                    ac.tracking.insert(uid);
                    ac.mlat.seen = now;
                }
            }
            MatchMsg::Mlat { uid, ts, msg, now, .. } => {
                collect(&self.collector, &self.kick, |c| c.add(uid, ts, &msg, now));
            }
            MatchMsg::Resolve { group, now, .. } => {
                if !group.dummy {
                    let env = ShardEnv { registry: &self.registry, store: &self.store };
                    let mut table = ShardTable(&mut self.aircraft);
                    if let Some(result) = resolve(&group, &mut table, now, &env, &mut self.rstats) {
                        let _ = self.results.send(result);
                    }
                }
            }
            MatchMsg::RecomputeWanted { now, .. } => {
                for ac in self.aircraft.values_mut() {
                    let since_force = now - ac.mlat.last_force_mlat;
                    if !ac.mlat.force_mlat && since_force > FORCE_MLAT_INTERVAL {
                        ac.mlat.force_mlat = true;
                    }
                    if since_force > FORCE_MLAT_INTERVAL + 30.0 {
                        ac.mlat.last_force_mlat = now;
                        ac.mlat.force_mlat = false;
                    }
                    ac.do_mlat = ac.tracking.len() >= 2
                        && ac.mlat.allow_mlat
                        && (now - ac.shared.last_adsb_time.load() > NO_ADSB_MLAT_SECONDS
                            || ac.sync_bad_percent > 10.0
                            || ac.mlat.force_mlat);
                }
                // publish for interest selection
                let mut m = AHashMap::with_capacity(self.aircraft.len());
                for (icao, ac) in &self.aircraft {
                    m.insert(*icao, AcInfo { altitude: ac.mlat.altitude, sync_dont_use: ac.shared.sync_dont_use.load(Ordering::Relaxed), do_mlat: ac.do_mlat });
                }
                *self.info.shards[self.idx].write().unwrap() = m;
            }
            MatchMsg::AircraftHealth { reply, .. } => {
                let mut reports = Vec::with_capacity(self.aircraft.len());
                for ac in self.aircraft.values_mut() {
                    let (good, bad) = (ac.shared.sync_good.load(), ac.shared.sync_bad.load());
                    let sync_count = (good + bad).round_ties_even();
                    let pct = round_dec(100.0 * bad / (sync_count + 0.01), 1);
                    reports.push(AcReport {
                        icao: ac.mlat.icao,
                        seen: ac.mlat.seen,
                        tracking: ac.tracking.iter().copied().collect(),
                        allow_mlat: ac.mlat.allow_mlat,
                        mlat_message_count: ac.mlat.mlat_message_count,
                        mlat_result_count: ac.mlat.mlat_result_count,
                        mlat_kalman_count: ac.mlat.mlat_kalman_count,
                        sync_count: sync_count as i64,
                        sync_bad_percent: pct,
                        last_result: ac.mlat.last_result_time.zip(ac.mlat.last_result_position),
                        altitude: ac.mlat.altitude,
                        kalman: ac.mlat.kalman.valid.then_some((ac.mlat.kalman.heading.unwrap_or(0.0), ac.mlat.kalman.ground_speed.unwrap_or(0.0))),
                    });
                    ac.sync_bad_percent = pct;
                    ac.shared.sync_dont_use.store(bad > 3.0 && pct > 15.0, Ordering::Relaxed);
                    ac.shared.sync_good.store(good * 0.8);
                    ac.shared.sync_bad.store(bad * 0.8);
                }
                let _ = reply.send(reports);
            }
            MatchMsg::Lost { uid, icaos, .. } => {
                for icao in icaos {
                    if let Some(ac) = self.aircraft.get_mut(&icao) {
                        ac.tracking.remove(&uid);
                        if ac.tracking.is_empty() {
                            self.aircraft.remove(&icao);
                        }
                    }
                }
            }
            MatchMsg::Disconnect { uid, .. } => {
                self.aircraft.retain(|_, ac| {
                    ac.tracking.remove(&uid);
                    !ac.tracking.is_empty()
                });
            }
            MatchMsg::Control { seq, c } => {
                for p in &self.pairers {
                    let _ = p.send(PairMsg::Control { from: self.idx, seq, c });
                }
            }
            MatchMsg::ClearSyncPoints { .. } => self.points.clear(),
            MatchMsg::Tick { .. } => {}
            MatchMsg::Barrier { seq, done } => {
                for p in &self.pairers {
                    let _ = p.send(PairMsg::Watermark { from: self.idx, seq });
                }
                let _ = done.send(());
            }
            MatchMsg::Shutdown => return false,
        }
        true
    }

    /// ClockTracker.receiver_sync.
    fn sync(&mut self, seq: u64, m: SyncMsg) {
        self.stats.sync_in.fetch_add(1, Ordering::Relaxed);
        let r0 = &m.recv;
        let now = m.now;

        let (ta, tb, k0, k1) = if m.et < m.ot { (m.et, m.ot, &m.em, &m.om) } else { (m.ot, m.et, &m.om, &m.em) };
        let mut key = [0u8; 28];
        key[..14].copy_from_slice(k0);
        key[14..].copy_from_slice(k1);

        let interval = (tb - ta) / r0.clock.freq;

        if let Some(entry) = self.points.get_mut(&key) {
            // later copies of a pair for an aircraft in mlat mode also feed the mlat grouping
            if entry.do_mlat {
                collect(&self.collector, &self.kick, |c| c.add(r0.uid, m.et, &m.em, now));
            }
            let Some(details) = entry.details.as_ref() else { return };
            if interval > 5.0 {
                return;
            }
            let idx = match entry.list.iter().position(|sp| (sp.interval - interval).abs() < 0.75e-3) {
                Some(i) => i,
                None => {
                    entry.list.push(SyncPoint { interval, receivers: Vec::new() });
                    entry.list.len() - 1
                }
            };
            add_to_existing(self.idx, seq, &self.pairers, &self.stats, details, &mut entry.list[idx], r0, ta, tb, now);
            return;
        }

        if interval > 5.0 {
            return;
        }
        if now - r0.sync_range_exceeded.load() < 15.0 {
            return;
        }

        // Only DF17 pairs can synchronise clocks. (The Python server also
        // caches other message types as unusable; that only saves it work.)
        let Some(even) = decode_df17(&m.em) else { return };
        let odd_df = downlink_format(&m.om);
        if !matches!(odd_df, 0 | 4 | 5 | 11 | 16 | 17 | 18 | 20 | 21) {
            return;
        }
        let odd = decode_df17(&m.om);

        let Some(ac) = self.aircraft.get_mut(&even.address) else { return };
        let ac_shared = ac.shared.clone();
        ac.mlat.seen = now;
        let do_mlat = (ac.do_mlat || ac.mlat.force_mlat) && ac.tracking.len() >= 3;
        if do_mlat {
            collect(&self.collector, &self.kick, |c| c.add_placeholder(r0.uid, m.et, now));
        }

        // From here on the pair is remembered, usable or not.
        self.points.insert(key, Entry { do_mlat, details: None, list: Vec::new() });

        let Some(odd) = odd else { return };
        if !even.crc_ok || even.estype != EsType::AirbornePosition || even.f != 0 {
            return;
        }
        if !odd.crc_ok || odd.estype != EsType::AirbornePosition || odd.f == 0 {
            return;
        }
        if even.address != odd.address {
            return;
        }
        let Some([even_lat, even_lon, odd_lat, odd_lon]) =
            decode_cpr(even.lat as f64, even.lon as f64, odd.lat as f64, odd.lon as f64)
        else {
            return;
        };
        let (Some(even_alt), Some(odd_alt)) = (even.altitude, odd.altitude) else { return };
        if (even_alt - odd_alt).abs() > 5000 {
            return;
        }
        if even_lat > 85.0 || even_lat < -85.0 {
            return;
        }
        if even_lat == 0.0 || even_lon == 0.0 {
            return;
        }

        let even_ecef = llh2ecef(even_lat, even_lon, even_alt as f64 * FTOM);
        let odd_ecef = llh2ecef(odd_lat, odd_lon, odd_alt as f64 * FTOM);
        if ecef_distance(&even_ecef, &odd_ecef) > MAX_INTERMESSAGE_RANGE {
            return;
        }

        if let Some(ac) = self.aircraft.get_mut(&even.address) {
            ac.mlat.last_altitude_time = Some(now);
            ac.mlat.altitude = Some(even_alt);
        }

        if even.nuc.map_or(true, |n| n < 6) || odd.nuc.map_or(true, |n| n < 6) {
            return;
        }

        let (pos_a, pos_b) = if m.et < m.ot { (even_ecef, odd_ecef) } else { (odd_ecef, even_ecef) };
        let details = Details { pos_a, pos_b, ac: ac_shared };

        self.stats.syncpoints_created.fetch_add(1, Ordering::Relaxed);
        let mut sp = SyncPoint { interval, receivers: Vec::new() };

        if ecef_distance(&even_ecef, &r0.pos) > MAX_RANGE {
            r0.sync_range_exceeded.store(now);
            self.points.insert(key, Entry { do_mlat, details: Some(details), list: vec![sp] });
            return;
        }
        add_to_existing(self.idx, seq, &self.pairers, &self.stats, &details, &mut sp, r0, ta, tb, now);
        self.points.insert(key, Entry { do_mlat, details: Some(details), list: vec![sp] });
    }
}

/// `_add_to_existing_syncpoint`: records this receiver's view of the sync
/// point and emits a pair update against every receiver already holding it.
#[allow(clippy::too_many_arguments)]
fn add_to_existing(
    from: usize,
    seq: u64,
    pairers: &[Sender<PairMsg>],
    stats: &Stats,
    details: &Details,
    sp: &mut SyncPoint,
    r0: &Arc<Receiver>,
    t0a: f64,
    t0b: f64,
    now: f64,
) {
    let dist_a = ecef_distance(&details.pos_a, &r0.pos);
    let dist_b = ecef_distance(&details.pos_b, &r0.pos);

    if dist_a > MAX_RANGE {
        r0.sync_range_exceeded.store(now);
        return;
    }

    let delay_factor = r0.clock.delay_factor;
    let td0a = t0a - dist_a * delay_factor;
    let td0b = t0b - dist_b * delay_factor;
    let i0 = td0b - td0a;

    for r1l in &sp.receivers {
        if r1l.recv.uid == r0.uid {
            continue;
        }
        let u = PairUpdate {
            r0: r0.clone(),
            r1: r1l.recv.clone(),
            td0: td0b,
            i0,
            td1: r1l.td,
            i1: r1l.i,
            now,
            ac: details.ac.clone(),
            dont_use: details.ac.sync_dont_use.load(Ordering::Relaxed),
        };
        stats.pair_updates.fetch_add(1, Ordering::Relaxed);
        let shard = pair_shard(r0.uid, r1l.recv.uid, pairers.len());
        let _ = pairers[shard].send(PairMsg::Update { from, seq, u });
    }

    sp.receivers.push(SyncRecv { recv: r0.clone(), td: td0b, i: i0 });
}

// ----------------------------------------------------------------- pairer

enum Pend {
    Update(PairUpdate),
    Control(PairControl),
}

/// Min-heap entry: released in (seq, arrival) order.
struct Pending {
    seq: u64,
    ctr: u64,
    item: Pend,
}

impl PartialEq for Pending {
    fn eq(&self, o: &Self) -> bool {
        (self.seq, self.ctr) == (o.seq, o.ctr)
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Pending {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        // reversed: BinaryHeap is a max-heap
        (o.seq, o.ctr).cmp(&(self.seq, self.ctr))
    }
}

struct Pairer {
    pairs: AHashMap<(u32, u32), ClockPairing>,
    peers: Vec<Sender<PairMsg>>,
    stats: Arc<Stats>,
    /// Where this pairer publishes its pairings' model parameters.
    store: Arc<PairStore>,
    /// Per matcher: everything up to this sequence number has been received.
    watermarks: Vec<u64>,
    heap: BinaryHeap<Pending>,
    ctr: u64,
}

impl Pairer {
    fn run(mut self, rx: Rx<PairMsg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                PairMsg::Update { from, seq, u } => {
                    self.watermarks[from] = self.watermarks[from].max(seq);
                    self.push(seq, Pend::Update(u));
                    self.release();
                }
                PairMsg::Control { from, seq, c } => {
                    self.watermarks[from] = self.watermarks[from].max(seq);
                    self.push(seq, Pend::Control(c));
                    self.release();
                }
                PairMsg::Watermark { from, seq } => {
                    self.watermarks[from] = self.watermarks[from].max(seq);
                    self.release();
                }
                PairMsg::ResetNow(uid) => self.reset_receiver(uid, None),
                PairMsg::Snapshot(tx) => {
                    self.release();
                    let mut rows: Vec<PairRow> = self
                        .pairs
                        .iter()
                        .map(|(&(a, b), p)| PairRow {
                            a,
                            b,
                            n: p.n,
                            valid: p.valid,
                            variance: p.variance,
                            factor: p.factor,
                            i_factor: p.i_factor,
                            base_avg: p.base_avg,
                            peer_avg: p.peer_avg,
                            updated: p.updated,
                        })
                        .collect();
                    rows.sort_by_key(|r| (r.a, r.b));
                    let _ = tx.send(rows);
                }
                PairMsg::Flush(tx) => {
                    self.release();
                    let _ = tx.send(());
                }
                PairMsg::Dump { now, reply } => {
                    self.release();
                    let rows = self.pairs.iter_mut().filter_map(|(k, p)| p.dump_state(now).map(|h| (k.0, k.1, h))).collect();
                    let _ = reply.send(rows);
                }
                PairMsg::Shutdown => break,
            }
        }
    }


    fn push(&mut self, seq: u64, item: Pend) {
        self.ctr += 1;
        self.heap.push(Pending { seq, ctr: self.ctr, item });
    }

    /// Applies, in global order, everything every matcher has moved past.
    fn release(&mut self) {
        let safe = self.watermarks.iter().copied().min().unwrap_or(0);
        while self.heap.peek().is_some_and(|t| t.seq <= safe) {
            match self.heap.pop().unwrap().item {
                Pend::Update(u) => self.update(u),
                Pend::Control(c) => self.control(c),
            }
        }
    }

    fn reset_receiver(&mut self, uid: u32, except: Option<(u32, u32)>) {
        for (k, p) in self.pairs.iter_mut() {
            if Some(*k) != except && p.involves(uid) {
                p.reset_offsets();
                self.store.put(*k, p.params());
            }
        }
    }

    fn control(&mut self, c: PairControl) {
        match c {
            PairControl::Cleanup(now) => {
                let mut prune = Vec::new();
                for (k, p) in self.pairs.iter_mut() {
                    p.check_valid(now);
                    if now - p.updated > 45.0 || (!p.valid && now - p.updated > 30.0) {
                        prune.push(*k);
                    } else {
                        self.store.put(*k, p.params());
                    }
                }
                for k in prune {
                    self.store.remove(k);
                    if let Some(p) = self.pairs.remove(&k) {
                        p.base().sync_peers[p.cat].fetch_sub(1, Ordering::Relaxed);
                        p.peer().sync_peers[p.cat].fetch_sub(1, Ordering::Relaxed);
                        self.stats.pairs_deleted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            PairControl::ResetReceiver(uid) => self.reset_receiver(uid, None),
            PairControl::Disconnect(uid) => {
                let doomed: Vec<_> = self.pairs.iter().filter(|(_, p)| p.involves(uid)).map(|(k, _)| *k).collect();
                for k in doomed {
                    self.store.remove(k);
                    if let Some(p) = self.pairs.remove(&k) {
                        p.base().sync_peers[p.cat].fetch_sub(1, Ordering::Relaxed);
                        p.peer().sync_peers[p.cat].fetch_sub(1, Ordering::Relaxed);
                        self.stats.pairs_deleted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// The pairing half of `_add_to_existing_syncpoint`.
    fn update(&mut self, u: PairUpdate) {
        let (r0, r1) = (&u.r0, &u.r1);
        let key = if r0.uid < r1.uid { (r0.uid, r1.uid) } else { (r1.uid, r0.uid) };
        let now = u.now;

        match self.pairs.get_mut(&key) {
            None => {
                if r1.is_dead() {
                    return;
                }
                let dist = ecef_distance(&r0.pos, &r1.pos);
                let cat = ((dist / 50e3) as usize).min(3);
                let limit = 0.7 * get_limit(cat);
                let p0 = r0.sync_peers[cat].load(Ordering::Relaxed) as f64;
                let p1 = r1.sync_peers[cat].load(Ordering::Relaxed) as f64;
                if p0 > limit && p1 > limit {
                    return;
                }
                self.pairs.insert(key, ClockPairing::new(r0.clone(), r1.clone(), cat));
                r0.sync_peers[cat].fetch_add(1, Ordering::Relaxed);
                r1.sync_peers[cat].fetch_add(1, Ordering::Relaxed);
                self.stats.pairs_created.fetch_add(1, Ordering::Relaxed);
            }
            Some(p) => {
                if p.n > 8 && now - p.update_attempted < 0.2 {
                    return;
                }
                let cat = p.cat;
                let limit = 1.2 * get_limit(cat);
                let p0 = r0.sync_peers[cat].load(Ordering::Relaxed) as f64;
                let p1 = r1.sync_peers[cat].load(Ordering::Relaxed) as f64;
                if p0 > limit && p1 > limit {
                    r0.sync_peers[cat].fetch_sub(1, Ordering::Relaxed);
                    r1.sync_peers[cat].fetch_sub(1, Ordering::Relaxed);
                    self.pairs.remove(&key);
                    self.store.remove(key);
                    self.stats.pairs_deleted.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            }
        }

        let pairing = self.pairs.get_mut(&key).expect("pairing present");
        let (_, resets) = if r0.uid < r1.uid {
            pairing.update(u.td0, u.td1, u.i0, u.i1, now, &u.ac, u.dont_use)
        } else {
            pairing.update(u.td1, u.td0, u.i1, u.i0, now, &u.ac, u.dont_use)
        };
        self.store.put(key, pairing.params());

        // A clock jump resets every pairing of the receiver. The pairing that
        // reported the jump has already reset itself and then stored its newest
        // point, so it is skipped here (the Python server resets it mid-update,
        // i.e. before that point is stored).
        for uid in resets.0.into_iter().flatten() {
            self.reset_receiver(uid, Some(key));
            for peer in &self.peers {
                let _ = peer.send(PairMsg::ResetNow(uid));
            }
        }
    }
}

// ----------------------------------------------------------------- engine

pub struct Engine {
    matchers: Vec<Sender<MatchMsg>>,
    pairers: Vec<Sender<PairMsg>>,
    handles: Vec<JoinHandle<()>>,
    registry: Arc<Registry>,
    collector: Arc<Mutex<Collector>>,
    kick_rx: Rx<()>,
    results: Rx<MlatResult>,
    info: Arc<AcInfoStore>,
    /// Global sequence number: orders every event the engine is given.
    seq: AtomicU64,
    /// Deterministic mode makes every observation wait for the pipeline to
    /// catch up, so a run is repeatable and comparable with a reference; it is
    /// for testing, not for production.
    deterministic: bool,
    /// Events sent to the matchers since the last time they were known idle.
    dirty: AtomicBool,
    /// ... and since the last time the pairers were known to be up to date.
    dirty_pairs: AtomicBool,
    /// The earliest event time not yet known to have been processed.
    unflushed_min: AtomicF64,
    /// When the set of aircraft wanted for multilateration was last recomputed.
    mlat_wanted_ts: AtomicF64,
    pub stats: Arc<Stats>,
}

/// How many events pass between progress ticks to idle matchers.
const TICK_EVERY: u64 = 128;

impl Engine {
    /// `start` is the server time at construction (the origin for the
    /// throttle on recomputing which aircraft want multilateration).
    pub fn new(n_matchers: usize, n_pairers: usize, deterministic: bool, start: f64) -> Engine {
        Engine::with_partition(n_matchers, n_pairers, deterministic, start, (1, 1))
    }

    pub fn with_partition(n_matchers: usize, n_pairers: usize, deterministic: bool, start: f64, partition: (u32, u32)) -> Engine {
        let n_matchers = n_matchers.max(1);
        let n_pairers = n_pairers.max(1);
        let stats = Arc::new(Stats::default());
        let registry = Arc::new(Registry::default());
        let store = Arc::new(PairStore::new(n_pairers));
        let info = Arc::new(AcInfoStore::new(n_matchers));
        let collector = Arc::new(Mutex::new(Collector::new()));
        let (results_tx, results_rx) = unbounded();
        let (kick_tx, kick_rx) = bounded::<()>(1);
        let mut handles = Vec::new();

        let mut pairer_tx = Vec::new();
        let mut pairer_rx = Vec::new();
        for _ in 0..n_pairers {
            let (tx, rx) = unbounded();
            pairer_tx.push(tx);
            pairer_rx.push(rx);
        }
        for (i, rx) in pairer_rx.into_iter().enumerate() {
            let p = Pairer {
                pairs: AHashMap::new(),
                peers: pairer_tx.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, t)| t.clone()).collect(),
                stats: stats.clone(),
                store: store.clone(),
                watermarks: vec![0; n_matchers],
                heap: BinaryHeap::new(),
                ctr: 0,
            };
            handles.push(std::thread::Builder::new().name(format!("pairer-{i}")).spawn(move || p.run(rx)).unwrap());
        }

        let mut matcher_tx = Vec::new();
        for i in 0..n_matchers {
            let (tx, rx) = bounded(16384);
            matcher_tx.push(tx);
            let m = Matcher {
                idx: i,
                deterministic,
                partition,
                aircraft: AHashMap::new(),
                points: AHashMap::new(),
                pairers: pairer_tx.clone(),
                stats: stats.clone(),
                registry: registry.clone(),
                store: store.clone(),
                collector: collector.clone(),
                kick: kick_tx.clone(),
                results: results_tx.clone(),
                rstats: ResolveStats::default(),
                info: info.clone(),
            };
            handles.push(std::thread::Builder::new().name(format!("matcher-{i}")).spawn(move || m.run(rx)).unwrap());
        }

        Engine {
            matchers: matcher_tx,
            pairers: pairer_tx,
            handles,
            registry,
            collector,
            kick_rx,
            results: results_rx,
            info,
            seq: AtomicU64::new(0),
            deterministic,
            dirty: AtomicBool::new(false),
            dirty_pairs: AtomicBool::new(false),
            unflushed_min: AtomicF64::new(f64::INFINITY),
            mlat_wanted_ts: AtomicF64::new(start),
            stats,
        }
    }

    /// Next sequence number; every TICK_EVERY events all matchers are told how
    /// far the stream has got so idle ones do not hold the pairers back.
    fn next_seq(&self) -> u64 {
        let s = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        if s % TICK_EVERY == 0 {
            for m in &self.matchers {
                let _ = m.send(MatchMsg::Tick { seq: s });
            }
        }
        s
    }

    /// Notes that work stamped `now` has been handed to the pipeline.
    fn note_dispatch(&self, now: f64) {
        if !self.deterministic {
            return; // only the test mode needs to know how far the pipeline has got
        }
        self.dirty.store(true, Ordering::Relaxed);
        self.dirty_pairs.store(true, Ordering::Relaxed);
        if now < self.unflushed_min.load() {
            self.unflushed_min.store(now);
        }
    }

    /// Registers a receiver; None if its clock type is unknown.
    pub fn connect(&self, user: &str, lat: f64, lon: f64, alt: f64, clock_type: &str) -> Option<u32> {
        let clock = Clock::from_type(clock_type)?;
        let uid = self.registry.len() as u32;
        self.registry.push(Arc::new(Receiver::new(uid, user.to_string(), lat, lon, alt, clock)));
        Some(uid)
    }

    pub fn disconnect(&self, uid: u32) {
        let Some(r) = self.registry.get(uid) else { return };
        if r.is_dead() {
            return;
        }
        r.dead.store(true, Ordering::Relaxed);
        let seq = self.next_seq();
        for m in &self.matchers {
            let _ = m.send(MatchMsg::Disconnect { seq, uid });
        }
        let seq = self.next_seq();
        let _ = self.matchers[0].send(MatchMsg::Control { seq, c: PairControl::Disconnect(uid) });
        self.dirty.store(true, Ordering::Relaxed);
        self.dirty_pairs.store(true, Ordering::Relaxed);
    }

    /// A receiver that is still connected.
    fn receiver(&self, uid: u32) -> Option<Arc<Receiver>> {
        self.registry.get(uid).filter(|r| !r.is_dead())
    }

    pub fn sync(&self, now: f64, uid: u32, et: f64, ot: f64, em: [u8; 14], om: [u8; 14]) {
        let Some(recv) = self.receiver(uid) else { return };
        self.sync_from(&recv, now, et, ot, em, om);
    }

    /// `sync` for a caller that already holds the receiver (saves a lookup).
    pub fn sync_from(&self, recv: &Arc<Receiver>, now: f64, et: f64, ot: f64, em: [u8; 14], om: [u8; 14]) {
        if recv.is_dead() {
            return;
        }
        self.note_dispatch(now);
        let icao = ((em[1] as u32) << 16) | ((em[2] as u32) << 8) | em[3] as u32;
        let shard = icao_shard(icao, self.matchers.len());
        let seq = self.next_seq();
        let _ = self.matchers[shard].send(MatchMsg::Sync { seq, m: SyncMsg { now, recv: recv.clone(), et, ot, em, om } });
    }

    /// A message the receiver heard that is wanted for multilateration.
    pub fn mlat(&self, now: f64, uid: u32, ts: f64, msg: Vec<u8>) {
        let Some(recv) = self.receiver(uid) else { return };
        self.mlat_from(&recv, now, ts, msg);
    }

    /// `mlat` for a caller that already holds the receiver.
    pub fn mlat_from(&self, recv: &Arc<Receiver>, now: f64, ts: f64, msg: Vec<u8>) {
        if recv.is_dead() {
            return;
        }
        if self.deterministic && self.dirty_pairs.load(Ordering::Relaxed) {
            // the gate below reads clock-sync state the pairers maintain
            self.flush();
        }
        if recv.bad_syncs.load() > 0.0 || now - recv.last_sync.load() > 20.0 {
            return;
        }
        self.dispatch_mlat(recv, now, ts, msg);
    }

    /// `mlat_from` without the bad-sync / stale-sync gate: the Python server's
    /// UDP path hands messages to the coordinator unchecked.
    pub fn mlat_from_ungated(&self, recv: &Arc<Receiver>, now: f64, ts: f64, msg: Vec<u8>) {
        if recv.is_dead() {
            return;
        }
        if self.deterministic && self.dirty_pairs.load(Ordering::Relaxed) {
            self.flush();
        }
        self.dispatch_mlat(recv, now, ts, msg);
    }

    fn dispatch_mlat(&self, recv: &Arc<Receiver>, now: f64, ts: f64, msg: Vec<u8>) {
        let uid = recv.uid;
        self.note_dispatch(now);
        let address = decode_any(&msg).map_or(0, |d| d.address);
        let shard = icao_shard(address, self.matchers.len());
        let seq = self.next_seq();
        let _ = self.matchers[shard].send(MatchMsg::Mlat { seq, uid, ts, msg, now });
    }

    fn split(&self, icaos: &[u32]) -> Vec<Vec<u32>> {
        let mut by = vec![Vec::new(); self.matchers.len()];
        for &i in icaos {
            by[icao_shard(i, self.matchers.len())].push(i);
        }
        by
    }

    pub fn seen(&self, now: f64, uid: u32, icaos: &[u32]) {
        let Some(recv) = self.receiver(uid) else { return };
        self.note_dispatch(now);
        let seq = self.next_seq();
        for (shard, set) in self.split(icaos).into_iter().enumerate() {
            if !set.is_empty() {
                let _ = self.matchers[shard].send(MatchMsg::Seen { seq, now, uid, icaos: set });
            }
        }
        if !recv.has_rate_report.load(Ordering::Relaxed) {
            self.update_interest(now);
        }
    }

    pub fn lost(&self, now: f64, uid: u32, icaos: &[u32]) {
        let Some(recv) = self.receiver(uid) else { return };
        self.note_dispatch(now);
        let seq = self.next_seq();
        for (shard, set) in self.split(icaos).into_iter().enumerate() {
            if !set.is_empty() {
                let _ = self.matchers[shard].send(MatchMsg::Lost { seq, uid, icaos: set });
            }
        }
        if !recv.has_rate_report.load(Ordering::Relaxed) {
            self.update_interest(now);
        }
    }

    /// A rate report arrived: the client now gets selective interest, and the
    /// set of aircraft wanted for multilateration is refreshed.
    pub fn rate_report(&self, now: f64, uid: u32) {
        let Some(recv) = self.receiver(uid) else { return };
        recv.has_rate_report.store(true, Ordering::Relaxed);
        self.update_interest(now);
    }

    /// The part of `Tracker.update_interest` that decides which aircraft want
    /// multilateration. Recomputed at most every 0.1 s.
    fn update_interest(&self, now: f64) {
        if now - self.mlat_wanted_ts.load() <= 0.1 {
            return;
        }
        if self.deterministic {
            self.flush(); // last_adsb_time is written by the pairers
        }
        let seq = self.next_seq();
        for m in &self.matchers {
            let _ = m.send(MatchMsg::RecomputeWanted { seq, now });
        }
        self.mlat_wanted_ts.store(now);
        self.note_dispatch(now);
        if self.deterministic {
            self.barrier();
        }
    }

    /// The receiver's clock sync state must be restarted (reconnect, clock jump).
    pub fn clock_reset(&self, uid: u32) {
        if self.receiver(uid).is_some() {
            let seq = self.next_seq();
            let _ = self.matchers[0].send(MatchMsg::Control { seq, c: PairControl::ResetReceiver(uid) });
            self.dirty_pairs.store(true, Ordering::Relaxed);
        }
    }

    /// Periodic pairing cleanup (every 10 s in the server).
    pub fn cleanup(&self, now: f64) {
        let seq = self.next_seq();
        let _ = self.matchers[0].send(MatchMsg::Control { seq, c: PairControl::Cleanup(now) });
        self.dirty_pairs.store(true, Ordering::Relaxed);
    }

    /// Forget all sync points (every 15 s in the server).
    pub fn clear_sync_points(&self) {
        let seq = self.next_seq();
        for m in &self.matchers {
            let _ = m.send(MatchMsg::ClearSyncPoints { seq });
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Waits until everything sent to the matchers has been processed, and
    /// tells the pairers that no more work with an earlier sequence number is
    /// coming.
    pub fn barrier(&self) {
        let seq = self.next_seq();
        let (tx, rx) = unbounded();
        for m in &self.matchers {
            let _ = m.send(MatchMsg::Barrier { seq, done: tx.clone() });
        }
        for _ in &self.matchers {
            let _ = rx.recv();
        }
        self.dirty.store(false, Ordering::Relaxed);
        self.unflushed_min.store(f64::INFINITY);
    }

    /// Waits until the matchers and the pairers have processed everything
    /// sent so far.
    pub fn flush(&self) {
        self.barrier();
        let (tx, rx) = unbounded();
        for p in &self.pairers {
            let _ = p.send(PairMsg::Flush(tx.clone()));
        }
        for _ in &self.pairers {
            let _ = rx.recv();
        }
        self.dirty_pairs.store(false, Ordering::Relaxed);
    }

    /// The time of the next group-collection deadline that falls at or before
    /// `limit`, if any. In deterministic mode this first lets the matchers
    /// finish, since a deadline may depend on copies still in their queues.
    pub fn next_due_before(&self, limit: f64) -> Option<f64> {
        if self.deterministic && self.dirty.load(Ordering::Relaxed) {
            let known = self.collector.lock().unwrap().next_due();
            let earliest_hidden = self.unflushed_min.load() + crate::collector::MLAT_DELAY;
            if known.is_some_and(|d| d <= limit) || earliest_hidden <= limit {
                self.barrier();
            }
        }
        self.collector.lock().unwrap().next_due().filter(|&d| d <= limit)
    }

    /// Sleeps until `timeout` passes or a group arrives that may be due before then.
    pub fn wait_for_work(&self, timeout: std::time::Duration) {
        let _ = self.kick_rx.recv_timeout(timeout);
    }

    /// When the next group-collection deadline falls, if any group is waiting.
    pub fn peek_due(&self) -> Option<f64> {
        self.collector.lock().unwrap().next_due()
    }

    /// Releases the cohort that is due next to the resolvers.
    pub fn process_next_cohort(&self) {
        if self.deterministic {
            self.flush();
        }
        let Some((due, groups)) = self.collector.lock().unwrap().pop_cohort() else { return };
        let seq = self.next_seq();
        for g in groups {
            if g.dummy {
                continue;
            }
            // a message whose address cannot be decoded resolves to nothing
            let Some(d) = decode_any(&g.message) else { continue };
            let shard = icao_shard(d.address, self.matchers.len());
            let _ = self.matchers[shard].send(MatchMsg::Resolve { seq, group: g, now: due });
        }
        self.dirty.store(true, Ordering::Relaxed);
        if self.deterministic {
            self.barrier();
        }
    }

    /// A handle for blocking consumption of results.
    pub fn results_receiver(&self) -> Rx<MlatResult> {
        self.results.clone()
    }

    pub fn ac_info(&self) -> Arc<AcInfoStore> {
        self.info.clone()
    }

    /// The receivers that are still connected.
    pub fn live_receivers(&self) -> Vec<Arc<Receiver>> {
        self.registry.live()
    }

    /// Results produced so far.
    pub fn take_results(&self) -> Vec<MlatResult> {
        self.results.try_iter().collect()
    }

    /// The 15 s health evaluation (`_write_state` in coordinator.py, without
    /// the file output): per-aircraft sync statistics, then per-receiver
    /// judgement of how well each receiver's clock agrees with its peers'.
    pub fn health(&self, now: f64) -> HealthReport {
        self.flush();
        let seq = self.next_seq();
        let (atx, arx) = unbounded();
        for m in &self.matchers {
            let _ = m.send(MatchMsg::AircraftHealth { seq, reply: atx.clone() });
        }
        self.barrier();
        let mut report = HealthReport::default();
        for _ in &self.matchers {
            report.aircraft.extend(arx.recv().unwrap_or_default());
        }

        // what every pairing says, read before any receiver is updated
        let (tx, rx) = unbounded();
        for p in &self.pairers {
            let _ = p.send(PairMsg::Dump { now, reply: tx.clone() });
        }
        let mut rows = Vec::new();
        for _ in &self.pairers {
            rows.extend(rx.recv().unwrap_or_default());
        }
        rows.sort_by_key(|r| (r.0, r.1));

        let mut peers: AHashMap<u32, Vec<(PairHealth, PeerReport)>> = AHashMap::new();
        for (a, b, h) in &rows {
            let (Some(ra), Some(rb)) = (self.registry.get(*a), self.registry.get(*b)) else { continue };
            // as seen by a, b is the peer (drift from the base side), and the other way round
            let rep_a = PeerReport { peer: *b, n: h.n, error_us: h.error_us, drift_ppm: h.drift_ppm, peer_bad_syncs: round_dec(rb.bad_syncs.load(), 2), jumped: h.jumped, outlier_percent: h.outlier_percent, age: h.age, peer_total: rb.total_peers() };
            let rep_b = PeerReport { peer: *a, n: h.n, error_us: h.error_us, drift_ppm: h.i_drift_ppm, peer_bad_syncs: round_dec(ra.bad_syncs.load(), 2), jumped: h.jumped, outlier_percent: h.outlier_percent, age: h.age, peer_total: ra.total_peers() };
            peers.entry(*a).or_default().push((*h, rep_a));
            peers.entry(*b).or_default().push((*h, rep_b));
        }

        for r in self.registry.live() {
            let mut bad_peers = 0.0_f64;
            let mut bad_list = Vec::new();
            let mut num_peers = 8.0_f64;
            let mut sum_outlier_percent = 0.0;
            for (_, st) in peers.get(&r.uid).map(|v| v.as_slice()).unwrap_or(&[]) {
                let h = st;
                // skip peers with bad sync, peers not updated recently, peers with few peers
                if st.peer_bad_syncs > 0.0 || h.age > 20 || st.peer_total < 8 {
                    continue;
                }
                sum_outlier_percent += h.outlier_percent;
                num_peers += 1.0;
                if h.jumped
                    || h.outlier_percent > 35.0
                    || (h.n > 10 && h.error_us > 1.2)
                    || (h.n > 3 && h.error_us > 1.8)
                    || h.error_us > 2.4
                {
                    bad_peers += 1.0;
                    bad_list.push(st.peer);
                }
            }
            report.outlier_sum += sum_outlier_percent / 100.0;
            report.sync_sum += num_peers;
            report.bad_peers.insert(r.uid, bad_list);

            let outlier_percent = sum_outlier_percent / num_peers;
            let rolling = r.outlier_percent_rolling.load();
            r.outlier_percent_rolling.store(rolling - 0.1 * (rolling - outlier_percent));

            // if your sync with many peers is bad, you are probably the reason
            if bad_peers / num_peers > 0.15 && bad_peers > 3.0 {
                r.bad_syncs.add((0.5_f64).min(2.0 * bad_peers / num_peers) + 0.1);
            }
            r.bad_syncs.add(-0.1);
            r.bad_syncs.store((0.0_f64).max((6.0_f64).min(r.bad_syncs.load())));
            if r.bad_syncs.load() > 0.0 {
                report.bad_receivers += 1;
            }

            r.recent_pair_jumps.store(0, Ordering::Relaxed);
            // almost reset the counters, keeping a bit of the last interval
            r.num_outliers.store(r.num_outliers.load() * 0.25);
            r.num_syncs.store(r.num_syncs.load() * 0.25);
        }
        report.peers = peers.into_iter().map(|(k, v)| (k, v.into_iter().map(|(_, p)| p).collect())).collect();
        report
    }

    /// A receiver by uid, whether or not it is still connected.
    pub fn receiver_info(&self, uid: u32) -> Option<Arc<Receiver>> {
        self.registry.get(uid)
    }

    /// Number of aircraft currently wanted for multilateration.
    pub fn mlat_wanted_count(&self) -> usize {
        self.info.shards.iter().map(|s| s.read().unwrap().values().filter(|a| a.do_mlat).count()).sum()
    }

    /// All pairings, sorted by receiver pair, after pending work completes.
    pub fn snapshot(&self) -> Vec<PairRow> {
        self.barrier();
        let (tx, rx) = unbounded();
        for p in &self.pairers {
            let _ = p.send(PairMsg::Snapshot(tx.clone()));
        }
        let mut rows = Vec::new();
        for _ in &self.pairers {
            rows.extend(rx.recv().unwrap_or_default());
        }
        rows.sort_by_key(|r| (r.a, r.b));
        rows
    }

    pub fn shutdown(self) {
        for m in &self.matchers {
            let _ = m.send(MatchMsg::Shutdown);
        }
        for p in &self.pairers {
            let _ = p.send(PairMsg::Shutdown);
        }
        for h in self.handles {
            let _ = h.join();
        }
    }
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<SyncMsg>();
    is_send::<AtomicF64>();
}
