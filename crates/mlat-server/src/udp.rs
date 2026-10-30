//! The UDP transport (`PackedMlatServerProtocol` in jsonclient.py): a receiver
//! that asked for `udp_transport: 2` sends its sync and mlat messages as packed
//! datagrams, authenticated only by the random key it was given in the
//! handshake response.

use crate::state::{log, now, Server};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;

const TYPE_SYNC: u8 = 1;
const TYPE_MLAT_SHORT: u8 = 2;
const TYPE_MLAT_LONG: u8 = 3;
const TYPE_REBASE: u8 = 5;
const TYPE_ABS_SYNC: u8 = 6;

const HEADER: usize = 4 + 2 + 8; // key, sequence, base timestamp

fn be_u64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

fn be_i32(b: &[u8]) -> i32 {
    i32::from_be_bytes(b[..4].try_into().unwrap())
}

/// `base + offset` as the exact integer Python would have, then as a double.
fn offset_time(base: u64, off: i32) -> f64 {
    (base as i128 + off as i128) as f64
}

/// Handles one datagram. A short or unknown-key datagram is dropped silently,
/// as in Python; an unknown record type abandons the rest of the datagram.
pub fn datagram(server: &Server, data: &[u8], from: SocketAddr, utc: f64) {
    if data.len() < HEADER {
        return;
    }
    let key = u32::from_be_bytes(data[..4].try_into().unwrap());
    let Some(conn) = server.udp_clients.read().unwrap().get(&key).cloned() else { return };
    let e = &server.engine;
    let mut base = be_u64(&data[6..14]);

    let mut i = HEADER;
    while i < data.len() {
        let kind = data[i];
        i += 1;
        let rest = &data[i..];
        match kind {
            TYPE_SYNC => {
                if rest.len() < 36 {
                    return;
                }
                let (et, ot) = (be_i32(&rest[0..4]), be_i32(&rest[4..8]));
                let em: [u8; 14] = rest[8..22].try_into().unwrap();
                let om: [u8; 14] = rest[22..36].try_into().unwrap();
                e.sync_from(&conn.recv, utc, offset_time(base, et), offset_time(base, ot), em, om);
                i += 36;
            }
            TYPE_ABS_SYNC => {
                if rest.len() < 44 {
                    return;
                }
                let (et, ot) = (be_u64(&rest[0..8]), be_u64(&rest[8..16]));
                let em: [u8; 14] = rest[16..30].try_into().unwrap();
                let om: [u8; 14] = rest[30..44].try_into().unwrap();
                e.sync_from(&conn.recv, utc, et as f64, ot as f64, em, om);
                i += 44;
            }
            // unlike the TCP path, no bad_syncs / last_sync gate is applied here
            TYPE_MLAT_SHORT | TYPE_MLAT_LONG => {
                let n = if kind == TYPE_MLAT_SHORT { 7 } else { 14 };
                if rest.len() < 4 + n {
                    return;
                }
                e.mlat_from_ungated(&conn.recv, utc, offset_time(base, be_i32(&rest[0..4])), rest[4..4 + n].to_vec());
                i += 4 + n;
            }
            TYPE_REBASE => {
                if rest.len() < 8 {
                    return;
                }
                base = be_u64(rest);
                i += 8;
            }
            _ => {
                log("client", &format!("bad UDP packet from {}:{}", from.ip(), from.port()));
                return;
            }
        }
    }
}

/// Receives datagrams on `socket` until the process exits.
pub async fn serve(server: Arc<Server>, socket: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65536];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((n, from)) => datagram(&server, &buf[..n], from, now()),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_are_exact_integers() {
        assert_eq!(offset_time(1 << 60, -5), ((1u64 << 60) - 5) as f64);
        assert_eq!(offset_time(100, 7), 107.0);
        assert_eq!(offset_time(3, -10), -7.0);
    }
}
