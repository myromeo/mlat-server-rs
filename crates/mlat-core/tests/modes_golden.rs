//! Rust vs the real compiled Python modules, on fixtures from
//! testbed/gen_modes_golden.py.

use mlat_core::geodesy::{ecef_distance, llh2ecef};
use mlat_core::modes::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Decoded {
    address: u32,
    crc_ok: bool,
    estype: String,
    nuc: Option<u8>,
    altitude: Option<i32>,
    #[serde(rename = "F")]
    f: Option<u8>,
    #[serde(rename = "LAT")]
    lat: Option<u32>,
    #[serde(rename = "LON")]
    lon: Option<u32>,
}
#[derive(Deserialize)]
struct Msg {
    hex: String,
    df: u8,
    decoded: Option<Decoded>,
}
#[derive(Deserialize)]
struct Cpr {
    #[serde(rename = "in")]
    input: [f64; 4],
    out: Option<[f64; 4]>,
}
#[derive(Deserialize)]
struct Llh {
    llh: [f64; 3],
    ecef: [f64; 3],
    other_llh: [f64; 3],
    dist: f64,
}
#[derive(Deserialize)]
struct Fixture {
    messages: Vec<Msg>,
    ac12: Vec<Option<i32>>,
    cpr: Vec<Cpr>,
    llh: Vec<Llh>,
}

fn load() -> Fixture {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/modes_golden.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn hex14(s: &str) -> [u8; 14] {
    let mut out = [0u8; 14];
    for i in 0..14 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

fn estype_name(e: EsType) -> &'static str {
    match e {
        EsType::IdAndCategory => "id_and_category",
        EsType::AirbornePosition => "airborne_position",
        EsType::SurfacePosition => "surface_position",
        EsType::AirborneVelocity => "airborne_velocity",
        EsType::Other => "other",
    }
}

#[test]
fn altitude_table_matches_for_every_code() {
    for (v, expect) in load().ac12.iter().enumerate() {
        assert_eq!(decode_ac12(v as u32), *expect, "ac12 = {v:#x}");
    }
}

#[test]
fn df17_decode_matches() {
    let fx = load();
    let (mut with_decode, mut crc_ok, mut airborne) = (0, 0, 0);
    for m in &fx.messages {
        let msg = hex14(&m.hex);
        assert_eq!(downlink_format(&msg), m.df);
        let got = decode_df17(&msg);
        match (&m.decoded, got) {
            (None, None) => {}
            (Some(e), Some(g)) => {
                with_decode += 1;
                assert_eq!(g.address, e.address, "{}", m.hex);
                assert_eq!(g.crc_ok, e.crc_ok, "{}", m.hex);
                assert_eq!(estype_name(g.estype), e.estype, "{}", m.hex);
                assert_eq!(g.nuc, e.nuc, "{}", m.hex);
                assert_eq!(g.altitude, e.altitude, "{}", m.hex);
                crc_ok += e.crc_ok as u32;
                if e.estype == "airborne_position" {
                    airborne += 1;
                    assert_eq!(Some(g.f), e.f, "{}", m.hex);
                    assert_eq!(Some(g.lat), e.lat, "{}", m.hex);
                    assert_eq!(Some(g.lon), e.lon, "{}", m.hex);
                }
            }
            (e, g) => panic!("decode presence differs for {}: python {} rust {}", m.hex, e.is_some(), g.is_some()),
        }
    }
    println!("DF17 messages compared: {with_decode} (crc ok {crc_ok}, airborne {airborne})");
    assert!(with_decode > 3000 && crc_ok > 1500 && airborne > 300);
}

#[test]
fn cpr_decode_matches() {
    let fx = load();
    let (mut ok, mut err) = (0, 0);
    for c in &fx.cpr {
        let got = decode_cpr(c.input[0], c.input[1], c.input[2], c.input[3]);
        match (&c.out, got) {
            (None, None) => err += 1,
            (Some(e), Some(g)) => {
                ok += 1;
                for k in 0..4 {
                    assert!((e[k] - g[k]).abs() < 1e-12, "{:?}: {:?} vs {:?}", c.input, e, g);
                }
            }
            (e, g) => panic!("cpr success differs for {:?}: python {:?} rust {:?}", c.input, e, g),
        }
    }
    println!("CPR cases: {ok} decoded, {err} rejected (all agree)");
    assert!(ok > 100 && err > 100);
}

#[test]
fn geodesy_matches() {
    for l in &load().llh {
        let e = llh2ecef(l.llh[0], l.llh[1], l.llh[2]);
        let o = llh2ecef(l.other_llh[0], l.other_llh[1], l.other_llh[2]);
        for k in 0..3 {
            assert!((e[k] - l.ecef[k]).abs() < 1e-6, "{:?}", l.llh);
        }
        assert!((ecef_distance(&e, &o) - l.dist).abs() < 1e-6);
    }
}
