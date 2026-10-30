//! Clock normalisation: converts each receiver's timestamps for one message to
//! a common timebase using the clock pairings, ported from `normalize2` in
//! clocktrack.pyx. Receivers are linked by the minimum spanning tree of the
//! usable pairings (by variance); a central receiver of the biggest connected
//! component becomes the reference and every timestamp is converted along the
//! tree towards it.

/// What the normaliser needs to know about one usable clock pairing (the pair
/// is keyed by receiver uids, `lo < hi`).
#[derive(Clone, Copy, Debug)]
pub struct PairParams {
    pub valid: bool,
    pub variance: f64,
    pub updated: f64,
    pub n: i32,
    pub factor: f64,
    pub i_factor: f64,
    pub base_avg: f64,
    pub peer_avg: f64,
}

/// One receiver's timestamps for the message: (receiver clock reading, utc).
pub struct ReceiverStamps {
    pub uid: u32,
    pub freq: f64,
    pub jitter: f64,
    pub stamps: Vec<(f64, f64)>,
}

/// Normalised timestamps of one receiver: (variance, [(timestamp, utc)]).
pub type Normalised = (u32, f64, Vec<(f64, f64)>);

#[derive(Clone, Copy)]
struct Predictor {
    target_ref: f64,
    source_ref: f64,
    factor: f64,
    variance: f64,
}

/// `_make_predictors`: (predicts hi from lo, predicts lo from hi), or None
/// when the pairing is unusable.
fn make_predictors(p: &PairParams, now: f64) -> Option<(Predictor, Predictor)> {
    if !p.valid {
        return None;
    }
    let mut variance = p.variance;
    let stale = now - p.updated;
    variance *= 1.0 + stale / 60.0;
    if p.n < 10 {
        variance *= 1.0 + (10 - p.n) as f64 * 0.05;
    }
    let base = Predictor { target_ref: p.base_avg, source_ref: p.peer_avg, factor: p.i_factor, variance };
    let peer = Predictor { target_ref: p.peer_avg, source_ref: p.base_avg, factor: p.factor, variance };
    // station0 < station1: (peer_predictor, base_predictor)
    Some((peer, base))
}

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u32>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind { parent: (0..n).collect(), rank: vec![0; n] }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) -> bool {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return false;
        }
        if self.rank[ra] < self.rank[rb] {
            self.parent[ra] = rb;
        } else {
            self.parent[rb] = ra;
            if self.rank[ra] == self.rank[rb] {
                self.rank[ra] += 1;
            }
        }
        true
    }
}

/// Tree over the receivers of the biggest component, with neighbour lists in
/// edge insertion order (which the traversal below depends on).
struct Tree {
    neighbors: Vec<Vec<usize>>,
    weight: std::collections::HashMap<(usize, usize), f64>,
}

impl Tree {
    fn edge_weight(&self, a: usize, b: usize) -> f64 {
        let k = if a < b { (a, b) } else { (b, a) };
        self.weight[&k]
    }
}

fn label_heights(t: &Tree, node: usize, heights: &mut Vec<Option<f64>>) {
    heights[node] = Some(0.0);
    for &each in &t.neighbors[node] {
        if heights[each].is_none() {
            label_heights(t, each, heights);
            let mn = heights[each].unwrap() + t.edge_weight(node, each);
            if mn > heights[node].unwrap() {
                heights[node] = Some(mn);
            }
        }
    }
}

/// Returns (path length, node) of the tallest branch from `node`.
fn tallest_branch(t: &Tree, node: usize, heights: &[Option<f64>], ignore: Option<usize>) -> (f64, Option<usize>) {
    let mut tallest = (0.0, None);
    for &each in &t.neighbors[node] {
        if Some(each) == ignore {
            continue;
        }
        let eh = heights[each].unwrap() + t.edge_weight(node, each);
        if eh > tallest.0 {
            tallest = (eh, Some(each));
        }
    }
    tallest
}

#[allow(clippy::too_many_arguments)]
fn convert(
    t: &Tree,
    recv: &[ReceiverStamps],
    uids: &[u32],
    preds: &std::collections::HashMap<(u32, u32), Predictor>,
    node: usize,
    visited: &mut Vec<bool>,
    results: &mut Vec<Normalised>,
    chain: &[Predictor],
    variance: f64,
) {
    visited[node] = true;
    let mut r = Vec::with_capacity(recv[node].stamps.len());
    for &(ts0, utc) in &recv[node].stamps {
        let mut ts = ts0;
        for p in chain {
            ts = p.target_ref + (ts - p.source_ref) * p.factor;
        }
        r.push((ts, utc));
    }
    results.push((uids[node], variance, r));

    for &neighbor in &t.neighbors[node] {
        if !visited[neighbor] {
            let p = preds[&(uids[neighbor], uids[node])];
            let mut c = Vec::with_capacity(chain.len() + 1);
            c.push(p);
            c.extend_from_slice(chain);
            convert(t, recv, uids, preds, neighbor, visited, results, &c, variance + p.variance);
        }
    }
}

/// `normalize2`. `receivers` is in the caller's order; `pair` looks up the
/// clock pairing for a (lo, hi) uid pair. Returns at most one component: the
/// biggest, in traversal order.
pub fn normalize2<F: Fn(u32, u32) -> Option<PairParams>>(receivers: &[ReceiverStamps], pair: F, now: f64) -> Vec<Normalised> {
    // sort by uid, as `receivers.sort()` does
    let mut order: Vec<usize> = (0..receivers.len()).collect();
    order.sort_by_key(|&i| receivers[i].uid);
    let recv: Vec<&ReceiverStamps> = order.iter().map(|&i| &receivers[i]).collect();
    let n = recv.len();
    let uids: Vec<u32> = recv.iter().map(|r| r.uid).collect();

    // candidate edges, row-major over (si < sj)
    let mut edges: Vec<(usize, usize, f64)> = Vec::new();
    let mut preds = std::collections::HashMap::new();
    for ri in 0..n {
        for ci in ri + 1..n {
            if let Some(p) = pair(uids[ri], uids[ci]) {
                if let Some((lo_to_hi, hi_to_lo)) = make_predictors(&p, now) {
                    preds.insert((uids[ri], uids[ci]), lo_to_hi);
                    preds.insert((uids[ci], uids[ri]), hi_to_lo);
                    edges.push((ri, ci, lo_to_hi.variance));
                }
            }
        }
    }
    if edges.len() < 2 {
        return Vec::new();
    }

    // minimum spanning tree (Kruskal, stable order for ties), keeping the
    // surviving edges in their original row-major order. As in scipy, an edge
    // of weight exactly zero still joins its endpoints while the tree is built
    // but is then erased from the result, which can strand a receiver.
    let mut sorted: Vec<usize> = (0..edges.len()).collect();
    sorted.sort_by(|&a, &b| edges[a].2.partial_cmp(&edges[b].2).unwrap());
    let mut uf = UnionFind::new(n);
    let mut keep = vec![false; edges.len()];
    for &e in &sorted {
        if uf.union(edges[e].0, edges[e].1) {
            keep[e] = true;
        }
    }
    let tree_edges: Vec<(usize, usize, f64)> =
        edges.iter().zip(&keep).filter(|(e, &k)| k && e.2 != 0.0).map(|(e, _)| *e).collect();

    // connected components of the tree, labelled in order of first node
    let mut uf2 = UnionFind::new(n);
    for &(a, b, _) in &tree_edges {
        uf2.union(a, b);
    }
    let mut label_of_root = std::collections::HashMap::new();
    let mut labels = vec![0usize; n];
    for i in 0..n {
        let root = uf2.find(i);
        let next = label_of_root.len();
        labels[i] = *label_of_root.entry(root).or_insert(next);
    }
    let n_comps = label_of_root.len();
    let mut sizes = vec![0usize; n_comps];
    let mut first_node = vec![usize::MAX; n_comps];
    for i in 0..n {
        sizes[labels[i]] += 1;
        if first_node[labels[i]] == usize::MAX {
            first_node[labels[i]] = i;
        }
    }
    // biggest component; ties go to the lower label (stable sort)
    let mut big = 0;
    for l in 1..n_comps {
        if sizes[l] > sizes[big] {
            big = l;
        }
    }
    if sizes[big] < 3 {
        return Vec::new();
    }
    let root = first_node[big];

    // the tree of that component, neighbour lists in edge order
    let mut tree = Tree { neighbors: vec![Vec::new(); n], weight: std::collections::HashMap::new() };
    for &(a, b, w) in &tree_edges {
        if labels[a] == big {
            tree.neighbors[a].push(b);
            tree.neighbors[b].push(a);
            tree.weight.insert((a.min(b), a.max(b)), w);
        }
    }

    // pick the centre of the tree's longest path through the root
    let mut heights: Vec<Option<f64>> = vec![None; n];
    label_heights(&tree, root, &mut heights);
    let tall1 = tallest_branch(&tree, root, &heights, None);
    let tall2 = tallest_branch(&tree, root, &heights, tall1.1);
    let target = (tall1.0 + tall2.0) / 2.0;
    let mut central = root;
    let mut step = tall1.1;
    while let Some(s) = step {
        if (heights[central].unwrap() - target).abs() > (heights[s].unwrap() - target).abs() {
            central = s;
            // (the Python passes `ignore=central`, which never matches a neighbour)
            step = tallest_branch(&tree, central, &heights, Some(central)).1;
        } else {
            break;
        }
    }

    let jitter2 = recv[central].jitter * recv[central].jitter;
    let first = Predictor { target_ref: 0.0, source_ref: 0.0, factor: 1.0 / recv[central].freq, variance: jitter2 };
    let mut results = Vec::new();
    let mut visited = vec![false; n];
    let owned: Vec<ReceiverStamps> = recv
        .iter()
        .map(|r| ReceiverStamps { uid: r.uid, freq: r.freq, jitter: r.jitter, stamps: r.stamps.clone() })
        .collect();
    convert(&tree, &owned, &uids, &preds, central, &mut visited, &mut results, &[first], jitter2);
    results
}
