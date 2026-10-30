use std::sync::atomic::{AtomicU64, Ordering};

/// An f64 that can be shared between threads.
#[derive(Debug, Default)]
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    pub fn new(v: f64) -> Self {
        AtomicF64(AtomicU64::new(v.to_bits()))
    }

    #[inline]
    pub fn load(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn store(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }

    /// Adds `d` and returns the new value.
    pub fn add(&self, d: f64) -> f64 {
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            let new = f64::from_bits(cur) + d;
            match self.0.compare_exchange_weak(cur, new.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return new,
                Err(c) => cur = c,
            }
        }
    }
}
