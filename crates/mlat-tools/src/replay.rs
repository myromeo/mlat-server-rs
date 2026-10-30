//! Replay a capture file against a server under test.
//!
//! Every captured client becomes a live connection that sends its original
//! handshake (user and uuid prefixed, so it can never collide with or evict a
//! real receiver) and then its original message lines at their original times.
//! Whatever the server sends back is decoded and written to a results file.

use crate::events::Event;
use anyhow::{bail, Context, Result};
use mlat_proto::{CompressMode, FrameEncoder, LineDecoder};
use serde_json::Value;
use std::collections::HashMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::oneshot;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::Instant;

pub struct Opts {
    pub input: PathBuf,
    pub target: String,
    /// 1.0 is real time, 2.0 twice as fast.
    pub speed: f64,
    pub prefix: String,
    pub results: Option<PathBuf>,
    /// Stop dispatching events after this many (capture) seconds.
    pub max_secs: Option<f64>,
    /// Allow a target that is not loopback or a private address.
    pub allow_public: bool,
    /// Send sync and mlat messages as datagrams if the server offers it.
    pub udp: bool,
}

#[derive(Default)]
struct Stats {
    connected: AtomicU64,
    connect_failed: AtomicU64,
    lines_sent: AtomicU64,
    lines_received: AtomicU64,
    send_errors: AtomicU64,
}

#[derive(Debug)]
pub struct Report {
    pub connected: u64,
    pub connect_failed: u64,
    pub lines_sent: u64,
    pub lines_received: u64,
    pub send_errors: u64,
    pub max_lag_secs: f64,
    /// Wall-clock time (Unix epoch) at which capture time 0 was replayed.
    pub start_epoch: f64,
}

enum Out {
    Handshake(String),
    Line(String),
    Close,
}

fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn check_target(target: &str, allow_public: bool) -> Result<()> {
    let addrs: Vec<_> = target
        .to_socket_addrs()
        .with_context(|| format!("resolving target {target}"))?
        .collect();
    if addrs.is_empty() {
        bail!("target {target} did not resolve");
    }
    if !allow_public && !addrs.iter().all(|a| is_local(a.ip())) {
        bail!(
            "refusing to replay at {target}: not a loopback/private address. \
             Replay adds fake receivers to the target server; pass --allow-public only for a test server you own"
        );
    }
    Ok(())
}

/// Replace identifying fields so replayed receivers cannot clash with real ones.
fn rewrite_handshake(line: &str, prefix: &str) -> Result<(String, Vec<String>)> {
    let mut hs: Value = serde_json::from_str(line).context("handshake is not JSON")?;
    let offered: Vec<String> = hs
        .get("compress")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();
    if let Some(obj) = hs.as_object_mut() {
        if let Some(u) = obj.get("user").and_then(|u| u.as_str()).map(str::to_owned) {
            obj.insert("user".into(), Value::String(format!("{prefix}{u}")));
        }
        if let Some(u) = obj.get("uuid").and_then(|u| u.as_str()).map(str::to_owned) {
            obj.insert("uuid".into(), Value::String(format!("{prefix}{u}")));
        }
    }
    Ok((serde_json::to_string(&hs)?, offered))
}

struct ResultRec {
    c: u64,
    t: f64,
    l: String,
}

async fn reader_task(
    mut rd: OwnedReadHalf,
    id: u64,
    start: Instant,
    results: Option<UnboundedSender<ResultRec>>,
    stats: Arc<Stats>,
    mut hs_reply: Option<oneshot::Sender<String>>,
) {
    let mut buf = vec![0u8; 65536];
    let mut head: Vec<u8> = Vec::new();
    let mut dec: Option<LineDecoder> = None;
    loop {
        let n = match rd.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let t = start.elapsed().as_secs_f64();
        let mut lines = Vec::new();
        if let Some(d) = dec.as_mut() {
            if d.push(&buf[..n], &mut lines).is_err() {
                return;
            }
        } else {
            head.extend_from_slice(&buf[..n]);
            let Some(nl) = head.iter().position(|&b| b == b'\n') else { continue };
            let rest = head.split_off(nl + 1);
            head.pop();
            let line = String::from_utf8_lossy(&head).into_owned();
            let framed = serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|v| v.get("compress").and_then(|c| c.as_str()).map(|c| c == "zlib2"))
                .unwrap_or(false);
            if let Some(tx) = hs_reply.take() {
                let _ = tx.send(line.clone());
            }
            lines.push(line);
            let mut d = LineDecoder::new(framed);
            if !rest.is_empty() && d.push(&rest, &mut lines).is_err() {
                return;
            }
            dec = Some(d);
        }
        for l in lines {
            stats.lines_received.fetch_add(1, Ordering::Relaxed);
            if let Some(tx) = &results {
                let _ = tx.send(ResultRec { c: id, t, l });
            }
        }
    }
}

/// A message that can travel in a datagram.
enum Packable {
    Sync { et: f64, ot: f64, em: [u8; 14], om: [u8; 14] },
    Mlat { t: f64, m: Vec<u8> },
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn packable(line: &str) -> Option<Packable> {
    let v: Value = serde_json::from_str(line).ok()?;
    if let Some(s) = v.get("sync") {
        let f = |k: &str| s.get(k)?.as_f64();
        let h = |k: &str| -> Option<[u8; 14]> { unhex(s.get(k)?.as_str()?)?.try_into().ok() };
        return Some(Packable::Sync { et: f("et")?, ot: f("ot")?, em: h("em")?, om: h("om")? });
    }
    let m = v.get("mlat")?;
    let msg = unhex(m.get("m")?.as_str()?)?;
    let t = m.get("t")?.as_f64()?;
    (msg.len() == 7 || msg.len() == 14).then_some(Packable::Mlat { t, m: msg })
}

struct UdpOut {
    sock: UdpSocket,
    key: u32,
    seq: u16,
}

impl UdpOut {
    /// Packs the records into one datagram, relative to the first one's time.
    async fn send(&mut self, recs: &[Packable]) -> std::io::Result<()> {
        let first = match &recs[0] {
            Packable::Sync { et, .. } => *et,
            Packable::Mlat { t, .. } => *t,
        };
        let mut base = first.round().max(0.0) as u64;
        let mut p = Vec::with_capacity(14 + recs.len() * 40);
        p.extend_from_slice(&self.key.to_be_bytes());
        p.extend_from_slice(&self.seq.to_be_bytes());
        p.extend_from_slice(&base.to_be_bytes());
        self.seq = self.seq.wrapping_add(1);
        let rel = |t: f64, base: u64| -> Option<i32> { i32::try_from(t.round() as i64 - base as i64).ok() };
        for r in recs {
            match r {
                Packable::Sync { et, ot, em, om } => match (rel(*et, base), rel(*ot, base)) {
                    (Some(a), Some(b)) => {
                        p.push(1);
                        p.extend_from_slice(&a.to_be_bytes());
                        p.extend_from_slice(&b.to_be_bytes());
                        p.extend_from_slice(em);
                        p.extend_from_slice(om);
                    }
                    _ => {
                        p.push(6);
                        p.extend_from_slice(&(et.round() as u64).to_be_bytes());
                        p.extend_from_slice(&(ot.round() as u64).to_be_bytes());
                        p.extend_from_slice(em);
                        p.extend_from_slice(om);
                    }
                },
                Packable::Mlat { t, m } => {
                    let off = match rel(*t, base) {
                        Some(o) => o,
                        None => {
                            base = t.round().max(0.0) as u64;
                            p.push(5);
                            p.extend_from_slice(&base.to_be_bytes());
                            0
                        }
                    };
                    p.push(if m.len() == 7 { 2 } else { 3 });
                    p.extend_from_slice(&off.to_be_bytes());
                    p.extend_from_slice(m);
                }
            }
        }
        self.sock.send(&p).await.map(|_| ())
    }
}

/// Opens the UDP side if the handshake response offered it.
async fn open_udp(reply: &str, target: &str) -> Option<UdpOut> {
    let v: Value = serde_json::from_str(reply).ok()?;
    let u = v.get("udp_transport")?.as_array()?;
    let host = u.first()?.as_str().map(str::to_owned).or_else(|| target.rsplit_once(':').map(|(h, _)| h.to_owned()))?;
    let port = u.get(1)?.as_u64()?;
    let key = u.get(2)?.as_u64()? as u32;
    let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    sock.connect(format!("{host}:{port}")).await.ok()?;
    Some(UdpOut { sock, key, seq: 0 })
}

async fn conn_task(
    id: u64,
    target: String,
    prefix: String,
    mut rx: UnboundedReceiver<Out>,
    start: Instant,
    results: Option<UnboundedSender<ResultRec>>,
    stats: Arc<Stats>,
    use_udp: bool,
) {
    let stream = match TcpStream::connect(&target).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("conn {id}: connect to {target} failed: {e}");
            stats.connect_failed.fetch_add(1, Ordering::Relaxed);
            while rx.recv().await.is_some() {}
            return;
        }
    };
    let _ = stream.set_nodelay(true);
    stats.connected.fetch_add(1, Ordering::Relaxed);
    let (rd, mut wr) = stream.into_split();
    let (hs_tx, mut hs_rx) = oneshot::channel::<String>();
    let reader = tokio::spawn(reader_task(rd, id, start, results, stats.clone(), use_udp.then_some(hs_tx)));
    let mut udp_out: Option<UdpOut> = None;
    let mut udp_decided = !use_udp;

    let mut encoder: Option<FrameEncoder> = None;
    let mut handshook = false;
    while let Some(msg) = rx.recv().await {
        match msg {
            Out::Handshake(line) => {
                let (hs, offered) = match rewrite_handshake(&line, &prefix) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("conn {id}: {e:#}");
                        break;
                    }
                };
                let mode = CompressMode::negotiate(&offered).unwrap_or(CompressMode::Plain);
                if mode.client_to_server_framed() {
                    encoder = Some(FrameEncoder::new());
                }
                if wr.write_all(format!("{hs}\n").as_bytes()).await.is_err() {
                    stats.send_errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                handshook = true;
            }
            Out::Line(l) => {
                if !handshook {
                    continue;
                }
                if !udp_decided {
                    udp_decided = true;
                    if let Ok(Ok(reply)) = tokio::time::timeout(Duration::from_secs(3), &mut hs_rx).await {
                        udp_out = open_udp(&reply, &target).await;
                    }
                }
                let l = match udp_out.as_mut() {
                    Some(u) => {
                        let mut recs = Vec::new();
                        let mut rest = String::new();
                        for one in l.split('\n') {
                            match packable(one) {
                                Some(r) => recs.push(r),
                                None => {
                                    if !rest.is_empty() {
                                        rest.push('\n');
                                    }
                                    rest.push_str(one);
                                }
                            }
                        }
                        if !recs.is_empty() {
                            if u.send(&recs).await.is_err() {
                                stats.send_errors.fetch_add(1, Ordering::Relaxed);
                            }
                            stats.lines_sent.fetch_add(recs.len() as u64, Ordering::Relaxed);
                        }
                        if rest.is_empty() {
                            continue;
                        }
                        rest
                    }
                    None => l,
                };
                // a batch holds the lines that shared one packet in the capture
                let nlines = 1 + l.bytes().filter(|&b| b == b'\n').count() as u64;
                let mut bytes = l.into_bytes();
                bytes.push(b'\n');
                let wire = match encoder.as_mut() {
                    Some(enc) => match enc.encode(&bytes) {
                        Ok(w) => w,
                        Err(e) => {
                            eprintln!("conn {id}: encode: {e}");
                            stats.send_errors.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    },
                    None => bytes,
                };
                if wr.write_all(&wire).await.is_err() {
                    stats.send_errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                stats.lines_sent.fetch_add(nlines, Ordering::Relaxed);
            }
            Out::Close => break,
        }
    }
    let _ = wr.shutdown().await;
    // let the server's final bytes drain before the socket is dropped
    let _ = tokio::time::timeout(Duration::from_millis(500), reader).await;
}

async fn results_writer(path: PathBuf, mut rx: UnboundedReceiver<ResultRec>) -> Result<()> {
    let mut f = tokio::io::BufWriter::new(File::create(&path).await?);
    while let Some(r) = rx.recv().await {
        let rec = serde_json::json!({ "c": r.c, "t": r.t, "l": r.l });
        f.write_all(rec.to_string().as_bytes()).await?;
        f.write_all(b"\n").await?;
    }
    f.flush().await?;
    Ok(())
}

pub async fn run(opts: Opts) -> Result<Report> {
    if !(opts.speed > 0.0) {
        bail!("--speed must be positive");
    }
    check_target(&opts.target, opts.allow_public)?;

    let file = File::open(&opts.input)
        .await
        .with_context(|| format!("opening {}", opts.input.display()))?;
    let mut lines = BufReader::new(file).lines();

    let stats = Arc::new(Stats::default());
    let (res_tx, res_task): (Option<UnboundedSender<ResultRec>>, Option<JoinHandle<Result<()>>>) =
        match &opts.results {
            Some(p) => {
                let (tx, rx) = unbounded_channel();
                (Some(tx), Some(tokio::spawn(results_writer(p.clone(), rx))))
            }
            None => (None, None),
        };

    let start = Instant::now();
    let start_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    let mut conns: HashMap<u64, UnboundedSender<Out>> = HashMap::new();
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    let mut max_lag = 0.0_f64;
    // lines of one connection with one timestamp went out as one packet
    let mut pending: Option<(u64, f64, String)> = None;

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let ev: Event = serde_json::from_str(&line).context("bad event line")?;
        if matches!(ev, Event::Meta { .. } | Event::S2c { .. } | Event::HsResp { .. }) {
            continue;
        }
        let t = ev.time();
        if opts.max_secs.is_some_and(|m| t > m) {
            break;
        }

        if let Event::C2s { c, l, .. } = &ev {
            match pending.as_mut() {
                Some((pc, pt, buf)) if *pc == *c && *pt == t => {
                    buf.push('\n');
                    buf.push_str(l);
                    continue;
                }
                _ => {}
            }
        }
        if let Some((pc, _, buf)) = pending.take() {
            if let Some(tx) = conns.get(&pc) {
                let _ = tx.send(Out::Line(buf));
            }
        }

        let due = start + Duration::from_secs_f64(t / opts.speed);
        let now = Instant::now();
        if due > now {
            tokio::time::sleep_until(due).await;
        } else {
            max_lag = max_lag.max((now - due).as_secs_f64());
        }

        match ev {
            // Connections are opened when their handshake arrives, so the
            // empty reconnect attempts in a capture are not replayed.
            Event::Open { .. } => {}
            Event::Hs { c, line, .. } => {
                let (tx, rx) = unbounded_channel();
                tasks.push(tokio::spawn(conn_task(
                    c,
                    opts.target.clone(),
                    opts.prefix.clone(),
                    rx,
                    start,
                    res_tx.clone(),
                    stats.clone(),
                    opts.udp,
                )));
                let _ = tx.send(Out::Handshake(line));
                conns.insert(c, tx);
            }
            Event::C2s { c, l, .. } => {
                pending = Some((c, t, l));
            }
            Event::Close { c, .. } => {
                if let Some(tx) = conns.remove(&c) {
                    let _ = tx.send(Out::Close);
                }
            }
            _ => {}
        }
    }

    if let Some((pc, _, buf)) = pending.take() {
        if let Some(tx) = conns.get(&pc) {
            let _ = tx.send(Out::Line(buf));
        }
    }
    // end of input: close whatever is still open, then wait for everything
    for (_, tx) in conns.drain() {
        let _ = tx.send(Out::Close);
    }
    for t in tasks {
        let _ = t.await;
    }
    drop(res_tx);
    if let Some(t) = res_task {
        t.await??;
    }

    Ok(Report {
        connected: stats.connected.load(Ordering::Relaxed),
        connect_failed: stats.connect_failed.load(Ordering::Relaxed),
        lines_sent: stats.lines_sent.load(Ordering::Relaxed),
        lines_received: stats.lines_received.load(Ordering::Relaxed),
        send_errors: stats.send_errors.load(Ordering::Relaxed),
        max_lag_secs: max_lag,
        start_epoch,
    })
}
