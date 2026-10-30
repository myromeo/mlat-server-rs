//! Minimal reader for classic pcap files (what `tcpdump -w` writes) and the
//! Ethernet / IP / TCP header parsing needed to follow one TCP port.

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const FLAG_FIN: u8 = 0x01;
pub const FLAG_SYN: u8 = 0x02;
pub const FLAG_RST: u8 = 0x04;
pub const FLAG_ACK: u8 = 0x10;

pub struct PcapReader<R: Read> {
    r: R,
    big_endian: bool,
    nanos: bool,
    link: u32,
}

pub struct Packet {
    /// Seconds since the epoch.
    pub ts: f64,
    pub data: Vec<u8>,
}

pub struct TcpSegment<'a> {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub flags: u8,
    pub payload: &'a [u8],
}

impl<R: Read> PcapReader<R> {
    pub fn new(mut r: R) -> io::Result<Self> {
        let mut h = [0u8; 24];
        r.read_exact(&mut h)?;
        let (big_endian, nanos) = match [h[0], h[1], h[2], h[3]] {
            [0xd4, 0xc3, 0xb2, 0xa1] => (false, false),
            [0xa1, 0xb2, 0xc3, 0xd4] => (true, false),
            [0x4d, 0x3c, 0xb2, 0xa1] => (false, true),
            [0xa1, 0xb2, 0x3c, 0x4d] => (true, true),
            [0x0a, 0x0d, 0x0d, 0x0a] => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pcapng is not supported; capture with `tcpdump -w` (classic pcap)",
                ))
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "not a pcap file")),
        };
        let mut me = PcapReader { r, big_endian, nanos, link: 0 };
        me.link = me.u32_at(&h, 20);
        Ok(me)
    }

    fn u32_at(&self, b: &[u8], i: usize) -> u32 {
        let a = [b[i], b[i + 1], b[i + 2], b[i + 3]];
        if self.big_endian {
            u32::from_be_bytes(a)
        } else {
            u32::from_le_bytes(a)
        }
    }

    pub fn link_type(&self) -> u32 {
        self.link
    }

    /// Next packet, or None at end of file (a truncated final record, e.g.
    /// from a killed tcpdump, counts as the end).
    pub fn next_packet(&mut self) -> io::Result<Option<Packet>> {
        let mut h = [0u8; 16];
        match self.r.read_exact(&mut h) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let sec = self.u32_at(&h, 0) as f64;
        let frac = self.u32_at(&h, 4) as f64;
        let incl = self.u32_at(&h, 8) as usize;
        if incl > 1 << 24 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "implausible packet length"));
        }
        let mut data = vec![0u8; incl];
        match self.r.read_exact(&mut data) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let ts = sec + frac / if self.nanos { 1e9 } else { 1e6 };
        Ok(Some(Packet { ts, data }))
    }
}

/// Parse a captured frame down to its TCP segment, if it carries one.
pub fn parse_tcp(link: u32, frame: &[u8]) -> Option<TcpSegment<'_>> {
    let (ip, mut proto) = match link {
        1 => {
            // Ethernet, possibly with VLAN tags
            let mut off = 14;
            let mut et = u16::from_be_bytes([*frame.get(12)?, *frame.get(13)?]);
            while et == 0x8100 || et == 0x88a8 {
                et = u16::from_be_bytes([*frame.get(off + 2)?, *frame.get(off + 3)?]);
                off += 4;
            }
            (frame.get(off..)?, et)
        }
        113 => {
            // Linux cooked v1
            let et = u16::from_be_bytes([*frame.get(14)?, *frame.get(15)?]);
            (frame.get(16..)?, et)
        }
        276 => {
            // Linux cooked v2
            let et = u16::from_be_bytes([*frame.get(0)?, *frame.get(1)?]);
            (frame.get(20..)?, et)
        }
        101 | 12 => (frame, 0),
        _ => return None,
    };
    if proto == 0 {
        proto = match ip.first()? >> 4 {
            4 => 0x0800,
            6 => 0x86dd,
            _ => return None,
        };
    }

    let (src, dst, tcp) = match proto {
        0x0800 => {
            let ihl = (ip.first()? & 0x0f) as usize * 4;
            if ip.len() < 20 || ihl < 20 || ip[9] != 6 {
                return None;
            }
            // skip non-first fragments
            if u16::from_be_bytes([ip[6], ip[7]]) & 0x1fff != 0 {
                return None;
            }
            let total = (u16::from_be_bytes([ip[2], ip[3]]) as usize).min(ip.len());
            let src = IpAddr::V4(Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]));
            let dst = IpAddr::V4(Ipv4Addr::new(ip[16], ip[17], ip[18], ip[19]));
            (src, dst, ip.get(ihl..total)?)
        }
        0x86dd => {
            if ip.len() < 40 || ip[6] != 6 {
                return None;
            }
            let plen = u16::from_be_bytes([ip[4], ip[5]]) as usize;
            let mut s = [0u8; 16];
            let mut d = [0u8; 16];
            s.copy_from_slice(&ip[8..24]);
            d.copy_from_slice(&ip[24..40]);
            let end = (40 + plen).min(ip.len());
            (IpAddr::V6(Ipv6Addr::from(s)), IpAddr::V6(Ipv6Addr::from(d)), ip.get(40..end)?)
        }
        _ => return None,
    };

    if tcp.len() < 20 {
        return None;
    }
    let doff = (tcp[12] >> 4) as usize * 4;
    if doff < 20 || tcp.len() < doff {
        return None;
    }
    Some(TcpSegment {
        src,
        dst,
        sport: u16::from_be_bytes([tcp[0], tcp[1]]),
        dport: u16::from_be_bytes([tcp[2], tcp[3]]),
        seq: u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]),
        flags: tcp[13],
        payload: &tcp[doff..],
    })
}
