//! The capture file: JSON lines, one event each, timestamps in seconds
//! relative to the first packet of the capture.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    /// First line of every capture file.
    Meta { start_epoch: f64, port: u16 },
    /// A client connected (SYN seen).
    Open { c: u64, t: f64, client: String },
    /// The client's handshake line, as sent.
    Hs {
        c: u64,
        t: f64,
        line: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proxy: Option<String>,
    },
    /// The server's handshake response line.
    HsResp { c: u64, t: f64, line: String },
    /// One decoded message line, client -> server.
    C2s { c: u64, t: f64, l: String },
    /// One decoded message line, server -> client.
    S2c { c: u64, t: f64, l: String },
    /// Connection ended. `damaged` means the stream could not be fully decoded.
    Close { c: u64, t: f64, damaged: bool },
}

impl Event {
    /// Connection id, if the event belongs to one.
    pub fn conn(&self) -> Option<u64> {
        match self {
            Event::Meta { .. } => None,
            Event::Open { c, .. }
            | Event::Hs { c, .. }
            | Event::HsResp { c, .. }
            | Event::C2s { c, .. }
            | Event::S2c { c, .. }
            | Event::Close { c, .. } => Some(*c),
        }
    }

    pub fn time(&self) -> f64 {
        match self {
            Event::Meta { .. } => 0.0,
            Event::Open { t, .. }
            | Event::Hs { t, .. }
            | Event::HsResp { t, .. }
            | Event::C2s { t, .. }
            | Event::S2c { t, .. }
            | Event::Close { t, .. } => *t,
        }
    }
}

/// The message type of a protocol line: the first JSON key, e.g. `sync`.
pub fn message_kind(line: &str) -> &str {
    let rest = line.trim_start().strip_prefix('{').unwrap_or(line).trim_start();
    match rest.strip_prefix('"') {
        Some(r) => r.split('"').next().unwrap_or("?"),
        None => "?",
    }
}
