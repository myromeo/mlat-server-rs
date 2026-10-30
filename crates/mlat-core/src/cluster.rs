//! Groups normalised timestamps into clusters that are probably copies of the
//! same transmission, ported from `_cluster_timestamps` in mlattrack.py.

use crate::normalize::Normalised;
use crate::C_AIR;

#[derive(Clone, Copy, Debug)]
pub struct Member {
    pub uid: u32,
    pub timestamp: f64,
    pub variance: f64,
}

#[derive(Clone, Debug)]
pub struct Cluster {
    /// Number of distinct receivers (receivers within 1 km count once).
    pub distinct: usize,
    /// First UTC time any member was seen.
    pub first_seen: f64,
    /// Members in ascending timestamp order.
    pub members: Vec<Member>,
}

/// `distance(a, b)` is the distance between two receivers in metres.
pub fn cluster_timestamps(component: &[Normalised], min_receivers: usize, distance: &dyn Fn(u32, u32) -> f64) -> Vec<Cluster> {
    // (uid, timestamp, variance, utc), sorted by timestamp (stable)
    let mut flat: Vec<(u32, f64, f64, f64)> = Vec::new();
    for (uid, variance, stamps) in component {
        for &(ts, utc) in stamps {
            flat.push((*uid, ts, *variance, utc));
        }
    }
    if flat.is_empty() {
        return Vec::new();
    }
    flat.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

    // rough grouping: neighbours less than 2 ms apart
    let mut groups: Vec<Vec<(u32, f64, f64, f64)>> = vec![vec![flat[0]]];
    for t in &flat[1..] {
        let last = groups.last_mut().unwrap();
        if t.1 - last.last().unwrap().1 > 2e-3 {
            groups.push(vec![*t]);
        } else {
            last.push(*t);
        }
    }

    let mut clusters = Vec::new();
    for mut group in groups {
        while group.len() >= min_receivers {
            let (uid, timestamp, variance, utc) = group.pop().unwrap();
            let mut cluster = vec![Member { uid, timestamp, variance }];
            let last_timestamp = timestamp;
            let mut distinct = 1usize;
            let mut first_seen = utc;

            let mut i = group.len();
            while i > 0 {
                i -= 1;
                let (r_uid, ts, var, r_utc) = group[i];
                if last_timestamp - ts > 2e-3 {
                    break;
                }

                let mut is_distinct = true;
                let mut can_cluster = true;
                for other in &cluster {
                    if other.uid == r_uid {
                        can_cluster = false;
                        break;
                    }
                    let d = distance(r_uid, other.uid);
                    if (other.timestamp - ts).abs() > (d * 1.05 + 1e3) / C_AIR {
                        can_cluster = false;
                        break;
                    }
                    if d < 1e3 {
                        is_distinct = false;
                    }
                }

                if can_cluster {
                    cluster.push(Member { uid: r_uid, timestamp: ts, variance: var });
                    first_seen = first_seen.min(r_utc);
                    if is_distinct {
                        distinct += 1;
                    }
                    group.remove(i);
                }
            }

            if distinct >= min_receivers {
                cluster.reverse();
                clusters.push(Cluster { distinct, first_seen, members: cluster });
            }
        }
    }
    clusters
}
