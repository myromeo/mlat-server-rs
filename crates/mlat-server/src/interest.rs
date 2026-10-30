//! Interest selection: which aircraft each receiver is asked to send, ported
//! from `Tracker.update_interest` (the part after the wanted set is worked out)
//! and `Receiver.update_interest_sets` / `request_traffic`.
//!
//! One worker thread owns this state. It is fed the receivers' seen/lost lists
//! and rate reports, and tells each connection what to start and stop sending.

use crate::state::{Line, Rng, Server};
use ahash::{AHashMap, AHashSet};
use crossbeam_channel::{Receiver as Rx, Sender};
use std::sync::Arc;

const MAX_SYNC_AC: usize = 15;
const MAX_SYNC_RATE: f64 = 12.0;

pub enum InterestMsg {
    Seen { now: f64, uid: u32, icaos: Vec<u32> },
    Lost { now: f64, uid: u32, icaos: Vec<u32> },
    Rate { now: f64, uid: u32, rates: Vec<(u32, f64)> },
    Disconnect { uid: u32 },
    Snapshot { reply: Sender<InterestSnapshot> },
}

/// What the state files report about interest.
#[derive(Default)]
pub struct InterestSnapshot {
    pub rx_sync: AHashMap<u32, Vec<u32>>,
    pub rx_mlat: AHashMap<u32, Vec<u32>>,
    pub ac_sync: AHashMap<u32, usize>,
    pub ac_mlat: AHashMap<u32, usize>,
    pub ac_adsb: AHashMap<u32, usize>,
    pub ac_seen: AHashMap<u32, f64>,
}

#[derive(Default)]
struct Rx_ {
    tracking: AHashSet<u32>,
    rates: Option<AHashMap<u32, f64>>,
    sync: AHashSet<u32>,
    mlat: AHashSet<u32>,
    adsb: AHashSet<u32>,
    /// What the connection has been told to send.
    sent: AHashSet<u32>,
}

#[derive(Default)]
struct Ac {
    tracking: AHashSet<u32>,
    sync: AHashSet<u32>,
    mlat: AHashSet<u32>,
    adsb: AHashSet<u32>,
    seen: f64,
}

struct Worker {
    server: Arc<Server>,
    rx: AHashMap<u32, Rx_>,
    ac: AHashMap<u32, Ac>,
    rng: Rng,
}

pub fn run(server: Arc<Server>, rx: Rx<InterestMsg>) {
    let mut w = Worker { server, rx: AHashMap::new(), ac: AHashMap::new(), rng: Rng::new() };
    while let Ok(msg) = rx.recv() {
        match msg {
            InterestMsg::Seen { now, uid, icaos } => {
                let r = w.rx.entry(uid).or_default();
                for icao in icaos {
                    r.tracking.insert(icao);
                    let ac = w.ac.entry(icao).or_default();
                    ac.tracking.insert(uid);
                    ac.seen = now;
                }
                if w.rx[&uid].rates.is_none() {
                    w.recompute(uid, now);
                }
            }
            InterestMsg::Lost { now, uid, icaos } => {
                let r = w.rx.entry(uid).or_default();
                for icao in icaos {
                    r.tracking.remove(&icao);
                    if let Some(ac) = w.ac.get_mut(&icao) {
                        ac.tracking.remove(&uid);
                        if ac.tracking.is_empty() {
                            w.ac.remove(&icao);
                        }
                    }
                }
                if w.rx[&uid].rates.is_none() {
                    w.recompute(uid, now);
                }
            }
            InterestMsg::Rate { now, uid, rates } => {
                w.rx.entry(uid).or_default().rates = Some(rates.into_iter().collect());
                w.recompute(uid, now);
            }
            InterestMsg::Disconnect { uid } => {
                if let Some(r) = w.rx.remove(&uid) {
                    for icao in r.tracking {
                        if let Some(ac) = w.ac.get_mut(&icao) {
                            ac.tracking.remove(&uid);
                            ac.sync.remove(&uid);
                            ac.mlat.remove(&uid);
                            ac.adsb.remove(&uid);
                            if ac.tracking.is_empty() {
                                w.ac.remove(&icao);
                            }
                        }
                    }
                }
            }
            InterestMsg::Snapshot { reply } => {
                let mut s = InterestSnapshot::default();
                for (uid, r) in &w.rx {
                    s.rx_sync.insert(*uid, r.sync.iter().copied().collect());
                    s.rx_mlat.insert(*uid, r.mlat.iter().copied().collect());
                }
                for (icao, a) in &w.ac {
                    s.ac_sync.insert(*icao, a.sync.len());
                    s.ac_mlat.insert(*icao, a.mlat.len());
                    s.ac_adsb.insert(*icao, a.adsb.len());
                    s.ac_seen.insert(*icao, a.seen);
                }
                let _ = reply.send(s);
            }
        }
    }
}

impl Worker {
    fn recompute(&mut self, uid: u32, now: f64) {
        let Some(recv) = self.server.engine.receiver_info(uid) else { return };
        let info = self.server.engine.ac_info();
        let bad_syncs = recv.bad_syncs.load();
        let Some(rx) = self.rx.get(&uid) else { return };

        let new_mlat: AHashSet<u32> = rx.tracking.iter().copied().filter(|i| info.get(*i).is_some_and(|a| a.do_mlat)).collect();
        let mut new_adsb: AHashSet<u32> = AHashSet::new();
        let mut new_sync: AHashSet<u32> = AHashSet::new();

        match &rx.rates {
            None => {
                // legacy client without rate reports: cannot be very selective
                let v: Vec<u32> = rx.tracking.iter().copied().collect();
                new_sync = if v.len() > MAX_SYNC_AC { self.rng.sample(&v, MAX_SYNC_AC).into_iter().collect() } else { v.into_iter().collect() };
            }
            Some(rates) => {
                // (rate product, other receiver, aircraft, this receiver's rate)
                type Pair = (f64, u32, u32, f64);
                let mut by_ac: AHashMap<u32, Vec<Pair>> = AHashMap::new();
                let mut all: Vec<Pair> = Vec::new();
                for (&icao, &rate) in rates {
                    let Some(ac) = self.ac.get_mut(&icao) else { continue };
                    ac.seen = now;
                    new_adsb.insert(icao);

                    let alt_factor = info.get(icao).and_then(|a| a.altitude).filter(|&a| a > 0).map(|a| 1.0 + (a as f64 / 20000.0).powf(1.5));
                    let list = by_ac.entry(icao).or_default();
                    for &r1 in &ac.tracking {
                        if r1 == uid {
                            continue;
                        }
                        let rate1 = match self.rx.get(&r1).and_then(|r| r.rates.as_ref()) {
                            None => 0.8, // receiver without rate reports: just guess
                            Some(m) => m.get(&icao).copied().unwrap_or(0.0),
                        };
                        let mut rp = rate * rate1 / 2.25;
                        if let Some(f) = alt_factor {
                            rp *= f;
                        }
                        if rp < 0.01 {
                            continue;
                        }
                        list.push((rp, r1, icao, rate));
                        all.push((rp, r1, icao, rate));
                    }
                }
                all.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
                let split = all.len() / 2;
                let mut first_half: Vec<Pair> = all[..split].to_vec();
                self.rng.shuffle(&mut first_half);

                let mut ntotal: AHashMap<u32, f64> = AHashMap::new();
                let mut total_rate = 0.0;
                let add = |new_sync: &mut AHashSet<u32>, ntotal: &mut AHashMap<u32, f64>, total_rate: &mut f64, icao: u32, rate: f64| {
                    new_sync.insert(icao);
                    *total_rate += rate;
                    for (rp2, r2, _, _) in &by_ac[&icao] {
                        *ntotal.entry(*r2).or_insert(0.0) += rp2;
                    }
                };

                // round 1: aircraft that fill the receivers' needs most evenly
                for &(_, r1, icao, rate) in &first_half {
                    if new_sync.contains(&icao) {
                        continue;
                    }
                    if info.get(icao).is_some_and(|a| a.sync_dont_use) {
                        continue; // not aircraft with sub-par GPS in the first round
                    }
                    if total_rate > MAX_SYNC_RATE {
                        break;
                    }
                    if ntotal.get(&r1).copied().unwrap_or(0.0) < 0.3 {
                        add(&mut new_sync, &mut ntotal, &mut total_rate, icao, rate);
                    }
                }
                // round 2: less strict
                for &(_, r1, icao, rate) in &all {
                    if new_sync.contains(&icao) {
                        continue;
                    }
                    if total_rate > MAX_SYNC_RATE {
                        break;
                    }
                    if ntotal.get(&r1).copied().unwrap_or(0.0) < 3.5 {
                        add(&mut new_sync, &mut ntotal, &mut total_rate, icao, rate);
                    }
                }

                // always have a few aircraft to sync on
                let want = MAX_SYNC_AC / 4;
                if new_sync.len() < want {
                    let avail: Vec<u32> = by_ac.keys().copied().filter(|i| !new_sync.contains(i)).collect();
                    new_sync.extend(self.rng.sample(&avail, want - new_sync.len()));
                    if new_sync.len() < want {
                        let avail: Vec<u32> = rx.tracking.iter().copied().filter(|i| !new_sync.contains(i)).collect();
                        new_sync.extend(self.rng.sample(&avail, want - new_sync.len()));
                    }
                }
            }
        }

        self.apply(uid, new_sync, new_mlat, new_adsb, bad_syncs);
    }

    /// `update_interest_sets` followed by `request_traffic`/`send_traffic_updates`.
    fn apply(&mut self, uid: u32, mut new_sync: AHashSet<u32>, mut new_mlat: AHashSet<u32>, new_adsb: AHashSet<u32>, bad_syncs: f64) {
        if bad_syncs > 3.0 && new_sync.len() > 3 {
            let v: Vec<u32> = new_sync.iter().copied().collect();
            new_sync = self.rng.sample(&v, 3).into_iter().collect();
        }
        if bad_syncs > 0.0 {
            new_mlat.clear();
        }

        let Some(r) = self.rx.get_mut(&uid) else { return };
        let ac = &mut self.ac;
        let mut relink = |old: &AHashSet<u32>, new: &AHashSet<u32>, pick: fn(&mut Ac) -> &mut AHashSet<u32>| {
            for i in new.difference(old) {
                if let Some(a) = ac.get_mut(i) {
                    pick(a).insert(uid);
                }
            }
            for i in old.difference(new) {
                if let Some(a) = ac.get_mut(i) {
                    pick(a).remove(&uid);
                }
            }
        };
        relink(&r.adsb, &new_adsb, |a| &mut a.adsb);
        relink(&r.sync, &new_sync, |a| &mut a.sync);
        relink(&r.mlat, &new_mlat, |a| &mut a.mlat);
        r.adsb = new_adsb;
        r.sync = new_sync;
        r.mlat = new_mlat;

        let wanted: AHashSet<u32> = r.sync.union(&r.mlat).copied().collect();
        let start: Vec<u32> = wanted.difference(&r.sent).copied().collect();
        let stop: Vec<u32> = r.sent.difference(&wanted).copied().collect();
        r.sent = wanted;

        let hex = |v: &[u32]| v.iter().map(|i| format!("\"{i:06x}\"")).collect::<Vec<_>>().join(",");
        if !start.is_empty() {
            self.server.send_to(uid, Line::from(format!("{{\"start_sending\":[{}]}}", hex(&start))));
        }
        if !stop.is_empty() {
            self.server.send_to(uid, Line::from(format!("{{\"stop_sending\":[{}]}}", hex(&stop))));
        }
    }
}
