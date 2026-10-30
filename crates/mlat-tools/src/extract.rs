//! Turn a tcpdump capture of the mlat client port into a replayable event file.
//!
//! Each TCP connection is reassembled per direction, the handshake is parsed
//! to learn the compression mode, and the zlib framing is undone so the file
//! holds plain message lines. Connections whose SYN is missing cannot be
//! decoded (the zlib stream is continuous) and are only counted.

use crate::events::{message_kind, Event};
use crate::pcap::{parse_tcp, PcapReader, FLAG_ACK, FLAG_FIN, FLAG_RST, FLAG_SYN};
use anyhow::{Context, Result};
use mlat_proto::{CompressMode, LineDecoder};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::net::IpAddr;
use std::path::Path;

#[derive(Default, Debug)]
pub struct Summary {
    pub packets: u64,
    pub tcp_on_port: u64,
    pub connections: u64,
    pub without_syn: u64,
    pub handshakes: u64,
    pub damaged: u64,
    pub denied: u64,
    pub modes: BTreeMap<String, u64>,
    pub c2s_kinds: BTreeMap<String, u64>,
    pub s2c_kinds: BTreeMap<String, u64>,
    pub duration: f64,
}

/// One direction of a TCP stream: delivers bytes in order, tolerating
/// retransmission, overlap and reordering.
#[derive(Default)]
struct Dir {
    base: Option<u32>,
    next: u64,
    pending: BTreeMap<u64, Vec<u8>>,
}

impl Dir {
    fn syn(&mut self, seq: u32) {
        if self.base.is_none() {
            self.base = Some(seq.wrapping_add(1));
        }
    }

    fn data(&mut self, seq: u32, payload: &[u8]) -> Vec<u8> {
        let Some(base) = self.base else { return Vec::new() };
        let off = seq.wrapping_sub(base);
        if off > 0x8000_0000 {
            return Vec::new(); // before the start of the stream
        }
        let (mut off, mut data) = (off as u64, payload);
        let len = data.len() as u64;
        if off + len <= self.next {
            return Vec::new(); // pure duplicate
        }
        if off < self.next {
            data = &data[(self.next - off) as usize..];
            off = self.next;
        }
        if off > self.next {
            self.pending.entry(off).or_insert_with(|| data.to_vec());
            return Vec::new();
        }
        let mut out = data.to_vec();
        self.next += data.len() as u64;
        while let Some((&poff, _)) = self.pending.iter().next() {
            if poff > self.next {
                break;
            }
            let seg = self.pending.remove(&poff).unwrap();
            let end = poff + seg.len() as u64;
            if end > self.next {
                out.extend_from_slice(&seg[(self.next - poff) as usize..]);
                self.next = end;
            }
        }
        out
    }

    fn has_gap(&self) -> bool {
        !self.pending.is_empty()
    }
}

struct Conn {
    id: u64,
    c2s: Dir,
    s2c: Dir,
    hs_buf: Vec<u8>,
    resp_buf: Vec<u8>,
    proxy: Option<String>,
    c2s_dec: Option<LineDecoder>,
    s2c_dec: Option<LineDecoder>,
    broken: bool,
    closed: bool,
}

struct Extractor<W: Write> {
    out: W,
    t0: f64,
    port: u16,
    conns: HashMap<(IpAddr, u16), Conn>,
    orphans: HashSet<(IpAddr, u16)>,
    next_id: u64,
    summary: Summary,
}

impl<W: Write> Extractor<W> {
    fn emit(&mut self, ev: &Event) -> Result<()> {
        serde_json::to_writer(&mut self.out, ev)?;
        self.out.write_all(b"\n")?;
        Ok(())
    }

    fn rel(&self, ts: f64) -> f64 {
        // microsecond resolution keeps the file compact and stable
        ((ts - self.t0) * 1e6).round() / 1e6
    }

    fn segment(&mut self, ts: f64, seg: &crate::pcap::TcpSegment) -> Result<()> {
        let to_server = seg.dport == self.port;
        let from_server = seg.sport == self.port;
        if !to_server && !from_server {
            return Ok(());
        }
        self.summary.tcp_on_port += 1;
        let key = if to_server { (seg.src, seg.sport) } else { (seg.dst, seg.dport) };
        let t = self.rel(ts);

        if to_server && seg.flags & FLAG_SYN != 0 && seg.flags & FLAG_ACK == 0 {
            if let Some(mut old) = self.conns.remove(&key) {
                self.finish(&mut old, t)?;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.summary.connections += 1;
            let mut conn = Conn {
                id,
                c2s: Dir::default(),
                s2c: Dir::default(),
                hs_buf: Vec::new(),
                resp_buf: Vec::new(),
                proxy: None,
                c2s_dec: None,
                s2c_dec: None,
                broken: false,
                closed: false,
            };
            conn.c2s.syn(seg.seq);
            self.orphans.remove(&key);
            self.emit(&Event::Open { c: id, t, client: format!("{}:{}", key.0, key.1) })?;
            self.conns.insert(key, conn);
            return Ok(());
        }

        let Some(mut conn) = self.conns.remove(&key) else {
            self.orphans.insert(key);
            return Ok(());
        };

        if from_server && seg.flags & FLAG_SYN != 0 {
            conn.s2c.syn(seg.seq);
        }

        if !conn.closed && !seg.payload.is_empty() {
            if to_server {
                let bytes = conn.c2s.data(seg.seq, seg.payload);
                if !bytes.is_empty() {
                    self.feed_c2s(&mut conn, t, &bytes)?;
                }
            } else {
                let bytes = conn.s2c.data(seg.seq, seg.payload);
                if !bytes.is_empty() {
                    self.feed_s2c(&mut conn, t, &bytes)?;
                }
            }
        }

        if seg.flags & (FLAG_FIN | FLAG_RST) != 0 {
            self.finish(&mut conn, t)?;
        }
        // closed connections stay in the map so trailing ACKs are not
        // mistaken for streams whose SYN was missed
        self.conns.insert(key, conn);
        Ok(())
    }

    fn finish(&mut self, conn: &mut Conn, t: f64) -> Result<()> {
        if conn.closed {
            return Ok(());
        }
        conn.closed = true;
        let damaged = conn.broken || conn.c2s.has_gap() || conn.s2c.has_gap();
        if damaged {
            self.summary.damaged += 1;
        }
        self.emit(&Event::Close { c: conn.id, t, damaged })
    }

    fn feed_c2s(&mut self, conn: &mut Conn, t: f64, bytes: &[u8]) -> Result<()> {
        if conn.broken {
            return Ok(());
        }
        if conn.c2s_dec.is_none() {
            conn.hs_buf.extend_from_slice(bytes);
            loop {
                let Some(nl) = conn.hs_buf.iter().position(|&b| b == b'\n') else {
                    return Ok(());
                };
                let raw: Vec<u8> = conn.hs_buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
                if line.starts_with("PROXY ") && conn.proxy.is_none() {
                    conn.proxy = Some(line);
                    continue;
                }
                let offered: Vec<String> = serde_json::from_str::<serde_json::Value>(&line)
                    .ok()
                    .and_then(|v| v.get("compress").cloned())
                    .and_then(|c| serde_json::from_value(c).ok())
                    .unwrap_or_default();
                let Some(mode) = CompressMode::negotiate(&offered) else {
                    conn.broken = true;
                    return Ok(());
                };
                self.summary.handshakes += 1;
                *self.summary.modes.entry(mode.name().to_string()).or_default() += 1;
                self.emit(&Event::Hs { c: conn.id, t, line, proxy: conn.proxy.take() })?;
                conn.c2s_dec = Some(LineDecoder::new(mode.client_to_server_framed()));
                let rest = std::mem::take(&mut conn.hs_buf);
                if rest.is_empty() {
                    return Ok(());
                }
                return self.decode_c2s(conn, t, &rest);
            }
        }
        self.decode_c2s(conn, t, bytes)
    }

    fn decode_c2s(&mut self, conn: &mut Conn, t: f64, bytes: &[u8]) -> Result<()> {
        let mut lines = Vec::new();
        if conn.c2s_dec.as_mut().unwrap().push(bytes, &mut lines).is_err() {
            conn.broken = true;
        }
        for l in lines {
            *self.summary.c2s_kinds.entry(message_kind(&l).to_string()).or_default() += 1;
            self.emit(&Event::C2s { c: conn.id, t, l })?;
        }
        Ok(())
    }

    fn feed_s2c(&mut self, conn: &mut Conn, t: f64, bytes: &[u8]) -> Result<()> {
        if conn.broken {
            return Ok(());
        }
        if conn.s2c_dec.is_none() {
            conn.resp_buf.extend_from_slice(bytes);
            let Some(nl) = conn.resp_buf.iter().position(|&b| b == b'\n') else {
                return Ok(());
            };
            let raw: Vec<u8> = conn.resp_buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
            let resp: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            if resp.get("deny").is_some() {
                self.summary.denied += 1;
            }
            let framed = resp.get("compress").and_then(|c| c.as_str()) == Some("zlib2");
            self.emit(&Event::HsResp { c: conn.id, t, line })?;
            conn.s2c_dec = Some(LineDecoder::new(framed));
            let rest = std::mem::take(&mut conn.resp_buf);
            if rest.is_empty() {
                return Ok(());
            }
            return self.decode_s2c(conn, t, &rest);
        }
        self.decode_s2c(conn, t, bytes)
    }

    fn decode_s2c(&mut self, conn: &mut Conn, t: f64, bytes: &[u8]) -> Result<()> {
        let mut lines = Vec::new();
        if conn.s2c_dec.as_mut().unwrap().push(bytes, &mut lines).is_err() {
            conn.broken = true;
        }
        for l in lines {
            *self.summary.s2c_kinds.entry(message_kind(&l).to_string()).or_default() += 1;
            self.emit(&Event::S2c { c: conn.id, t, l })?;
        }
        Ok(())
    }
}

/// Extract `pcap_path` (classic pcap) into the event file at `out_path`.
pub fn run(pcap_path: &Path, port: u16, out_path: &Path) -> Result<Summary> {
    let file = File::open(pcap_path).with_context(|| format!("opening {}", pcap_path.display()))?;
    let mut reader = PcapReader::new(BufReader::new(file))?;
    let link = reader.link_type();
    let out = BufWriter::new(File::create(out_path).with_context(|| format!("creating {}", out_path.display()))?);

    let mut ex = Extractor {
        out,
        t0: 0.0,
        port,
        conns: HashMap::new(),
        orphans: HashSet::new(),
        next_id: 1,
        summary: Summary::default(),
    };

    let mut first = true;
    let mut last_t = 0.0;
    while let Some(pkt) = reader.next_packet()? {
        if first {
            ex.t0 = pkt.ts;
            ex.emit(&Event::Meta { start_epoch: pkt.ts, port })?;
            first = false;
        }
        ex.summary.packets += 1;
        last_t = ex.rel(pkt.ts);
        if let Some(seg) = parse_tcp(link, &pkt.data) {
            ex.segment(pkt.ts, &seg)?;
        }
    }

    let mut remaining: Vec<Conn> = ex.conns.drain().map(|(_, c)| c).collect();
    remaining.sort_by_key(|c| c.id);
    for mut c in remaining {
        ex.finish(&mut c, last_t)?;
    }
    ex.summary.without_syn = ex.orphans.len() as u64;
    ex.summary.duration = last_t;
    ex.out.flush()?;
    Ok(ex.summary)
}
