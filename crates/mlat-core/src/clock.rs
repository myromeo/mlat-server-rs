//! Receiver clock models, matching make_clock() in clocktrack.pyx.

use crate::C_AIR;

#[derive(Clone, Copy, Debug)]
pub struct Clock {
    /// Clock frequency in Hz.
    pub freq: f64,
    /// Maximum expected relative frequency error (1e-6 is 1 ppm).
    pub max_freq_error: f64,
    /// Expected jitter of a typical reading, seconds (std deviation).
    pub jitter: f64,
    /// Clock ticks per metre of signal path.
    pub delay_factor: f64,
}

impl Clock {
    fn new(freq: f64, max_freq_error: f64, jitter: f64) -> Clock {
        Clock { freq, max_freq_error, jitter, delay_factor: freq / C_AIR }
    }

    /// The clock for a handshake `clock_type`; None for unknown types.
    pub fn from_type(clock_type: &str) -> Option<Clock> {
        match clock_type {
            "radarcape_gps" | "radarcape" => Some(Clock::new(1e9, 1e-6, 15e-9)),
            "beast" | "radarcape_12mhz" => Some(Clock::new(12e6, 5e-6, 83e-9)),
            "sbs" => Some(Clock::new(20e6, 100e-6, 500e-9)),
            "dump1090" | "unknown" => Some(Clock::new(12e6, 100e-6, 500e-9)),
            _ => None,
        }
    }
}
