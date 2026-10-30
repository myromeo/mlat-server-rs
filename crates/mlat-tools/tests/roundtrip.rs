//! pcap -> extract -> replay -> mock server, end to end.

use mlat_proto::{CompressMode, FrameEncoder, LineDecoder};
use mlat_tools::events::Event;
use mlat_tools::{extract, replay};
use std::collections::BTreeMap;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SYN: u8 = 0x02;
const ACK: u8 = 0x10;
const PSH: u8 = 0x08;
const FIN: u8 = 0x01;

fn tcp_packet(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, seq: u32, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0u8; 12];
    p.extend_from_slice(&[0x08, 0x00]); // ethernet header (macs zero), IPv4
    let total = 20 + 20 + payload.len();
    p.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0x40, 0, 64, 6, 0, 0]);
    p.extend_from_slice(&src);
    p.extend_from_slice(&dst);
    p.extend_from_slice(&sport.to_be_bytes());
    p.extend_from_slice(&dport.to_be_bytes());
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 0, 0x50, flags, 0x01, 0xf4, 0, 0, 0, 0]);
    p.extend_from_slice(payload);
    p
}

fn pcap_file(records: &[(f64, Vec<u8>)]) -> Vec<u8> {
    let mut f = vec![0xd4, 0xc3, 0xb2, 0xa1, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 1, 0, 0, 0];
    for (ts, data) in records {
        f.extend_from_slice(&(ts.floor() as u32).to_le_bytes());
        f.extend_from_slice(&(((ts - ts.floor()) * 1e6).round() as u32).to_le_bytes());
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(&(data.len() as u32).to_le_bytes());
        f.extend_from_slice(data);
    }
    f
}

fn msg(kind: &str, i: usize) -> String {
    format!("{{\"{kind}\": {{\"t\": {}, \"m\": \"{:028x}\"}}}}", i * 1013, i * 7907)
}

fn frame(enc: &mut FrameEncoder, lines: &[String]) -> Vec<u8> {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    enc.encode(text.as_bytes()).unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mlat-tools-test-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

const SERVER: [u8; 4] = [10, 0, 0, 1];
const PORT: u16 = 31090;

/// Returns (pcap bytes, alice's expected client lines, bob's expected client lines).
fn build_capture() -> (Vec<u8>, Vec<String>, Vec<String>) {
    let mut rec: Vec<(f64, Vec<u8>)> = Vec::new();
    let t0 = 1_700_000_000.0;
    let mut t = t0;
    let mut tick = || {
        t += 0.01;
        t
    };

    // --- alice: zlib2 client, out-of-order / duplicate / split segments ---
    let a = [10, 0, 0, 2];
    let hs_a = "{\"version\": 3, \"user\": \"alice\", \"uuid\": \"u-alice\", \"lat\": 51.5, \"lon\": -0.1, \"alt\": 10, \"compress\": [\"zlib2\", \"zlib\", \"none\"], \"return_results\": true}\n";
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, 1000, SYN, b"")));
    rec.push((tick(), tcp_packet(SERVER, a, PORT, 40000, 5000, SYN | ACK, b"")));
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, 1001, ACK, b"")));
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, 1001, PSH | ACK, hs_a.as_bytes())));
    let mut seq_a = 1001 + hs_a.len() as u32;

    let mut enc = FrameEncoder::new();
    let mut alice_lines = Vec::new();
    let mut frames = Vec::new();
    for f in 0..3 {
        let lines: Vec<String> = (0..4).map(|i| msg(if i % 2 == 0 { "sync" } else { "mlat" }, f * 10 + i)).collect();
        alice_lines.extend(lines.clone());
        frames.push(frame(&mut enc, &lines));
    }
    let offs: Vec<u32> = frames
        .iter()
        .scan(seq_a, |s, f| {
            let o = *s;
            *s += f.len() as u32;
            Some(o)
        })
        .collect();
    // frame 2 first, then frame 1 split in two with a duplicate of its first half, then frame 3
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, offs[1], PSH | ACK, &frames[1])));
    let half = frames[0].len() / 2;
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, offs[0], PSH | ACK, &frames[0][..half])));
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, offs[0], PSH | ACK, &frames[0][..half])));
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, offs[0] + half as u32, PSH | ACK, &frames[0][half..])));
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, offs[2], PSH | ACK, &frames[2])));
    seq_a = offs[2] + frames[2].len() as u32;

    // server side: response line then a zlib2 frame
    let resp = "{\"compress\": \"zlib2\", \"motd\": \"hi\"}\n";
    let mut senc = FrameEncoder::new();
    let hb = frame(&mut senc, &["{\"heartbeat\": {\"server_time\": 1.5}}".to_string()]);
    let mut srv = resp.as_bytes().to_vec();
    srv.extend_from_slice(&hb);
    rec.push((tick(), tcp_packet(SERVER, a, PORT, 40000, 5001, PSH | ACK, &srv)));
    rec.push((tick(), tcp_packet(a, SERVER, 40000, PORT, seq_a, FIN | ACK, b"")));

    // --- bob: plain ("none") client ---
    let b = [10, 0, 0, 3];
    let hs_b = "PROXY TCP4 203.0.113.9 10.0.0.1 51000 31090\n{\"version\": 3, \"user\": \"bob\", \"lat\": 1.0, \"lon\": 2.0, \"alt\": 3, \"compress\": [\"none\"]}\n";
    rec.push((tick(), tcp_packet(b, SERVER, 41000, PORT, 70, SYN, b"")));
    rec.push((tick(), tcp_packet(SERVER, b, PORT, 41000, 900, SYN | ACK, b"")));
    rec.push((tick(), tcp_packet(b, SERVER, 41000, PORT, 71, PSH | ACK, hs_b.as_bytes())));
    let bob_lines: Vec<String> = (0..5).map(|i| msg("mlat", 100 + i)).collect();
    let body: String = bob_lines.iter().map(|l| format!("{l}\n")).collect();
    rec.push((tick(), tcp_packet(b, SERVER, 41000, PORT, 71 + hs_b.len() as u32, PSH | ACK, body.as_bytes())));
    rec.push((tick(), tcp_packet(SERVER, b, PORT, 41000, 901, PSH | ACK, b"{\"compress\": \"none\"}\n")));

    // --- carol: connection that began before the capture (no SYN) ---
    let c = [10, 0, 0, 4];
    rec.push((tick(), tcp_packet(c, SERVER, 42000, PORT, 777777, PSH | ACK, b"garbage-from-midstream")));

    (pcap_file(&rec), alice_lines, bob_lines)
}

fn read_events(path: &PathBuf) -> Vec<Event> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn extract_reassembles_and_decodes() {
    let (pcap, alice, bob) = build_capture();
    let (pcap_path, out_path) = (tmp("a.pcap"), tmp("a.jsonl"));
    std::fs::write(&pcap_path, pcap).unwrap();
    let s = extract::run(&pcap_path, PORT, &out_path).unwrap();

    assert_eq!(s.connections, 2);
    assert_eq!(s.handshakes, 2);
    assert_eq!(s.without_syn, 1);
    assert_eq!(s.damaged, 0);
    assert_eq!(s.modes.get("zlib2"), Some(&1));
    assert_eq!(s.modes.get("none"), Some(&1));

    let ev = read_events(&out_path);
    let c2s = |id: u64| -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                Event::C2s { c, l, .. } if *c == id => Some(l.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(c2s(1), alice);
    assert_eq!(c2s(2), bob);

    // server -> client: response line plus the zlib2 heartbeat
    assert!(ev.iter().any(|e| matches!(e, Event::HsResp { c: 1, line, .. } if line.contains("\"motd\""))));
    assert!(ev.iter().any(|e| matches!(e, Event::S2c { c: 1, l, .. } if l.contains("heartbeat"))));
    // PROXY line captured separately from the handshake
    assert!(ev.iter().any(|e| matches!(e, Event::Hs { c: 2, proxy: Some(p), .. } if p.starts_with("PROXY "))));
    // alice closed with FIN, bob still open at end of capture
    assert!(ev.iter().filter(|e| matches!(e, Event::Close { .. })).count() == 2);
    assert_eq!(s.c2s_kinds.get("sync"), Some(&6));
}

#[tokio::test]
async fn replay_delivers_identical_lines_to_mock_server() {
    let (pcap, alice, bob) = build_capture();
    let (pcap_path, out_path, res_path) = (tmp("b.pcap"), tmp("b.jsonl"), tmp("b-results.jsonl"));
    std::fs::write(&pcap_path, pcap).unwrap();
    extract::run(&pcap_path, PORT, &out_path).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Vec<String>)>();

    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let done = done_tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let mut head = Vec::new();
                let mut dec: Option<LineDecoder> = None;
                let mut user = String::new();
                let mut lines = Vec::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    if let Some(d) = dec.as_mut() {
                        d.push(&buf[..n], &mut lines).unwrap();
                        continue;
                    }
                    head.extend_from_slice(&buf[..n]);
                    let Some(nl) = head.iter().position(|&b| b == b'\n') else { continue };
                    let rest = head.split_off(nl + 1);
                    let hs: serde_json::Value = serde_json::from_slice(&head[..nl]).unwrap();
                    user = hs["user"].as_str().unwrap().to_string();
                    let offered: Vec<String> = serde_json::from_value(hs["compress"].clone()).unwrap();
                    let mode = CompressMode::negotiate(&offered).unwrap();
                    let reply = format!("{{\"compress\": \"{}\"}}\n", mode.name());
                    sock.write_all(reply.as_bytes()).await.unwrap();
                    let mut d = LineDecoder::new(mode.client_to_server_framed());
                    d.push(&rest, &mut lines).unwrap();
                    dec = Some(d);
                }
                let _ = done.send((user, lines));
            });
        }
    });

    let report = replay::run(replay::Opts {
        input: out_path,
        target: addr.to_string(),
        speed: 1000.0,
        prefix: "rp-".into(),
        results: Some(res_path.clone()),
        max_secs: None,
        allow_public: false,
        udp: false,
    })
    .await
    .unwrap();
    assert_eq!(report.connected, 2);
    assert_eq!(report.connect_failed, 0);
    assert_eq!(report.lines_sent as usize, alice.len() + bob.len());

    let mut got: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for _ in 0..2 {
        let (u, l) = tokio::time::timeout(std::time::Duration::from_secs(5), done_rx.recv())
            .await
            .unwrap()
            .unwrap();
        got.insert(u, l);
    }
    assert_eq!(got["rp-alice"], alice);
    assert_eq!(got["rp-bob"], bob);

    // the mock's handshake replies were captured to the results file
    let results = std::fs::read_to_string(res_path).unwrap();
    assert_eq!(results.lines().count(), 2);
}

#[tokio::test]
async fn replay_refuses_public_targets() {
    let r = replay::run(replay::Opts {
        input: tmp("none.jsonl"),
        target: "8.8.8.8:31090".into(),
        speed: 1.0,
        prefix: "rp-".into(),
        results: None,
        max_secs: None,
        allow_public: false,
        udp: false,
    })
    .await;
    assert!(r.unwrap_err().to_string().contains("refusing"));
}
