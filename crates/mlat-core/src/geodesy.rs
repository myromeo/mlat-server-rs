//! WGS84 coordinate conversion, matching mlat/geodesy.pyx.

const DTOR: f64 = std::f64::consts::PI / 180.0;

/// `x ** 2` as the Python server evaluates it.
#[inline]
pub fn sq(x: f64) -> f64 {
    #[cfg(feature = "glibc-pow")]
    {
        libm_pow(x, 2.0)
    }
    #[cfg(not(feature = "glibc-pow"))]
    {
        x * x
    }
}

/// `x ** 3` as the Python server evaluates it.
#[inline]
pub fn cube(x: f64) -> f64 {
    #[cfg(feature = "glibc-pow")]
    {
        libm_pow(x, 3.0)
    }
    #[cfg(not(feature = "glibc-pow"))]
    {
        x.powf(3.0)
    }
}

#[cfg(feature = "glibc-pow")]
fn libm_pow(x: f64, y: f64) -> f64 {
    extern "C" {
        fn pow(x: f64, y: f64) -> f64;
    }
    // black_box stops the compiler folding pow(x, 2.0) into x * x
    unsafe { pow(x, std::hint::black_box(y)) }
}
const WGS84_A: f64 = 6378137.0;
const WGS84_F: f64 = 1.0 / 298.257223563;

fn wgs84_b() -> f64 {
    WGS84_A * (1.0 - WGS84_F)
}

fn ecc_sq() -> f64 {
    let b = wgs84_b();
    1.0 - b * b / (WGS84_A * WGS84_A)
}

/// Converts WGS84 latitude/longitude (degrees) and height (m) to ECEF.
pub fn llh2ecef(lat_deg: f64, lon_deg: f64, alt: f64) -> [f64; 3] {
    let lat = lat_deg * DTOR;
    let lng = lon_deg * DTOR;
    let (slat, clat) = (lat.sin(), lat.cos());
    let (slng, clng) = (lng.sin(), lng.cos());
    let e2 = ecc_sq();
    let d = (1.0 - (slat * slat * e2)).sqrt();
    let rn = WGS84_A / d;
    [
        (rn + alt) * clat * clng,
        (rn + alt) * clat * slng,
        (rn * (1.0 - e2) + alt) * slat,
    ]
}

/// Straight-line distance in metres between two ECEF points.
#[inline]
pub fn ecef_distance(p0: &[f64; 3], p1: &[f64; 3]) -> f64 {
    (sq(p0[0] - p1[0]) + sq(p0[1] - p1[1]) + sq(p0[2] - p1[2])).sqrt()
}

const RTOD: f64 = 180.0 / std::f64::consts::PI;

/// Converts ECEF to WGS84 latitude/longitude (degrees) and height (m).
pub fn ecef2llh(ecef: &[f64; 3]) -> [f64; 3] {
    let (x, y, z) = (ecef[0], ecef[1], ecef[2]);
    let b = wgs84_b();
    let e2 = ecc_sq();
    let ep = ((sq(WGS84_A) - sq(b)) / sq(b)).sqrt();
    let ep2_b = sq(ep) * b;
    let e2_a = e2 * WGS84_A;

    let lon = y.atan2(x);
    let p = (sq(x) + sq(y)).sqrt();
    let th = (WGS84_A * z).atan2(b * p);
    let lat = (z + ep2_b * cube(th.sin())).atan2(p - e2_a * cube(th.cos()));

    let n = WGS84_A / (1.0 - e2 * sq(lat.sin())).sqrt();
    let alt = p / lat.cos() - n;

    [lat * RTOD, lon * RTOD, alt]
}
