//! State shared by the server's threads.

use crate::interest::InterestMsg;
use ahash::AHashMap;
use crossbeam_channel::Sender;
use mlat_core::atomic::AtomicF64;
use mlat_core::engine::Engine;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, Notify};

/// A line to send to a client (without the newline).
pub type Line = Arc<str>;

/// Lines queued for one receiver (about 2 MB of results) before it counts as stalled.
pub const CLIENT_QUEUE: usize = 8192;
/// Lines queued for one Basestation peer (about 5 MB) before it is dropped.
pub const SBS_QUEUE: usize = 32768;
/// How long one write to a peer may take before the peer counts as gone.
pub const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `--trusted-proxy`: an address range whose `PROXY` lines are believed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cidr {
    net: IpAddr,
    bits: u8,
}

impl Cidr {
    /// `a.b.c.d`, `a.b.c.d/n`, `x:y::z` or `x:y::z/n`.
    pub fn parse(s: &str) -> Result<Cidr, String> {
        let bad = || format!("'{s}' is not an address or address/prefix");
        let (addr, bits) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b.parse::<u8>().map_err(|_| bad())?)),
            None => (s, None),
        };
        let net: IpAddr = addr.parse().map_err(|_| bad())?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        if bits > max {
            return Err(bad());
        }
        Ok(Cidr { net, bits })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let mask = |bits: u8, width: u32| if bits == 0 { 0 } else { u128::MAX << (width - bits as u32) };
        match (self.net, ip.to_canonical()) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let m = mask(self.bits, 32) as u32;
                u32::from(n) & m == u32::from(a) & m
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let m = mask(self.bits, 128);
                u128::from(n) & m == u128::from(a) & m
            }
            _ => false,
        }
    }
}

pub struct Config {
    pub work_dir: PathBuf,
    pub motd: String,
    pub source_url: String,
    pub status_interval: f64,
    /// `--partition I/N`
    pub partition: (u32, u32),
    /// Write aircraft.json (about 1 MB every 15 s); nothing in the stack needs to read it.
    pub aircraft_json: bool,
    /// Peers allowed to send a `PROXY` line; empty means none.
    pub trusted_proxies: Vec<Cidr>,
}

/// One connected receiver.
pub struct ConnInfo {
    pub recv: Arc<mlat_core::receiver::Receiver>,
    pub uid: u32,
    pub user: String,
    pub uuid: Option<String>,
    pub privacy: bool,
    pub lat: f64,
    pub lon: f64,
    pub alt: f64,
    pub connection_info: String,
    pub source_ip: String,
    pub source_port: String,
    /// The position shown publicly, deliberately blurred; None for private receivers.
    pub map_lat: Option<f64>,
    pub map_lon: Option<f64>,
    pub return_results: bool,
    pub result_format_ecef: bool,
    pub return_stats: bool,
    pub tx: mpsc::Sender<Line>,
    pub msg_counter: AtomicU64,
    pub last_msg: AtomicF64,
    pub close: Notify,
    /// Set once the connection has been told to close because it stopped taking data.
    pub closing: AtomicBool,
    /// Key of this receiver in the UDP table, if it uses the UDP transport.
    pub udp_key: Option<u32>,
}

impl ConnInfo {
    /// Queues a line for the receiver. A receiver whose queue is full (it has
    /// stopped reading) or whose writer has gone is disconnected.
    pub fn send(&self, line: Line) {
        let reason = match self.tx.try_send(line) {
            Ok(()) => return,
            Err(TrySendError::Full(_)) => "Send queue full, closing connection",
            Err(TrySendError::Closed(_)) => "Write failed, closing connection",
        };
        if !self.closing.swap(true, Ordering::Relaxed) {
            log("client", &format!("[{}:{}] {reason}", self.source_ip, self.source_port));
            self.close.notify_one();
        }
    }
}

pub struct SbsClient {
    pub tx: mpsc::Sender<Line>,
    /// True for the filtered (Kalman-smoothed) listener.
    pub filtered: bool,
}

pub struct Server {
    pub engine: Engine,
    pub cfg: Config,
    pub conns: RwLock<AHashMap<u32, Arc<ConnInfo>>>,
    /// user name -> (uid, uuid), for unique user names
    pub users: Mutex<AHashMap<String, (u32, Option<String>)>>,
    pub interest: Sender<InterestMsg>,
    pub sbs_clients: Mutex<Vec<SbsClient>>,
    /// UDP key -> receiver, for receivers using the UDP transport
    pub udp_clients: RwLock<AHashMap<u32, Arc<ConnInfo>>>,
    handshake_log: Mutex<()>,
}

impl Server {
    pub fn new(engine: Engine, cfg: Config, interest: Sender<InterestMsg>) -> Server {
        Server {
            engine,
            cfg,
            conns: RwLock::new(AHashMap::new()),
            users: Mutex::new(AHashMap::new()),
            interest,
            sbs_clients: Mutex::new(Vec::new()),
            udp_clients: RwLock::new(AHashMap::new()),
            handshake_log: Mutex::new(()),
        }
    }

    pub fn send_to(&self, uid: u32, line: Line) {
        if let Some(c) = self.conns.read().unwrap().get(&uid) {
            c.send(line);
        }
    }

    /// Appends a handshake line to handshakes.log, rotating at 1 MB (two backups).
    pub fn log_handshake(&self, line: &str) {
        let _g = self.handshake_log.lock().unwrap();
        let path = self.cfg.work_dir.join("handshakes.log");
        if std::fs::metadata(&path).map_or(false, |m| m.len() > 1_048_576) {
            let _ = std::fs::rename(self.cfg.work_dir.join("handshakes.log.1"), self.cfg.work_dir.join("handshakes.log.2"));
            let _ = std::fs::rename(&path, self.cfg.work_dir.join("handshakes.log.1"));
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Log line in the Python server's format: `YYYYMMDD HH:MM:SS.mmm name message`.
pub fn log(name: &str, msg: &str) {
    let t = now();
    let secs = t.floor() as i64;
    let date = mlat_core::output::format_date(t).replace('/', "");
    let sod = secs.rem_euclid(86_400);
    eprintln!("{date} {:02}:{:02}:{:02}.{:03} {name:<12} {msg}", sod / 3600, (sod / 60) % 60, sod % 60, ((t - t.floor()) * 1000.0) as u32);
}

/// A cheap source of randomness (xorshift64*), seeded from the clock.
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Rng {
        Rng(now().to_bits() ^ 0x9E37_79B9_7F4A_7C15 ^ (std::process::id() as u64) << 32 | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n.max(1) as u64) as usize
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }

    /// `k` distinct items of `v`.
    pub fn sample<T: Clone>(&mut self, v: &[T], k: usize) -> Vec<T> {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        let k = k.min(v.len());
        for i in 0..k {
            let j = i + self.below(v.len() - i);
            idx.swap(i, j);
        }
        idx[..k].iter().map(|&i| v[i].clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_matching() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let c = Cidr::parse("10.1.0.0/16").unwrap();
        assert!(c.contains(ip("10.1.200.3")));
        assert!(!c.contains(ip("10.2.0.1")));
        assert!(c.contains(ip("::ffff:10.1.0.9")), "IPv4-mapped peers match IPv4 ranges");
        assert!(Cidr::parse("192.0.2.7").unwrap().contains(ip("192.0.2.7")));
        assert!(!Cidr::parse("192.0.2.7").unwrap().contains(ip("192.0.2.8")));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("203.0.113.1")));
        assert!(Cidr::parse("fd00::/8").unwrap().contains(ip("fd12::1")));
        assert!(!Cidr::parse("fd00::/8").unwrap().contains(ip("10.0.0.1")));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("nonsense").is_err());
    }
}
