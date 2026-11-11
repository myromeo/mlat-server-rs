//! `--coverage-json`: per-station MLAT coverage in `coverage.json` (work directory, every 60 s). An addition of this port.
//!
//! For each station, the multilateration results its timestamps went into (only results good enough to be sent back to receivers),
//! over the current UTC day and over a rolling 24 hours: how many, how many distinct aircraft, and how far each was from the
//! station: results per 10 nm band in each of 72 bearing sectors of 5 degrees, and each aircraft's furthest band. The file carries
//! counts only: per sector the furthest band with at least MIN_HITS results, band totals, never a position, aircraft or time.
//! Stations using `--privacy` are left out. Keys are station names (the `user` of `clients.json`).
//!
//! The shape is that of readsb-rs `range.json` (mlat.uk's ingest node), so one consumer reads both. No standard defines a station's
//! coverage; band width, sector count and threshold are set here.

use ahash::AHashMap;
use serde_json::{json, Map, Value};

const SECTORS: usize = 72;
const BANDS: usize = 40;
const BAND_NM: f64 = 10.0;
const MIN_HITS: u32 = 3;
const WINDOW_H: i64 = 24;
const NM: f64 = 1852.0;
const EARTH_M: f64 = 6_371_000.0;

type Counts = Box<[[u32; BANDS]; SECTORS]>;

fn counts() -> Counts {
    Box::new([[0; BANDS]; SECTORS])
}

fn reach(c: &Counts) -> Vec<u64> {
    c.iter().map(|s| s.iter().rposition(|&n| n >= MIN_HITS).map_or(0, |b| b as u64 + 1)).collect()
}

fn totals(c: &Counts) -> Vec<u64> {
    let mut bp = vec![0u64; BANDS];
    for s in c.iter() {
        for (b, n) in s.iter().enumerate() {
            bp[b] += *n as u64;
        }
    }
    bp
}

fn trim(mut v: Vec<u64>) -> Vec<u64> {
    while v.last() == Some(&0) {
        v.pop();
    }
    v
}

fn entry(pos: u64, ac: usize, sec: Vec<u64>, bp: Vec<u64>, ba: Vec<u64>) -> Value {
    json!({ "pos": pos, "ac": ac, "sec": sec, "bp": trim(bp), "ba": trim(ba) })
}

#[derive(Default)]
struct Day {
    pos: u64,
    aircraft: AHashMap<u32, u8>,
    counts: Option<Counts>,
}

struct Hour {
    hour: i64,
    pos: u64,
    sec: Vec<u64>,
    bp: Vec<u64>,
}

struct Recent {
    hours: Vec<Hour>,
    hour: i64,
    pos: u64,
    counts: Counts,
    /// aircraft -> (furthest band + 1 while heard within the window, last hour)
    aircraft: AHashMap<u32, (u8, i64)>,
}

impl Recent {
    fn close(&mut self, hour: i64) {
        if self.pos > 0 {
            self.hours.push(Hour { hour: self.hour, pos: self.pos, sec: reach(&self.counts), bp: totals(&self.counts) });
        }
        self.hours.retain(|h| h.hour > hour - WINDOW_H);
        self.hour = hour;
        self.pos = 0;
        *self.counts = [[0; BANDS]; SECTORS];
        self.aircraft.retain(|_, e| e.1 > hour - WINDOW_H);
    }
}

pub struct Coverage {
    started: i64,
    day: i64,
    cur: AHashMap<String, Day>,
    prev: Option<Value>,
    recent: AHashMap<String, Recent>,
}

/// Distance (m, haversine) and bearing (degrees from north) from (lat0, lon0) to (lat1, lon1)
fn dist_bearing(lat0: f64, lon0: f64, lat1: f64, lon1: f64) -> (f64, f64) {
    let (p0, p1, dl) = (lat0.to_radians(), lat1.to_radians(), (lon1 - lon0).to_radians());
    let a = ((p1 - p0) / 2.0).sin().powi(2) + p0.cos() * p1.cos() * (dl / 2.0).sin().powi(2);
    let d = 2.0 * EARTH_M * a.sqrt().min(1.0).asin();
    let b = (dl.sin() * p1.cos()).atan2(p0.cos() * p1.sin() - p0.sin() * p1.cos() * dl.cos()).to_degrees();
    (d, (b + 360.0) % 360.0)
}

impl Coverage {
    pub fn new(now_ms: i64) -> Self {
        Coverage { started: now_ms, day: now_ms.div_euclid(86_400_000), cur: AHashMap::new(), prev: None, recent: AHashMap::new() }
    }

    /// One result that used station `user` (at `st`) for aircraft `addr` at (`lat`, `lon`).
    pub fn add(&mut self, user: &str, st: (f64, f64), addr: u32, lat: f64, lon: f64, now_ms: i64) {
        let day = now_ms.div_euclid(86_400_000);
        if day != self.day {
            self.roll(day);
        }
        let (d, b) = dist_bearing(st.0, st.1, lat, lon);
        let band = ((d / (BAND_NM * NM)) as usize).min(BANDS - 1);
        let sector = (b * SECTORS as f64 / 360.0) as usize % SECTORS;
        let f = band as u8 + 1;

        let e = self.cur.entry(user.to_string()).or_default();
        e.pos += 1;
        e.counts.get_or_insert_with(counts)[sector][band] += 1;
        let a = e.aircraft.entry(addr).or_insert(0);
        *a = (*a).max(f);

        let hour = now_ms.div_euclid(3_600_000);
        let r = self.recent.entry(user.to_string()).or_insert_with(|| Recent { hours: Vec::new(), hour, pos: 0, counts: counts(), aircraft: AHashMap::new() });
        if hour > r.hour {
            r.close(hour);
        }
        r.pos += 1;
        r.counts[sector][band] += 1;
        let a = r.aircraft.entry(addr).or_insert((0, hour));
        if a.1 <= hour - WINDOW_H {
            *a = (0, hour);
        }
        a.0 = a.0.max(f);
        a.1 = hour;
    }

    fn roll(&mut self, day: i64) {
        if day > self.day && !self.cur.is_empty() {
            self.prev = Some(self.render());
        }
        self.cur.clear();
        self.day = day;
    }

    fn render(&self) -> Value {
        let mut m = Map::new();
        for (user, d) in &self.cur {
            let mut ba = vec![0u64; BANDS];
            for &f in d.aircraft.values() {
                if f > 0 {
                    ba[f as usize - 1] += 1;
                }
            }
            let c = d.counts.as_ref().expect("a counted day has counts");
            m.insert(user.clone(), entry(d.pos, d.aircraft.len(), reach(c), totals(c), ba));
        }
        json!({ "day": self.day, "receivers": m })
    }

    /// The whole file: `{now, started, band_nm, sectors, min_hits, cur, prev, last24}`.
    pub fn json(&mut self, now_ms: i64) -> Value {
        let day = now_ms.div_euclid(86_400_000);
        if day != self.day {
            self.roll(day);
        }
        let hour = now_ms.div_euclid(3_600_000);
        let mut last = Map::new();
        self.recent.retain(|_, r| {
            if hour > r.hour {
                r.close(hour);
            }
            r.pos > 0 || !r.hours.is_empty()
        });
        for (user, r) in &self.recent {
            let mut pos = r.pos;
            let mut sec = reach(&r.counts);
            let mut bp = totals(&r.counts);
            for h in &r.hours {
                pos += h.pos;
                for i in 0..SECTORS {
                    sec[i] = sec[i].max(h.sec[i]);
                }
                for i in 0..BANDS {
                    bp[i] += h.bp[i];
                }
            }
            let mut ba = vec![0u64; BANDS];
            for &(f, _) in r.aircraft.values() {
                if f > 0 {
                    ba[f as usize - 1] += 1;
                }
            }
            last.insert(user.clone(), entry(pos, r.aircraft.len(), sec, bp, ba));
        }
        json!({
            "now": now_ms, "started": self.started, "band_nm": BAND_NM, "sectors": SECTORS, "min_hits": MIN_HITS,
            "cur": self.render(), "prev": self.prev.clone().unwrap_or(Value::Null), "last24": { "receivers": last },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_rolls() {
        let t0 = 20_000 * 86_400_000i64;
        let mut c = Coverage::new(t0);
        // 1 degree north is 60.04 nm: band 6
        for _ in 0..3 {
            c.add("station-a", (55.0, -1.6), 0xabcdef, 56.0, -1.6, t0 + 10);
        }
        let j = c.json(t0 + 2 * 3_600_000);
        let a = &j["cur"]["receivers"]["station-a"];
        assert_eq!(a["pos"], 3);
        assert_eq!(a["ac"], 1);
        assert_eq!(a["sec"][0], 7);
        assert_eq!(j["last24"]["receivers"]["station-a"]["pos"], 3);
        let j = c.json(t0 + 25 * 3_600_000);
        assert_eq!(j["prev"]["day"], 20_000);
        assert!(j["last24"]["receivers"].as_object().unwrap().is_empty());
    }
}
