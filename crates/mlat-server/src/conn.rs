//! One client connection: handshake, framing, message dispatch.

use crate::interest::InterestMsg;
use crate::state::{log, now, ConnInfo, Line, Rng, Server};
use mlat_core::atomic::AtomicF64;
use mlat_core::pairing::round_dec;
use mlat_proto::{CompressMode, FrameEncoder, LineDecoder};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::Notify;
use tokio::time::{sleep_until, timeout, Instant};

struct Handshake {
    user: String,
    uuid: Option<String>,
    lat: f64,
    lon: f64,
    alt: f64,
    clock_type: String,
    mode: CompressMode,
    return_results: bool,
    ecef: bool,
    return_stats: Value,
    privacy: bool,
    version: i64,
    client_version: String,
    /// `udp_transport: 2` asks for the datagram transport
    udp_transport: i64,
}

/// Where this listener's UDP socket is, as told to clients.
#[derive(Clone)]
pub struct UdpAdvert {
    pub host: Option<String>,
    pub port: u16,
}

/// A random 32-bit key from the operating system (it authenticates datagrams).
fn random_key() -> u32 {
    use std::io::Read;
    let mut b = [0u8; 4];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)) {
        Ok(()) => u32::from_be_bytes(b),
        Err(_) => Rng::new().next_u64() as u32,
    }
}

fn fuzzy(rng: &mut Rng, t: f64) -> f64 {
    (0.9 * t + rng.f64() * 0.2 * t).round()
}

/// The sanitised user name: only `[A-Za-z0-9_.-]`, 3 to 40 characters.
fn sanitize_user(raw: &str, rng: &mut Rng) -> String {
    let mut user: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || "_.-".contains(c) { c } else { '_' }).collect();
    user.truncate(40);
    if user.len() < 3 {
        user = format!("{user}_{}", 10 + rng.below(89));
    }
    user
}

fn parse_handshake(line: &str, rng: &mut Rng) -> Result<Handshake, String> {
    let hs: Value = serde_json::from_str(line).map_err(|e| format!("Badly formatted handshake: {e}"))?;
    let get = |k: &str| hs.get(k).ok_or_else(|| format!("Missing field in handshake: '{k}'"));
    let version = get("version")?.as_i64().unwrap_or(0);
    if version != 2 && version != 3 {
        return Err("Bad values in handshake: Unsupported version in handshake".into());
    }
    let user_raw = match get("user")? {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let user = sanitize_user(&user_raw, rng);
    let uuid = hs.get("uuid").and_then(|v| v.as_str()).map(str::to_string);

    let offered: Vec<String> = get("compress")?.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    let mode = CompressMode::negotiate(&offered).ok_or("Bad values in handshake: No mutually usable compression type")?;

    let num = |k: &str| -> Result<f64, String> {
        let v = get(k)?;
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).ok_or_else(|| format!("Bad values in handshake: could not convert {k}"))
    };
    let lat = num("lat")?;
    if !(-90.0..=90.0).contains(&lat) {
        return Err("Bad values in handshake: invalid latitude, should be -90 .. 90".into());
    }
    let mut lon = num("lon")?;
    if !(-180.0..=360.0).contains(&lon) {
        return Err("Bad values in handshake: invalid longitude, should be -180 .. 360".into());
    }
    if lon > 180.0 {
        lon -= 180.0;
    }
    let alt = num("alt")?;
    if !(-1000.0..=10000.0).contains(&alt) {
        return Err("Bad values in handshake: invalid altitude, should be -1000 .. 10000".into());
    }

    let clock_type = hs.get("clock_type").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).unwrap_or_else(|| "dump1090".into());
    let truthy = |v: Option<&Value>| match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map_or(false, |x| x != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    };
    let return_results = truthy(hs.get("return_results"));
    let mut ecef = false;
    if return_results {
        match hs.get("return_result_format").and_then(|v| v.as_str()).unwrap_or("old") {
            "old" => {}
            "ecef" => ecef = true,
            _ => return Err("Bad values in handshake: invalid return_result_format, should be one of \"old\" or \"ecef\"".into()),
        }
    }
    Ok(Handshake {
        user,
        uuid,
        lat,
        lon,
        alt,
        clock_type,
        mode,
        return_results,
        ecef,
        return_stats: hs.get("return_stats").cloned().unwrap_or(Value::Bool(false)),
        privacy: truthy(hs.get("privacy")),
        version,
        udp_transport: hs.get("udp_transport").and_then(|v| v.as_i64()).unwrap_or(0),
        client_version: hs.get("client_version").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).unwrap_or_else(|| "unknown".into()),
    })
}

async fn deny(wr: &mut OwnedWriteHalf, reason: &str, rng: &mut Rng) {
    let msg = json!({ "deny": [reason], "reconnect_in": fuzzy(rng, 900.0) });
    let _ = wr.write_all(format!("{msg}\n").as_bytes()).await;
}

/// Sends queued lines to the client: zlib frames flushed once a second for
/// `zlib2`, plain lines otherwise.
async fn writer(mut wr: OwnedWriteHalf, mut rx: UnboundedReceiver<Line>, mode: CompressMode) {
    if mode != CompressMode::Zlib2 {
        while let Some(first) = rx.recv().await {
            let mut out = Vec::with_capacity(256);
            out.extend_from_slice(first.as_bytes());
            out.push(b'\n');
            while let Ok(more) = rx.try_recv() {
                out.extend_from_slice(more.as_bytes());
                out.push(b'\n');
            }
            if wr.write_all(&out).await.is_err() {
                return;
            }
        }
        return;
    }

    let mut enc = FrameEncoder::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut deadline: Option<Instant> = None;
    loop {
        tokio::select! {
            line = rx.recv() => match line {
                None => break,
                Some(l) => {
                    pending.extend_from_slice(l.as_bytes());
                    pending.push(b'\n');
                    deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(1));
                }
            },
            _ = async { sleep_until(deadline.unwrap()).await }, if deadline.is_some() => {
                deadline = None;
                // frames hold at most 32 KiB of text so the length fits in 16 bits
                let mut out = Vec::new();
                let mut start = 0;
                while start < pending.len() {
                    let mut end = (start + 32 * 1024).min(pending.len());
                    if end < pending.len() {
                        if let Some(p) = pending[start..end].iter().rposition(|&b| b == b'\n') {
                            end = start + p + 1;
                        }
                    }
                    match enc.encode(&pending[start..end]) {
                        Ok(f) => out.extend_from_slice(&f),
                        Err(_) => return,
                    }
                    start = end;
                }
                pending.clear();
                if wr.write_all(&out).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Hex text into a fixed-size array (None unless it is exactly 2N digits).
fn hex_array<const N: usize>(s: &str) -> Option<[u8; N]> {
    let b = s.as_bytes();
    if b.len() != 2 * N {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = nib(b[2 * i])? << 4 | nib(b[2 * i + 1])?;
    }
    Some(out)
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}

#[derive(serde::Deserialize)]
struct SyncBody<'a> {
    et: f64,
    ot: f64,
    em: &'a str,
    om: &'a str,
}

#[derive(serde::Deserialize)]
struct MlatBody<'a> {
    t: f64,
    m: &'a str,
}

/// The two message types that make up nearly all the traffic, parsed without
/// building a generic JSON tree.
#[derive(serde::Deserialize)]
enum Hot<'a> {
    #[serde(rename = "sync", borrow)]
    Sync(SyncBody<'a>),
    #[serde(rename = "mlat", borrow)]
    Mlat(MlatBody<'a>),
}

fn icao_list(v: &Value) -> Vec<u32> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(|s| u32::from_str_radix(s, 16).ok()).collect()).unwrap_or_default()
}

/// `process_message` in jsonclient.py.
fn process_line(server: &Server, c: &ConnInfo, line: &str, now: f64) {
    let e = &server.engine;
    let head = line.as_bytes();
    if head.len() > 8 && (head[..12.min(head.len())].windows(6).any(|w| w == b"\"sync\"" || w == b"\"mlat\"")) {
        if let Ok(hot) = serde_json::from_str::<Hot>(line) {
            c.msg_counter.fetch_add(1, Ordering::Relaxed);
            match hot {
                Hot::Sync(s) => {
                    if let (Some(em), Some(om)) = (hex_array::<14>(s.em), hex_array::<14>(s.om)) {
                        e.sync_from(&c.recv, now, s.et, s.ot, em, om);
                    }
                }
                Hot::Mlat(m) => {
                    if let Some(msg) = parse_hex(m.m) {
                        e.mlat_from(&c.recv, now, m.t, msg);
                    }
                }
            }
            return;
        }
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        log("client", &format!("[{}:{}] process_message json ValueError: {} >> {}", c.source_ip, c.source_port, c.user, line));
        return;
    };
    c.msg_counter.fetch_add(1, Ordering::Relaxed);
    let Some(obj) = v.as_object() else { return };
    if let Some(s) = obj.get("sync") {
        let (Some(em), Some(om)) = (s["em"].as_str().and_then(hex_array::<14>), s["om"].as_str().and_then(hex_array::<14>)) else { return };
        e.sync(now, c.uid, s["et"].as_f64().unwrap_or(0.0), s["ot"].as_f64().unwrap_or(0.0), em, om);
    } else if let Some(m) = obj.get("mlat") {
        if let Some(msg) = m["m"].as_str().and_then(parse_hex) {
            e.mlat(now, c.uid, m["t"].as_f64().unwrap_or(0.0), msg);
        }
    } else if let Some(s) = obj.get("seen") {
        let icaos = icao_list(s);
        e.seen(now, c.uid, &icaos);
        let _ = server.interest.send(InterestMsg::Seen { now, uid: c.uid, icaos });
    } else if let Some(s) = obj.get("lost") {
        let icaos = icao_list(s);
        e.lost(now, c.uid, &icaos);
        let _ = server.interest.send(InterestMsg::Lost { now, uid: c.uid, icaos });
    } else if obj.contains_key("input_connected")
        || obj.contains_key("input_disconnected")
        || obj.contains_key("clock_reset")
        || obj.contains_key("clock_jump")
    {
        e.clock_reset(c.uid);
    } else if obj.contains_key("heartbeat") {
    } else if let Some(r) = obj.get("rate_report") {
        let rates: Vec<(u32, f64)> = r
            .as_object()
            .map(|m| m.iter().filter_map(|(k, v)| Some((u32::from_str_radix(k, 16).ok()?, v.as_f64()?))).collect())
            .unwrap_or_default();
        e.rate_report(now, c.uid);
        let _ = server.interest.send(InterestMsg::Rate { now, uid: c.uid, rates });
    } else if obj.contains_key("quine") {
    } else {
        log("client", &format!("[{}:{}] Received an unexpected message: {}", c.source_ip, c.source_port, line));
    }
}

/// Gives the receiver a unique user name and a uid, and records its connection.
fn register(
    server: &Arc<Server>,
    hs: &Handshake,
    tx: tokio::sync::mpsc::UnboundedSender<Line>,
    use_udp: bool,
    source_ip: &str,
    source_port: &str,
    rng: &mut Rng,
) -> Result<(Arc<ConnInfo>, String), &'static str> {
    let mut users = server.users.lock().unwrap();
    let mut user = hs.user.clone();
    if let Some((old_uid, old_uuid)) = users.get(&user).cloned() {
        if hs.uuid.is_some() && hs.uuid == old_uuid {
            // the same receiver reconnecting: drop the old connection
            if let Some(old) = server.conns.read().unwrap().get(&old_uid) {
                old.close.notify_one();
            }
            users.remove(&user);
        } else {
            for _ in 0..1000 {
                if !users.contains_key(&user) {
                    break;
                }
                user = format!("{user}_{}", 100 + rng.below(900));
            }
        }
    }
    let Some(uid) = server.engine.connect(&user, hs.lat, hs.lon, hs.alt, &hs.clock_type) else {
        return Err("Bad values in handshake: unsupported clock type");
    };
    users.insert(user.clone(), (uid, hs.uuid.clone()));

    // the position shown publicly is blurred
    let (map_lat, map_lon) = if hs.privacy {
        (None, None)
    } else {
        let precision = 20.0;
        let off_x = -1.0 / precision + 1.0 / precision * rng.f64();
        let off_y = -1.0 / precision + 1.0 / precision * rng.f64();
        (
            Some(round_dec((hs.lat * precision).round_ties_even() / precision + off_x, 2)),
            Some(round_dec((hs.lon * precision).round_ties_even() / precision + off_y, 2)),
        )
    };
    let connection_info = format!("{user} v{} {} {} {} {}", hs.version, hs.clock_type, hs.client_version, if use_udp { "udp" } else { "tcp" }, hs.mode.name());
    let udp_key = if use_udp {
        let table = server.udp_clients.read().unwrap();
        let mut k = random_key();
        while table.contains_key(&k) {
            k = random_key();
        }
        Some(k)
    } else {
        None
    };
    let Some(recv) = server.engine.receiver_info(uid) else { return Err("receiver vanished") };
    let conn = Arc::new(ConnInfo {
        recv,
        uid,
        user: user.clone(),
        uuid: hs.uuid.clone(),
        privacy: hs.privacy,
        lat: hs.lat,
        lon: hs.lon,
        alt: hs.alt,
        connection_info,
        source_ip: source_ip.to_string(),
        source_port: source_port.to_string(),
        map_lat,
        map_lon,
        return_results: hs.return_results,
        result_format_ecef: hs.ecef,
        return_stats: hs.return_stats.as_bool().unwrap_or(false),
        tx,
        msg_counter: AtomicU64::new(0),
        last_msg: AtomicF64::new(now()),
        close: Notify::new(),
        udp_key,
    });
    server.conns.write().unwrap().insert(uid, conn.clone());
    if let Some(k) = udp_key {
        server.udp_clients.write().unwrap().insert(k, conn.clone());
    }
    Ok((conn, user))
}

pub async fn handle(server: Arc<Server>, stream: TcpStream, peer: SocketAddr, udp: Option<UdpAdvert>) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let mut rng = Rng::new();
    let mut source_ip = peer.ip().to_string();
    let mut source_port = peer.port().to_string();

    // ---- the handshake line, preceded by an optional PROXY line
    let mut head: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 65536];
    let mut proxied = false;
    let line = loop {
        if let Some(nl) = head.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = head.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
            if text.starts_with("PROXY ") && !proxied {
                proxied = true;
                let p: Vec<&str> = text.split(' ').collect();
                if p.len() > 4 {
                    source_ip = p[2].to_string();
                    source_port = p[4].to_string();
                }
                continue;
            }
            break text;
        }
        match timeout(Duration::from_secs(15), rd.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => head.extend_from_slice(&buf[..n]),
            _ => return,
        }
        if head.len() > (1 << 20) {
            return;
        }
    };
    server.log_handshake(&line);

    let hs = match parse_handshake(&line, &mut rng) {
        Ok(h) => h,
        Err(reason) => {
            log("client", &format!("[{source_ip}:{source_port}] Handshake failed: {reason}"));
            deny(&mut wr, &reason, &mut rng).await;
            return;
        }
    };

    // ---- register the receiver (user names are unique)
    let (tx, rx) = unbounded_channel::<Line>();
    let use_udp = udp.is_some() && hs.udp_transport == 2;
    let (conn, user) = match register(&server, &hs, tx, use_udp, &source_ip, &source_port, &mut rng) {
        Ok(v) => v,
        Err(reason) => {
            deny(&mut wr, reason, &mut rng).await;
            return;
        }
    };

    // ---- the handshake response, always uncompressed
    let motd = format!(
        "\n\n        {}\n\n        The multilateration server source code is available under\n        the terms of the Affero GPL (v3 or later). You may obtain\n        a copy of this server's source code at the following\n        location: {}\n        ",
        server.cfg.motd, server.cfg.source_url
    );
    let mut response = json!({
        "compress": hs.mode.name(),
        "reconnect_in": fuzzy(&mut rng, 10.0),
        "selective_traffic": true,
        "heartbeat": true,
        "return_results": hs.return_results,
        "return_stats": hs.return_stats,
        "rate_reports": true,
        "motd": motd,
    });
    if let (Some(adv), Some(key)) = (&udp, conn.udp_key) {
        response["udp_transport"] = json!([adv.host, adv.port, key]);
    }
    if wr.write_all(format!("{response}\n").as_bytes()).await.is_err() {
        cleanup(&server, &conn, &user);
        return;
    }
    log("client", &format!("[{}:{}] Handshake successful ({})", source_ip, source_port, conn.connection_info));

    tokio::spawn(writer(wr, rx, hs.mode));

    // ---- messages
    let mut dec = LineDecoder::new(hs.mode.client_to_server_framed());
    let mut lines: Vec<String> = Vec::new();
    let mut pending = std::mem::take(&mut head);
    'conn: loop {
        if !pending.is_empty() {
            lines.clear();
            if dec.push(&pending, &mut lines).is_err() {
                log("client", &format!("[{}:{}] Exception handling client: bad compressed stream", source_ip, source_port));
                break;
            }
            pending.clear();
            let t = now();
            conn.last_msg.store(t);
            for l in &lines {
                process_line(&server, &conn, l, t);
            }
        }
        tokio::select! {
            r = rd.read(&mut buf) => match r {
                Ok(n) if n > 0 => pending.extend_from_slice(&buf[..n]),
                _ => break 'conn,
            },
            _ = conn.close.notified() => break 'conn,
        }
    }

    cleanup(&server, &conn, &user);
}

fn cleanup(server: &Server, conn: &Arc<ConnInfo>, user: &str) {
    log("client", &format!("[{}:{}] Disconnected: ({})", conn.source_ip, conn.source_port, conn.connection_info));
    server.engine.disconnect(conn.uid);
    let _ = server.interest.send(InterestMsg::Disconnect { uid: conn.uid });
    server.conns.write().unwrap().remove(&conn.uid);
    if let Some(k) = conn.udp_key {
        server.udp_clients.write().unwrap().remove(&k);
    }
    let mut users = server.users.lock().unwrap();
    if users.get(user).map_or(false, |(uid, _)| *uid == conn.uid) {
        users.remove(user);
    }
}
