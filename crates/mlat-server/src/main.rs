//! mlat-server-rs: a multilateration server, wire-compatible with the Python
//! mlat-server (wiedehopf's fork) so receivers and readsb need no changes.

mod conn;
mod interest;
mod outputs;
mod results;
mod state;
mod statefiles;
mod udp;

use crossbeam_channel::unbounded;
use mlat_core::engine::Engine;
use conn::UdpAdvert;
use state::{log, now, Config, Line, SbsClient, Server};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::mpsc::unbounded_channel;

const HELP: &str = "usage: mlat-server-rs --work-dir DIR [--client-listen [host:]tcp_port[:udp_port]]...
                      [--filtered-basestation-listen [host:]port]... [--basestation-listen [host:]port]...
                      [--filtered-basestation-connect host:port]... [--basestation-connect host:port]...
                      [--write-csv FILE]... [--dump-pseudorange FILE] [--partition I/N]
                      [--motd TEXT] [--status-interval SECONDS] [--engine-threads N] [--io-threads N]
                      [--source-url URL] [--tag NAME] [--check-leaks]

Multilateration server, wire-compatible with the Python mlat-server.";

type Listen = (Option<String>, u16);

struct Args {
    work_dir: String,
    client_listen: Vec<(Option<String>, u16, Option<u16>)>,
    filtered: Vec<Listen>,
    unfiltered: Vec<Listen>,
    filtered_connect: Vec<(String, u16)>,
    unfiltered_connect: Vec<(String, u16)>,
    write_csv: Vec<String>,
    pseudorange: Option<String>,
    partition: (u32, u32),
    motd: String,
    status_interval: f64,
    engine_threads: usize,
    io_threads: usize,
    source_url: String,
}

fn port(s: &str, whole: &str) -> Result<u16, String> {
    s.parse::<u16>().map_err(|_| format!("bad port in '{whole}'"))
}

/// `--client-listen`: `tcp`, `host:tcp`, `tcp:udp` or `host:tcp:udp`.
fn client_listen(s: &str) -> Result<(Option<String>, u16, Option<u16>), String> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => Ok((None, port(parts[0], s)?, None)),
        2 => match (parts[0].parse::<u16>(), parts[1].parse::<u16>()) {
            (Ok(tcp), Ok(udp)) => Ok((None, tcp, Some(udp))),
            _ => Ok((Some(parts[0].to_string()), port(parts[1], s)?, None)),
        },
        3 => Ok((Some(parts[0].to_string()), port(parts[1], s)?, Some(port(parts[2], s)?))),
        _ => Err(format!("'{s}' should be in one of these formats: 'tcp_port', 'host:tcp_port', 'tcp_port:udp_port', 'host:tcp_port:udp_port'")),
    }
}

/// `--*basestation-listen`: `port` or `host:port`.
fn host_port(s: &str) -> Result<Listen, String> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => Ok((None, port(parts[0], s)?)),
        2 => Ok((Some(parts[0].to_string()), port(parts[1], s)?)),
        _ => Err(format!("'{s}' should be in 'port' or 'host:port' format")),
    }
}

/// `--*basestation-connect`: `host:port`.
fn connect_target(s: &str) -> Result<(String, u16), String> {
    match s.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !h.contains(':') => Ok((h.to_string(), port(p, s)?)),
        _ => Err(format!("'{s}' should be in 'host:port' format")),
    }
}

fn partition(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("'{s}' should be in the form I/N, where N is the total number of partitions and I is the partition for this server (1..N)");
    let (i, n) = s.split_once('/').ok_or_else(bad)?;
    let (i, n): (u32, u32) = (i.parse().map_err(|_| bad())?, n.parse().map_err(|_| bad())?);
    if i < 1 || i > n {
        return Err(bad());
    }
    Ok((i, n))
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        work_dir: String::new(),
        client_listen: Vec::new(),
        filtered: Vec::new(),
        unfiltered: Vec::new(),
        filtered_connect: Vec::new(),
        unfiltered_connect: Vec::new(),
        write_csv: Vec::new(),
        pseudorange: None,
        partition: (1, 1),
        motd: String::new(),
        status_interval: 15.0,
        engine_threads: 1,
        io_threads: 0,
        source_url: "https://github.com/myromeo/mlat-server".into(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let (key, inline) = match argv[i].split_once('=') {
            Some((k, v)) if argv[i].starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (argv[i].clone(), None),
        };
        let value = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            *i += 1;
            argv.get(*i).cloned().ok_or_else(|| format!("{key} needs a value"))
        };
        match key.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                std::process::exit(0);
            }
            "--work-dir" => a.work_dir = value(&mut i)?,
            "--client-listen" => a.client_listen.push(client_listen(&value(&mut i)?)?),
            "--filtered-basestation-listen" => a.filtered.push(host_port(&value(&mut i)?)?),
            "--basestation-listen" => a.unfiltered.push(host_port(&value(&mut i)?)?),
            "--filtered-basestation-connect" => a.filtered_connect.push(connect_target(&value(&mut i)?)?),
            "--basestation-connect" => a.unfiltered_connect.push(connect_target(&value(&mut i)?)?),
            "--write-csv" => a.write_csv.push(value(&mut i)?),
            "--dump-pseudorange" => a.pseudorange = Some(value(&mut i)?),
            "--check-leaks" => {} // Python memory-leak diagnostics; nothing to check here
            "--partition" => a.partition = partition(&value(&mut i)?)?,
            "--motd" => a.motd = value(&mut i)?,
            "--status-interval" => a.status_interval = value(&mut i)?.parse().map_err(|_| "bad --status-interval")?,
            "--engine-threads" => a.engine_threads = value(&mut i)?.parse().map_err(|_| "bad --engine-threads")?,
            "--io-threads" => a.io_threads = value(&mut i)?.parse().map_err(|_| "bad --io-threads")?,
            "--source-url" => a.source_url = value(&mut i)?,
            "--tag" => {
                value(&mut i)?;
            }
            other => return Err(format!("unrecognised option {other}")),
        }
        i += 1;
    }
    if a.work_dir.is_empty() {
        return Err("--work-dir is required".into());
    }
    if a.client_listen.is_empty() {
        return Err("at least one --client-listen is required".into());
    }
    Ok(a)
}

fn bind_addrs(host: &Option<String>, port: u16) -> Vec<SocketAddr> {
    match host {
        Some(h) => format!("{h}:{port}").parse().into_iter().collect(),
        None => vec![SocketAddr::from(([0, 0, 0, 0], port)), "[::]:0".parse::<SocketAddr>().map(|mut s| {
            s.set_port(port);
            s
        }).unwrap()],
    }
}

async fn serve_clients(server: Arc<Server>, host: Option<String>, tcp_port: u16, udp_port: Option<u16>, udp_tasks: usize) {
    // the UDP socket first: its address goes into the handshake response
    let advert = match udp_port {
        Some(up) => {
            // as in Python, an unspecified host binds IPv4 only
            let bind = format!("{}:{up}", host.as_deref().unwrap_or("0.0.0.0"));
            match UdpSocket::bind(&bind).await {
                Ok(sock) => {
                    let name = sock.local_addr().map(|a| a.to_string()).unwrap_or(bind);
                    log("client", &format!("JSON client handler listening on {name} (UDP)"));
                    let sock = Arc::new(sock);
                    for _ in 0..udp_tasks.max(1) {
                        tokio::spawn(udp::serve(server.clone(), sock.clone()));
                    }
                    Some(UdpAdvert { host: host.clone(), port: up })
                }
                Err(e) => {
                    log("client", &format!("cannot listen on {bind} (UDP): {e}"));
                    None
                }
            }
        }
        None => None,
    };

    for addr in bind_addrs(&host, tcp_port) {
        let Ok(socket) = (if addr.is_ipv6() { tokio::net::TcpSocket::new_v6() } else { tokio::net::TcpSocket::new_v4() }) else { continue };
        let _ = socket.set_reuseaddr(true);
        if addr.is_ipv6() {
            // IPv4 is bound separately; keep the two sockets independent
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let on: libc::c_int = 1;
                unsafe { libc::setsockopt(socket.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, &on as *const _ as *const libc::c_void, 4) };
            }
        }
        let listener = match socket.bind(addr).and_then(|_| socket.listen(1024)) {
            Ok(l) => l,
            Err(e) => {
                if !addr.is_ipv6() {
                    log("client", &format!("cannot listen on {addr}: {e}"));
                }
                continue;
            }
        };
        log("client", &format!("JSON client handler listening on {addr} (TCP)"));
        let server = server.clone();
        let advert = advert.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        tokio::spawn(conn::handle(server.clone(), stream, peer, advert.clone()));
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        });
    }
}

/// Sends results to one Basestation peer until it goes away (`BasestationClient`).
async fn sbs_session(server: Arc<Server>, stream: TcpStream, peer: SocketAddr, filtered: bool) {
    let (tx, mut rx) = unbounded_channel::<Line>();
    server.sbs_clients.lock().unwrap().push(SbsClient { tx, filtered });
    log("basestation", &format!("[{peer}] Connection established"));
    let (mut rd, mut wr) = stream.into_split();
    // lines as results arrive; a bare newline if nothing was sent for 30 s
    let writer = async move {
        loop {
            match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
                Ok(Some(l)) => {
                    if wr.write_all(l.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    if wr.write_all(b"\n").await.is_err() {
                        break;
                    }
                }
            }
        }
    };
    // detect the peer going away
    let reader = async move {
        let mut b = [0u8; 1024];
        while matches!(rd.read(&mut b).await, Ok(n) if n > 0) {}
    };
    tokio::select! { _ = writer => {}, _ = reader => {} }
}

async fn serve_sbs(server: Arc<Server>, host: Option<String>, port: u16, filtered: bool) {
    for addr in bind_addrs(&host, port) {
        let Ok(listener) = TcpListener::bind(addr).await else { continue };
        log("basestation", &format!("Basestation output listener listening on {addr} (TCP)"));
        let server = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else { continue };
                tokio::spawn(sbs_session(server.clone(), stream, peer, filtered));
            }
        });
    }
}

/// `--basestation-connect`: connect out and send results, reconnecting every 30 s.
async fn connect_sbs(server: Arc<Server>, host: String, port: u16, filtered: bool) {
    loop {
        if let Ok(stream) = TcpStream::connect((host.as_str(), port)).await {
            if let Ok(peer) = stream.peer_addr() {
                sbs_session(server.clone(), stream, peer, filtered).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// Drives everything time-based: group-collection deadlines, pairing cleanup,
/// the 15 s state evaluation and the heartbeats.
fn clock_thread(server: Arc<Server>) {
    let start = now();
    let mut next_cleanup = start + 1.0;
    let mut next_15 = start;
    let mut next_heartbeat = start + 30.0;
    let mut status = statefiles::StatusState { next_status: start + server.cfg.status_interval, last_cpu: 0.0 };
    loop {
        // sleep until something is due, but look again soon: new groups can bring the deadline forward
        let t0 = now();
        let next = [next_cleanup, next_15, next_heartbeat].into_iter().fold(f64::MAX, f64::min);
        let due = server.engine.peek_due().unwrap_or(f64::MAX).min(next);
        std::thread::sleep(Duration::from_secs_f64((due - t0).clamp(0.005, 0.025)));
        let t = now();
        while server.engine.next_due_before(t).is_some() {
            server.engine.process_next_cohort();
        }
        if t >= next_cleanup {
            server.engine.cleanup(t);
            next_cleanup += 10.0;
        }
        if t >= next_15 {
            let health = server.engine.health(t);
            let (tx, rx) = unbounded();
            let _ = server.interest.send(interest::InterestMsg::Snapshot { reply: tx });
            let snapshot = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
            statefiles::write_state(&server, &health, &snapshot, t, &mut status);
            server.engine.clear_sync_points();
            next_15 += 15.0;
        }
        if t >= next_heartbeat {
            next_heartbeat += 30.0;
            let beat: Line = Line::from(format!("{{\"heartbeat\":{{\"server_time\":{:.3}}}}}", t));
            for c in server.conns.read().unwrap().values() {
                if t - c.last_msg.load() > 150.0 {
                    log("client", &format!("[{}:{}] No recent messages seen, closing connection", c.source_ip, c.source_port));
                    c.close.notify_one();
                } else {
                    let _ = c.tx.send(beat.clone());
                }
            }
        }
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("mlat-server-rs: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    if let Err(e) = std::fs::create_dir_all(&args.work_dir) {
        eprintln!("cannot create work dir {}: {e}", args.work_dir);
        std::process::exit(1);
    }

    let status_interval = if args.status_interval < 0.0 { 1e15 } else { args.status_interval * 0.95 };
    let engine = Engine::with_partition(args.engine_threads, args.engine_threads, false, now(), args.partition);
    let (itx, irx) = unbounded();
    let server = Arc::new(Server::new(
        engine,
        Config { work_dir: args.work_dir.clone().into(), motd: args.motd.clone(), source_url: args.source_url.clone(), status_interval, partition: args.partition },
        itx,
    ));

    let mut outs = outputs::Outputs::default();
    for f in &args.write_csv {
        match outputs::CsvWriter::new(f) {
            Ok(w) => outs.csv.push(w),
            Err(e) => {
                eprintln!("mlat-server-rs: cannot open {f}: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Some(f) = &args.pseudorange {
        match outputs::Pseudorange::new(f) {
            Ok(p) => outs.pseudo = Some(Arc::new(p)),
            Err(e) => {
                eprintln!("mlat-server-rs: cannot open {f}: {e}");
                std::process::exit(1);
            }
        }
    }
    let pseudo = outs.pseudo.clone();

    {
        let s = server.clone();
        std::thread::Builder::new().name("interest".into()).spawn(move || interest::run(s, irx)).unwrap();
        let s = server.clone();
        std::thread::Builder::new().name("results".into()).spawn(move || results::run(s, outs)).unwrap();
        let s = server.clone();
        std::thread::Builder::new().name("clock".into()).spawn(move || clock_thread(s)).unwrap();
    }

    // connection handling spreads over this many threads (default: up to 4)
    let io_threads = if args.io_threads > 0 { args.io_threads } else { std::thread::available_parallelism().map_or(2, |n| n.get()).min(4) };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(io_threads).enable_all().thread_name("io").build().unwrap();
    rt.block_on(async {
        for (h, p, u) in &args.client_listen {
            serve_clients(server.clone(), h.clone(), *p, *u, io_threads).await;
        }
        for (h, p) in &args.unfiltered_connect {
            tokio::spawn(connect_sbs(server.clone(), h.clone(), *p, false));
        }
        for (h, p) in &args.filtered_connect {
            tokio::spawn(connect_sbs(server.clone(), h.clone(), *p, true));
        }
        if let Some(p) = pseudo {
            // SIGHUP reopens the pseudorange file, so it can be rotated
            let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).expect("SIGHUP handler");
            tokio::spawn(async move {
                while hup.recv().await.is_some() {
                    if let Err(e) = p.reopen() {
                        log("coordinator", &format!("cannot reopen pseudorange file: {e}"));
                    }
                }
            });
        }
        for (h, p) in &args.filtered {
            serve_sbs(server.clone(), h.clone(), *p, true).await;
        }
        for (h, p) in &args.unfiltered {
            serve_sbs(server.clone(), h.clone(), *p, false).await;
        }
        log("coordinator", "mlat-server-rs started");
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log("coordinator", "Halting on SIGINT"),
            _ = term.recv() => log("coordinator", "Halting on SIGTERM"),
        }
    });
}
