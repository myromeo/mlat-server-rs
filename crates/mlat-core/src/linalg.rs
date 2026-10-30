//! Small dense linear algebra for the Kalman filter: row-major matrices with
//! the handful of operations pykalman's unscented filter uses.

#[derive(Clone, Debug, PartialEq)]
pub struct Mat {
    pub r: usize,
    pub c: usize,
    pub d: Vec<f64>,
}

impl Mat {
    pub fn zeros(r: usize, c: usize) -> Mat {
        Mat { r, c, d: vec![0.0; r * c] }
    }

    pub fn from_rows(rows: &[Vec<f64>]) -> Mat {
        let r = rows.len();
        let c = rows.first().map_or(0, |x| x.len());
        Mat { r, c, d: rows.iter().flatten().copied().collect() }
    }

    #[inline]
    pub fn at(&self, i: usize, j: usize) -> f64 {
        self.d[i * self.c + j]
    }

    #[inline]
    pub fn set(&mut self, i: usize, j: usize, v: f64) {
        self.d[i * self.c + j] = v;
    }

    pub fn t(&self) -> Mat {
        let mut o = Mat::zeros(self.c, self.r);
        for i in 0..self.r {
            for j in 0..self.c {
                o.d[j * self.r + i] = self.d[i * self.c + j];
            }
        }
        o
    }

    pub fn matmul(&self, o: &Mat) -> Mat {
        assert_eq!(self.c, o.r);
        let mut out = Mat::zeros(self.r, o.c);
        for i in 0..self.r {
            for k in 0..self.c {
                let a = self.d[i * self.c + k];
                for j in 0..o.c {
                    out.d[i * o.c + j] += a * o.d[k * o.c + j];
                }
            }
        }
        out
    }

    pub fn matvec(&self, v: &[f64]) -> Vec<f64> {
        (0..self.r).map(|i| (0..self.c).map(|k| self.at(i, k) * v[k]).sum()).collect()
    }

    pub fn add(&self, o: &Mat) -> Mat {
        Mat { r: self.r, c: self.c, d: self.d.iter().zip(&o.d).map(|(a, b)| a + b).collect() }
    }

    pub fn sub(&self, o: &Mat) -> Mat {
        Mat { r: self.r, c: self.c, d: self.d.iter().zip(&o.d).map(|(a, b)| a - b).collect() }
    }

    pub fn is_finite(&self) -> bool {
        self.d.iter().all(|v| v.is_finite())
    }
}

/// Lower-triangular Cholesky factor L with `a = L L^T`; None if `a` is not
/// positive definite.
pub fn cholesky_lower(a: &Mat) -> Option<Mat> {
    let n = a.r;
    let mut l = Mat::zeros(n, n);
    for j in 0..n {
        let mut s = a.at(j, j);
        for k in 0..j {
            s -= l.at(j, k) * l.at(j, k);
        }
        if s <= 0.0 || s.is_nan() {
            return None;
        }
        let d = s.sqrt();
        l.set(j, j, d);
        for i in j + 1..n {
            let mut s = a.at(i, j);
            for k in 0..j {
                s -= l.at(i, k) * l.at(j, k);
            }
            l.set(i, j, s / d);
        }
    }
    Some(l)
}

/// Inverse by LU decomposition with partial pivoting; None if singular.
pub fn inverse(a: &Mat) -> Option<Mat> {
    let n = a.r;
    let mut lu = a.clone();
    let mut piv: Vec<usize> = (0..n).collect();
    for k in 0..n {
        let mut p = k;
        for i in k + 1..n {
            if lu.at(i, k).abs() > lu.at(p, k).abs() {
                p = i;
            }
        }
        if lu.at(p, k) == 0.0 {
            return None;
        }
        if p != k {
            for j in 0..n {
                let (x, y) = (lu.at(k, j), lu.at(p, j));
                lu.set(k, j, y);
                lu.set(p, j, x);
            }
            piv.swap(k, p);
        }
        for i in k + 1..n {
            let f = lu.at(i, k) / lu.at(k, k);
            lu.set(i, k, f);
            for j in k + 1..n {
                let v = lu.at(i, j) - f * lu.at(k, j);
                lu.set(i, j, v);
            }
        }
    }
    let mut inv = Mat::zeros(n, n);
    for col in 0..n {
        // solve L U x = P e_col
        let mut x = vec![0.0; n];
        for i in 0..n {
            let mut s = if piv[i] == col { 1.0 } else { 0.0 };
            for j in 0..i {
                s -= lu.at(i, j) * x[j];
            }
            x[i] = s;
        }
        for i in (0..n).rev() {
            let mut s = x[i];
            for j in i + 1..n {
                s -= lu.at(i, j) * x[j];
            }
            x[i] = s / lu.at(i, i);
        }
        for i in 0..n {
            inv.set(i, col, x[i]);
        }
    }
    Some(inv)
}

/// Eigen-decomposition of a symmetric matrix by cyclic Jacobi rotations.
/// Returns (eigenvalues, eigenvectors as columns).
fn symmetric_eigen(a: &Mat) -> (Vec<f64>, Mat) {
    let n = a.r;
    let mut m = a.clone();
    let mut v = Mat::zeros(n, n);
    for i in 0..n {
        v.set(i, i, 1.0);
    }
    for _sweep in 0..60 {
        let mut off = 0.0;
        for i in 0..n {
            for j in i + 1..n {
                off += m.at(i, j) * m.at(i, j);
            }
        }
        if off < 1e-300 {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                let apq = m.at(p, q);
                if apq == 0.0 {
                    continue;
                }
                let theta = (m.at(q, q) - m.at(p, p)) / (2.0 * apq);
                let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t = if theta == 0.0 { 1.0 } else { t };
                let c = 1.0 / (t * t + 1.0).sqrt();
                let s = t * c;
                for k in 0..n {
                    let (mkp, mkq) = (m.at(k, p), m.at(k, q));
                    m.set(k, p, c * mkp - s * mkq);
                    m.set(k, q, s * mkp + c * mkq);
                }
                for k in 0..n {
                    let (mpk, mqk) = (m.at(p, k), m.at(q, k));
                    m.set(p, k, c * mpk - s * mqk);
                    m.set(q, k, s * mpk + c * mqk);
                }
                for k in 0..n {
                    let (vkp, vkq) = (v.at(k, p), v.at(k, q));
                    v.set(k, p, c * vkp - s * vkq);
                    v.set(k, q, s * vkp + c * vkq);
                }
            }
        }
    }
    ((0..n).map(|i| m.at(i, i)).collect(), v)
}

/// Moore-Penrose pseudo-inverse of a symmetric positive semi-definite matrix,
/// dropping singular values below `max(M, N) * eps * max(s)` as scipy does.
pub fn pinv_symmetric(a: &Mat) -> Mat {
    let n = a.r;
    let (w, v) = symmetric_eigen(a);
    let smax = w.iter().fold(0.0_f64, |m, x| m.max(x.abs()));
    let cutoff = n as f64 * f64::EPSILON * smax;
    let mut out = Mat::zeros(n, n);
    for k in 0..n {
        if w[k].abs() > cutoff {
            let inv = 1.0 / w[k];
            for i in 0..n {
                for j in 0..n {
                    out.d[i * n + j] += v.at(i, k) * inv * v.at(j, k);
                }
            }
        }
    }
    out
}
