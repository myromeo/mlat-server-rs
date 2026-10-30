//! Core multilateration engine pieces: coordinates, Mode S decoding and clock
//! synchronisation, ported from the Python/Cython mlat-server.

pub mod atomic;
pub mod clock;
pub mod cluster;
pub mod collector;
pub mod engine;
pub mod geodesy;
pub mod kalman;
pub mod linalg;
pub mod minpack;
pub mod modes;
pub mod normalize;
pub mod output;
pub mod pairing;
pub mod receiver;
pub mod resolve;
pub mod solver;

/// Speed of light in air, as used by the Python server.
pub const C_AIR: f64 = 299_792_458.0 / 1.00032;
/// Feet to metres, exactly as in the Python server's constants (not 0.3048).
pub const FTOM: f64 = 0.3038;
