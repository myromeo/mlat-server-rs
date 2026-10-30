//! Mode S / ADS-B decoding needed by clock synchronisation, ported from
//! modes_cython/message.pyx: CRC, the altitude codes, DF17 airborne position
//! fields and global CPR decoding.

use std::sync::OnceLock;

pub const BAD_ALT: i32 = -999_999;

const CRC_POLY: u32 = 0xfff409;

const fn make_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 16;
        let mut j = 0;
        while j < 8 {
            if c & 0x80_0000 != 0 {
                c = (c << 1) ^ CRC_POLY;
            } else {
                c <<= 1;
            }
            j += 1;
        }
        t[i] = c & 0xff_ffff;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = make_crc_table();

/// The 24-bit Mode S CRC residual (zero for a good extended squitter).
pub fn crc_residual(payload: &[u8]) -> u32 {
    let df = (payload[0] & 0xf8) >> 3;
    let n = if df > 15 { 14 } else { 7 };
    let mut rem = CRC_TABLE[payload[0] as usize];
    for &b in &payload[1..n - 3] {
        rem = ((rem & 0xffff) << 8) ^ CRC_TABLE[(b as u32 ^ (rem >> 16)) as usize];
    }
    rem ^ ((payload[n - 3] as u32) << 16) ^ ((payload[n - 2] as u32) << 8) ^ (payload[n - 1] as u32)
}

fn decode_ac13_slow(ac13: u32) -> i32 {
    if ac13 == 0 {
        return BAD_ALT;
    }
    if ac13 & 0x0040 != 0 {
        return BAD_ALT; // M bit
    }
    if ac13 & 0x0010 != 0 {
        // Q bit set
        let n = ((ac13 & 0x1f80) >> 2) | ((ac13 & 0x0020) >> 1) | (ac13 & 0x000f);
        return n as i32 * 25 - 1000;
    }

    // Gillham code
    if ac13 & 0x1500 == 0 {
        return BAD_ALT;
    }
    let mut h: i32 = 0;
    if ac13 & 0x1000 != 0 {
        h ^= 7;
    }
    if ac13 & 0x0400 != 0 {
        h ^= 3;
    }
    if ac13 & 0x0100 != 0 {
        h ^= 1;
    }
    if h & 5 != 0 {
        h ^= 5;
    }
    if h > 5 {
        return BAD_ALT;
    }

    let mut f: i32 = 0;
    for (bit, mask) in [
        (0x0010, 0x1ff),
        (0x0004, 0x0ff),
        (0x0001, 0x07f),
        (0x0800, 0x03f),
        (0x0200, 0x01f),
        (0x0080, 0x00f),
        (0x0020, 0x007),
        (0x0008, 0x003),
        (0x0002, 0x001),
    ] {
        if ac13 & bit != 0 {
            f ^= mask;
        }
    }
    if f & 1 != 0 {
        h = 6 - h;
    }
    let a = 500 * f + 100 * h - 1300;
    if a < -1200 {
        return BAD_ALT;
    }
    a
}

fn alt_table() -> &'static [i32; 8192] {
    static T: OnceLock<[i32; 8192]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0i32; 8192];
        for (i, v) in t.iter_mut().enumerate() {
            *v = decode_ac13_slow(i as u32);
        }
        t
    })
}

/// Decodes the 12-bit altitude field of an extended squitter, in feet.
pub fn decode_ac12(ac12: u32) -> Option<i32> {
    let idx = ((ac12 & 0x0fc0) << 1) | (ac12 & 0x003f);
    let alt = alt_table()[idx as usize];
    (alt != BAD_ALT).then_some(alt)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EsType {
    IdAndCategory,
    AirbornePosition,
    SurfacePosition,
    AirborneVelocity,
    Other,
}

/// (type, nuc) for an extended squitter type code, as in `es_types`.
fn es_type(metype: u8) -> (EsType, Option<u8>) {
    use EsType::*;
    match metype {
        0 => (AirbornePosition, Some(0)),
        1..=4 => (IdAndCategory, None),
        5 => (SurfacePosition, Some(9)),
        6 => (SurfacePosition, Some(8)),
        7 => (SurfacePosition, Some(7)),
        8 => (SurfacePosition, Some(6)),
        9 => (AirbornePosition, Some(9)),
        10 => (AirbornePosition, Some(8)),
        11 => (AirbornePosition, Some(7)),
        12 => (AirbornePosition, Some(6)),
        13 => (AirbornePosition, Some(5)),
        14 => (AirbornePosition, Some(4)),
        15 => (AirbornePosition, Some(3)),
        16 => (AirbornePosition, Some(2)),
        17 => (AirbornePosition, Some(1)),
        18 => (AirbornePosition, Some(0)),
        19 => (AirborneVelocity, None),
        20 => (AirbornePosition, Some(9)),
        21 => (AirbornePosition, Some(8)),
        22 => (AirbornePosition, Some(0)),
        _ => (Other, None),
    }
}

/// The fields of a DF17 extended squitter that clock sync looks at.
#[derive(Clone, Debug)]
pub struct Df17 {
    pub address: u32,
    pub crc_ok: bool,
    pub estype: EsType,
    pub nuc: Option<u8>,
    /// CPR format flag (airborne positions only).
    pub f: u8,
    pub lat: u32,
    pub lon: u32,
    pub altitude: Option<i32>,
}

/// Downlink format of a message.
#[inline]
pub fn downlink_format(msg: &[u8; 14]) -> u8 {
    (msg[0] & 0xf8) >> 3
}

/// Decodes a DF17 message; None for any other downlink format.
pub fn decode_df17(msg: &[u8; 14]) -> Option<Df17> {
    if downlink_format(msg) != 17 {
        return None;
    }
    let (estype, nuc) = es_type((msg[4] & 0xf8) >> 3);
    let mut d = Df17 {
        address: ((msg[1] as u32) << 16) | ((msg[2] as u32) << 8) | msg[3] as u32,
        crc_ok: crc_residual(msg) == 0,
        estype,
        nuc,
        f: 0,
        lat: 0,
        lon: 0,
        altitude: None,
    };
    if estype == EsType::AirbornePosition {
        let ac12 = ((msg[5] as u32) << 4) | ((msg[6] as u32 & 0xf0) >> 4);
        d.f = (msg[6] & 0x04) >> 2;
        d.lat = ((msg[6] as u32 & 0x03) << 15) | ((msg[7] as u32) << 7) | ((msg[8] as u32 & 0xfe) >> 1);
        d.lon = ((msg[8] as u32 & 0x01) << 16) | ((msg[9] as u32) << 8) | msg[10] as u32;
        d.altitude = decode_ac12(ac12);
    }
    Some(d)
}

/// CPR number of longitude zones for a latitude.
fn nl(lat: f64) -> i32 {
    const THRESHOLDS: [f64; 58] = [
        10.47047130, 14.82817437, 18.18626357, 21.02939493, 23.54504487, 25.82924707, 27.93898710,
        29.91135686, 31.77209708, 33.53993436, 35.22899598, 36.85025108, 38.41241892, 39.92256684,
        41.38651832, 42.80914012, 44.19454951, 45.54626723, 46.86733252, 48.16039128, 49.42776439,
        50.67150166, 51.89342469, 53.09516153, 54.27817472, 55.44378444, 56.59318756, 57.72747354,
        58.84763776, 59.95459277, 61.04917774, 62.13216659, 63.20427479, 64.26616523, 65.31845310,
        66.36171008, 67.39646774, 68.42322022, 69.44242631, 70.45451075, 71.45986473, 72.45884545,
        73.45177442, 74.43893416, 75.42056257, 76.39684391, 77.36789461, 78.33374083, 79.29428225,
        80.24923213, 81.19801349, 82.13956981, 83.07199445, 83.99173563, 84.89166191, 85.75541621,
        86.53536998, 87.00000000,
    ];
    let lat = lat.abs();
    for (i, &t) in THRESHOLDS.iter().enumerate() {
        if lat < t {
            return 59 - i as i32;
        }
    }
    1
}

fn modulo(a: i32, b: i32) -> i32 {
    let r = a % b;
    if r < 0 {
        r + b
    } else {
        r
    }
}

/// Globally unambiguous CPR decode of an even/odd airborne pair. Returns
/// (even lat, even lon, odd lat, odd lon), or None if the pair is unusable.
pub fn decode_cpr(lat_e: f64, lon_e: f64, lat_o: f64, lon_o: f64) -> Option<[f64; 4]> {
    let j = ((59.0 * lat_e - 60.0 * lat_o) / 131072.0 + 0.5).floor() as i32;
    let mut rlat_e = (360.0 / 60.0) * (modulo(j, 60) as f64 + lat_e / 131072.0);
    let mut rlat_o = (360.0 / 59.0) * (modulo(j, 59) as f64 + lat_o / 131072.0);

    if rlat_e >= 270.0 {
        rlat_e -= 360.0;
    }
    if rlat_o >= 270.0 {
        rlat_o -= 360.0;
    }
    if !(-90.0..=90.0).contains(&rlat_e) || !(-90.0..=90.0).contains(&rlat_o) {
        return None;
    }

    let nlat = nl(rlat_e);
    if nlat != nl(rlat_o) {
        return None;
    }
    let n_e = nlat;
    let n_o = (nlat - 1).max(1);

    let m = (((lon_e * (nlat - 1) as f64) - (lon_o * nlat as f64)) / 131072.0 + 0.5).floor() as i32;
    let mut rlon_e = (360.0 / n_e as f64) * (modulo(m, n_e) as f64 + lon_e / 131072.0);
    let mut rlon_o = (360.0 / n_o as f64) * (modulo(m, n_o) as f64 + lon_o / 131072.0);
    rlon_e -= ((rlon_e + 180.0) / 360.0).floor() * 360.0;
    rlon_o -= ((rlon_o + 180.0) / 360.0).floor() * 360.0;

    Some([rlat_e, rlon_e, rlat_o, rlon_o])
}

/// Altitude from a 13-bit altitude code (DF0/4/16/20), feet.
pub fn decode_ac13(ac13: u32) -> Option<i32> {
    let alt = alt_table()[(ac13 & 0x1fff) as usize];
    (alt != BAD_ALT).then_some(alt)
}

fn id13_lower() -> &'static [u32; 64] {
    static T: OnceLock<[u32; 64]> = OnceLock::new();
    T.get_or_init(|| {
        let mut lt = [0u32; 64];
        for (id13, v) in lt.iter_mut().enumerate() {
            let id13 = id13 as u32;
            if id13 & 0x0020 != 0 {
                *v |= 0x0100;
            }
            if id13 & 0x0010 != 0 {
                *v |= 0x0001;
            }
            if id13 & 0x0008 != 0 {
                *v |= 0x0200;
            }
            if id13 & 0x0004 != 0 {
                *v |= 0x0002;
            }
            if id13 & 0x0002 != 0 {
                *v |= 0x0400;
            }
            if id13 & 0x0001 != 0 {
                *v |= 0x0004;
            }
        }
        lt
    })
}

fn id13_upper() -> &'static [u32; 64] {
    static T: OnceLock<[u32; 64]> = OnceLock::new();
    T.get_or_init(|| {
        let mut ut = [0u32; 64];
        for (i, v) in ut.iter_mut().enumerate() {
            let id13 = (i as u32) << 7;
            if id13 & 0x1000 != 0 {
                *v |= 0x0010;
            }
            if id13 & 0x0800 != 0 {
                *v |= 0x1000;
            }
            if id13 & 0x0400 != 0 {
                *v |= 0x0020;
            }
            if id13 & 0x0200 != 0 {
                *v |= 0x2000;
            }
            if id13 & 0x0100 != 0 {
                *v |= 0x0040;
            }
            if id13 & 0x0080 != 0 {
                *v |= 0x4000;
            }
        }
        ut
    })
}

/// A 13-bit Mode A code as the 4-character squawk string.
pub fn decode_id13(id13: u32) -> String {
    format!("{:04x}", id13_lower()[(id13 & 63) as usize] | id13_upper()[((id13 >> 7) & 63) as usize])
}

const AIS_CHARSET: &[u8; 64] = b" ABCDEFGHIJKLMNOPQRSTUVWXYZ????? ???????????????0123456789??????";

/// The eight AIS-coded characters in bytes 5..=10 of an identification message.
fn ais_callsign(m: &[u8]) -> String {
    let idx = [
        (m[5] & 0xfc) >> 2,
        ((m[5] & 0x03) << 4) | ((m[6] & 0xf0) >> 4),
        ((m[6] & 0x0f) << 2) | ((m[7] & 0xc0) >> 6),
        m[7] & 0x3f,
        (m[8] & 0xfc) >> 2,
        ((m[8] & 0x03) << 4) | ((m[9] & 0xf0) >> 4),
        ((m[9] & 0x0f) << 2) | ((m[10] & 0xc0) >> 6),
        m[10] & 0x3f,
    ];
    idx.iter().map(|&i| AIS_CHARSET[i as usize] as char).collect()
}

/// Comm-B callsign: only present for BDS 2,0, and only if it looks valid.
fn commb_callsign(m: &[u8]) -> Option<String> {
    if m[4] != 0x20 {
        return None;
    }
    let cs = ais_callsign(m);
    (cs != "        " && !cs.contains('?')).then_some(cs)
}

/// What a message reveals beyond its position fields.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub df: u8,
    pub address: u32,
    pub altitude: Option<i32>,
    pub squawk: Option<String>,
    pub callsign: Option<String>,
}

/// Decodes any Mode S message the Python decoder handles; None for other
/// downlink formats or a buffer too short for its format.
pub fn decode_any(msg: &[u8]) -> Option<Decoded> {
    let df = downlink_format_of(msg)?;
    let long = |m: &[u8]| m.len() >= 14;
    let ac = |m: &[u8]| ((m[2] as u32 & 0x1f) << 8) | m[3] as u32;
    match df {
        0 | 4 if msg.len() >= 7 => Some(Decoded { df, address: crc_residual(msg), altitude: decode_ac13(ac(msg)), squawk: None, callsign: None }),
        5 if msg.len() >= 7 => Some(Decoded { df, address: crc_residual(msg), altitude: None, squawk: Some(decode_id13(ac(msg))), callsign: None }),
        11 if msg.len() >= 7 => Some(Decoded {
            df,
            address: ((msg[1] as u32) << 16) | ((msg[2] as u32) << 8) | msg[3] as u32,
            altitude: None,
            squawk: None,
            callsign: None,
        }),
        16 if long(msg) => Some(Decoded { df, address: crc_residual(msg), altitude: decode_ac13(ac(msg)), squawk: None, callsign: None }),
        17 | 18 if long(msg) => {
            let (estype, _) = es_type((msg[4] & 0xf8) >> 3);
            let (altitude, callsign) = match estype {
                EsType::AirbornePosition => (decode_ac12(((msg[5] as u32) << 4) | ((msg[6] as u32 & 0xf0) >> 4)), None),
                EsType::IdAndCategory => (None, Some(ais_callsign(msg))),
                _ => (None, None),
            };
            Some(Decoded { df, address: ((msg[1] as u32) << 16) | ((msg[2] as u32) << 8) | msg[3] as u32, altitude, squawk: None, callsign })
        }
        20 if long(msg) => Some(Decoded { df, address: crc_residual(msg), altitude: decode_ac13(ac(msg)), squawk: None, callsign: commb_callsign(msg) }),
        21 if long(msg) => Some(Decoded { df, address: crc_residual(msg), altitude: None, squawk: Some(decode_id13(ac(msg))), callsign: commb_callsign(msg) }),
        _ => None,
    }
}

fn downlink_format_of(msg: &[u8]) -> Option<u8> {
    msg.first().map(|b| (b & 0xf8) >> 3)
}
