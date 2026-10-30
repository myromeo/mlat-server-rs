//! Collects the copies of each Mode S message that different receivers send,
//! and releases them for resolving after a delay, as `receiver_mlat` and the
//! cohort timers do in mlattrack.py.
//!
//! Groups are released in cohorts: a cohort is opened by the first message of
//! a burst and holds the groups created within 50 ms of it (at most 26). Its
//! timer fires `MLAT_DELAY` after the cohort was opened, so a group is
//! resolved between 0.85 and 0.9 s after its first copy arrived.

use crate::resolve::Group;
use std::collections::{HashMap, VecDeque};

pub const MLAT_DELAY: f64 = 0.9;
/// Copies kept per message; further copies are only remembered as receivers.
pub const MAX_GROUP: usize = 15;

struct Cohort {
    created: f64,
    messages: Vec<Vec<u8>>,
}

#[derive(Default)]
pub struct Collector {
    pending: HashMap<Vec<u8>, Group>,
    cohorts: VecDeque<Cohort>,
    placeholders: u64,
}

impl Collector {
    pub fn new() -> Collector {
        Collector::default()
    }

    /// One copy of `message`, heard by receiver `uid` at its clock time
    /// `timestamp`, arriving at server time `now`.
    pub fn add(&mut self, uid: u32, timestamp: f64, message: &[u8], now: f64) {
        self.add_inner(uid, timestamp, message, now, false);
    }

    /// The Python server's sync path hands `receiver_mlat` a *decoded message
    /// object* instead of the raw bytes when it first sees a sync pair. The object
    /// never matches any other copy, so it becomes a one-copy group that resolves
    /// to nothing, but it still takes a slot in the current cohort (and can open
    /// a new one). This reproduces that.
    pub fn add_placeholder(&mut self, uid: u32, timestamp: f64, now: f64) {
        self.placeholders += 1;
        let mut key = b"\xffplaceholder".to_vec();
        key.extend_from_slice(&self.placeholders.to_be_bytes());
        self.add_inner(uid, timestamp, &key, now, true);
    }

    fn add_inner(&mut self, uid: u32, timestamp: f64, message: &[u8], now: f64, dummy: bool) {
        if !self.pending.contains_key(message) {
            let open_new = match self.cohorts.back() {
                None => true,
                Some(c) => now - c.created > 0.05 || c.messages.len() > 25,
            };
            if open_new {
                self.cohorts.push_back(Cohort { created: now, messages: Vec::new() });
            }
            self.cohorts.back_mut().unwrap().messages.push(message.to_vec());
            self.pending.insert(
                message.to_vec(),
                Group { dummy, message: message.to_vec(), first_seen: now, copies: Vec::new(), receivers: Vec::new() },
            );
        }

        let group = self.pending.get_mut(message).unwrap();
        // remember ALL receivers that heard it (results are sent back to them)
        if !group.receivers.contains(&uid) {
            group.receivers.push(uid);
        }
        // limit the group size: the rest of the copies are discarded
        if group.copies.len() > MAX_GROUP {
            return;
        }
        group.copies.push((uid, timestamp, now));
    }

    /// The time at which the next cohort is due, if any.
    pub fn next_due(&self) -> Option<f64> {
        self.cohorts.front().map(|c| c.created + MLAT_DELAY)
    }

    /// Groups of every cohort that is due by `now`, in order, with the time
    /// their cohort's timer fires (that is the time they are resolved at).
    pub fn take_due(&mut self, now: f64) -> Vec<(f64, Group)> {
        let mut out = Vec::new();
        while self.cohorts.front().is_some_and(|c| c.created + MLAT_DELAY <= now) {
            let cohort = self.cohorts.pop_front().unwrap();
            let due = cohort.created + MLAT_DELAY;
            for m in cohort.messages {
                if let Some(g) = self.pending.remove(&m) {
                    out.push((due, g));
                }
            }
        }
        out
    }

    /// The oldest cohort, with the time its timer fires, regardless of the
    /// current time (the caller has checked it is due).
    pub fn pop_cohort(&mut self) -> Option<(f64, Vec<Group>)> {
        let cohort = self.cohorts.pop_front()?;
        let due = cohort.created + MLAT_DELAY;
        let groups = cohort.messages.into_iter().filter_map(|m| self.pending.remove(&m)).collect();
        Some((due, groups))
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_copies_and_releases_after_the_delay() {
        let mut c = Collector::new();
        c.add(1, 10.0, b"abc", 100.000);
        c.add(2, 10.1, b"abc", 100.010);
        c.add(3, 10.2, b"abc", 100.020);
        assert!(c.take_due(100.5).is_empty());
        assert_eq!(c.next_due(), Some(100.9));
        let g = c.take_due(100.9);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].1.copies.len(), 3);
        assert_eq!(g[0].1.receivers, vec![1, 2, 3]);
        assert_eq!(g[0].1.first_seen, 100.0);
        assert_eq!(g[0].0, 100.9);
        assert_eq!(c.pending(), 0);
    }

    #[test]
    fn cohorts_open_after_50ms_and_after_26_groups() {
        let mut c = Collector::new();
        c.add(1, 0.0, b"a", 10.00);
        c.add(1, 0.0, b"b", 10.04); // same cohort as "a"
        c.add(1, 0.0, b"c", 10.06); // more than 50 ms after the cohort opened: new cohort
        assert_eq!(c.take_due(10.9).len(), 2);
        assert_eq!(c.take_due(10.96).len(), 1);

        let mut c = Collector::new();
        for i in 0..30u8 {
            c.add(1, 0.0, &[i], 20.0 + i as f64 * 1e-4);
        }
        // 26 groups fit in the first cohort; the 27th opens another
        assert_eq!(c.take_due(20.9 + 0.001).len(), 26);
        assert_eq!(c.take_due(21.0).len(), 4);
    }

    #[test]
    fn copies_beyond_the_limit_only_add_receivers() {
        let mut c = Collector::new();
        for uid in 0..20u32 {
            c.add(uid, 0.0, b"x", 5.0);
        }
        let g = c.take_due(6.0).pop().unwrap().1;
        assert_eq!(g.copies.len(), MAX_GROUP + 1);
        assert_eq!(g.receivers.len(), 20);
    }
}
