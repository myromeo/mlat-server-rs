//! Levenberg-Marquardt least squares with a forward-difference Jacobian: a port
//! of MINPACK's `lmdif` (with `fdjac2`, `qrfac`, `qrsolv`, `lmpar`, `enorm`),
//! which is what `scipy.optimize.leastsq` runs. Matrices are column-major with
//! the same leading dimensions as the Fortran original so that every floating
//! point operation happens in the original order.

const EPSMCH: f64 = f64::EPSILON; // dpmpar(1)
const DWARF: f64 = f64::MIN_POSITIVE; // dpmpar(2)

const P1: f64 = 0.1;
const P5: f64 = 0.5;
const P25: f64 = 0.25;
const P75: f64 = 0.75;
const P0001: f64 = 1e-4;

/// Euclidean norm, with MINPACK's overflow/underflow-safe accumulation.
pub fn enorm(x: &[f64]) -> f64 {
    const RDWARF: f64 = 3.834e-20;
    const RGIANT: f64 = 1.304e19;
    let n = x.len();
    let (mut s1, mut s2, mut s3) = (0.0_f64, 0.0_f64, 0.0_f64);
    let (mut x1max, mut x3max) = (0.0_f64, 0.0_f64);
    let agiant = RGIANT / n as f64;
    for &v in x {
        let xabs = v.abs();
        if xabs > RDWARF && xabs < agiant {
            s2 += xabs * xabs;
        } else if xabs <= RDWARF {
            if xabs <= x3max {
                if xabs != 0.0 {
                    s3 += (xabs / x3max) * (xabs / x3max);
                }
            } else {
                s3 = 1.0 + s3 * (x3max / xabs) * (x3max / xabs);
                x3max = xabs;
            }
        } else if xabs <= x1max {
            s1 += (xabs / x1max) * (xabs / x1max);
        } else {
            s1 = 1.0 + s1 * (x1max / xabs) * (x1max / xabs);
            x1max = xabs;
        }
    }
    if s1 != 0.0 {
        x1max * (s1 + (s2 / x1max) / x1max).sqrt()
    } else if s2 != 0.0 {
        if s2 >= x3max {
            (s2 * (1.0 + (x3max / s2) * (x3max * s3))).sqrt()
        } else {
            (x3max * ((s2 / x3max) + (x3max * s3))).sqrt()
        }
    } else {
        x3max * s3.sqrt()
    }
}

/// QR factorisation with column pivoting. On return the strict lower part of
/// `a` holds the Householder vectors, `rdiag` the diagonal of R, `acnorm` the
/// original column norms and `ipvt` the (0-based) permutation.
fn qrfac(m: usize, n: usize, a: &mut [f64], lda: usize, ipvt: &mut [usize], rdiag: &mut [f64], acnorm: &mut [f64], wa: &mut [f64]) {
    for j in 0..n {
        acnorm[j] = enorm(&a[j * lda..j * lda + m]);
        rdiag[j] = acnorm[j];
        wa[j] = rdiag[j];
        ipvt[j] = j;
    }

    let minmn = m.min(n);
    for j in 0..minmn {
        // bring the column of largest norm into the pivot position
        let mut kmax = j;
        for k in j..n {
            if rdiag[k] > rdiag[kmax] {
                kmax = k;
            }
        }
        if kmax != j {
            for i in 0..m {
                a.swap(i + j * lda, i + kmax * lda);
            }
            rdiag[kmax] = rdiag[j];
            wa[kmax] = wa[j];
            ipvt.swap(j, kmax);
        }

        // Householder transformation for column j
        let mut ajnorm = enorm(&a[j + j * lda..m + j * lda]);
        if ajnorm != 0.0 {
            if a[j + j * lda] < 0.0 {
                ajnorm = -ajnorm;
            }
            for i in j..m {
                a[i + j * lda] /= ajnorm;
            }
            a[j + j * lda] += 1.0;

            // apply it to the remaining columns and update the norms
            for k in j + 1..n {
                let mut sum = 0.0;
                for i in j..m {
                    sum += a[i + j * lda] * a[i + k * lda];
                }
                let temp = sum / a[j + j * lda];
                for i in j..m {
                    a[i + k * lda] -= temp * a[i + j * lda];
                }
                if rdiag[k] != 0.0 {
                    let temp = a[j + k * lda] / rdiag[k];
                    rdiag[k] *= (1.0 - temp * temp).max(0.0).sqrt();
                    let temp = rdiag[k] / wa[k];
                    if 0.05 * temp * temp <= EPSMCH {
                        rdiag[k] = enorm(&a[j + 1 + k * lda..m + k * lda]);
                        wa[k] = rdiag[k];
                    }
                }
            }
        }
        rdiag[j] = -ajnorm;
    }
}

/// Solves `[R; D] x = [Q^T b; 0]` in the least-squares sense for a diagonal D.
#[allow(clippy::too_many_arguments)]
fn qrsolv(n: usize, r: &mut [f64], ldr: usize, ipvt: &[usize], diag: &[f64], qtb: &[f64], x: &mut [f64], sdiag: &mut [f64], wa: &mut [f64]) {
    for j in 0..n {
        for i in j..n {
            r[i + j * ldr] = r[j + i * ldr];
        }
        x[j] = r[j + j * ldr];
        wa[j] = qtb[j];
    }

    // eliminate the diagonal matrix D using Givens rotations
    for j in 0..n {
        let l = ipvt[j];
        if diag[l] != 0.0 {
            for k in j..n {
                sdiag[k] = 0.0;
            }
            sdiag[j] = diag[l];

            let mut qtbpj = 0.0;
            for k in j..n {
                if sdiag[k] != 0.0 {
                    let (sin, cos);
                    if r[k + k * ldr].abs() < sdiag[k].abs() {
                        let cotan = r[k + k * ldr] / sdiag[k];
                        sin = P5 / (P25 + P25 * cotan * cotan).sqrt();
                        cos = sin * cotan;
                    } else {
                        let tan = sdiag[k] / r[k + k * ldr];
                        cos = P5 / (P25 + P25 * tan * tan).sqrt();
                        sin = cos * tan;
                    }

                    r[k + k * ldr] = cos * r[k + k * ldr] + sin * sdiag[k];
                    let temp = cos * wa[k] + sin * qtbpj;
                    qtbpj = -sin * wa[k] + cos * qtbpj;
                    wa[k] = temp;

                    for i in k + 1..n {
                        let temp = cos * r[i + k * ldr] + sin * sdiag[i];
                        sdiag[i] = -sin * r[i + k * ldr] + cos * sdiag[i];
                        r[i + k * ldr] = temp;
                    }
                }
            }
        }
        sdiag[j] = r[j + j * ldr];
        r[j + j * ldr] = x[j];
    }

    // solve the triangular system for z (least squares if singular)
    let mut nsing = n;
    for j in 0..n {
        if sdiag[j] == 0.0 && nsing == n {
            nsing = j;
        }
        if nsing < n {
            wa[j] = 0.0;
        }
    }
    for k in 1..=nsing {
        let j = nsing - k;
        let mut sum = 0.0;
        for i in j + 1..nsing {
            sum += r[i + j * ldr] * wa[i];
        }
        wa[j] = (wa[j] - sum) / sdiag[j];
    }

    for j in 0..n {
        x[ipvt[j]] = wa[j];
    }
}

/// Determines the Levenberg-Marquardt parameter.
#[allow(clippy::too_many_arguments)]
fn lmpar(n: usize, r: &mut [f64], ldr: usize, ipvt: &[usize], diag: &[f64], qtb: &[f64], delta: f64, par: &mut f64, x: &mut [f64], sdiag: &mut [f64], wa1: &mut [f64], wa2: &mut [f64]) {
    // Gauss-Newton direction
    let mut nsing = n;
    for j in 0..n {
        wa1[j] = qtb[j];
        if r[j + j * ldr] == 0.0 && nsing == n {
            nsing = j;
        }
        if nsing < n {
            wa1[j] = 0.0;
        }
    }
    for k in 1..=nsing {
        let j = nsing - k;
        wa1[j] /= r[j + j * ldr];
        let temp = wa1[j];
        for i in 0..j {
            wa1[i] -= r[i + j * ldr] * temp;
        }
    }
    for j in 0..n {
        x[ipvt[j]] = wa1[j];
    }

    let mut iter = 0;
    for j in 0..n {
        wa2[j] = diag[j] * x[j];
    }
    let mut dxnorm = enorm(&wa2[..n]);
    let mut fp = dxnorm - delta;
    if fp <= P1 * delta {
        *par = 0.0;
        return;
    }

    // lower bound from the Newton step, if the Jacobian is not rank deficient
    let mut parl = 0.0;
    if nsing >= n {
        for j in 0..n {
            let l = ipvt[j];
            wa1[j] = diag[l] * (wa2[l] / dxnorm);
        }
        for j in 0..n {
            let mut sum = 0.0;
            for i in 0..j {
                sum += r[i + j * ldr] * wa1[i];
            }
            wa1[j] = (wa1[j] - sum) / r[j + j * ldr];
        }
        let temp = enorm(&wa1[..n]);
        parl = ((fp / delta) / temp) / temp;
    }

    // upper bound
    for j in 0..n {
        let mut sum = 0.0;
        for i in 0..=j {
            sum += r[i + j * ldr] * qtb[i];
        }
        let l = ipvt[j];
        wa1[j] = sum / diag[l];
    }
    let gnorm = enorm(&wa1[..n]);
    let mut paru = gnorm / delta;
    if paru == 0.0 {
        paru = DWARF / delta.min(P1);
    }

    *par = par.max(parl);
    *par = par.min(paru);
    if *par == 0.0 {
        *par = gnorm / dxnorm;
    }

    loop {
        iter += 1;
        if *par == 0.0 {
            *par = DWARF.max(0.001 * paru);
        }
        let temp = par.sqrt();
        for j in 0..n {
            wa1[j] = temp * diag[j];
        }
        qrsolv(n, r, ldr, ipvt, wa1, qtb, x, sdiag, wa2);
        for j in 0..n {
            wa2[j] = diag[j] * x[j];
        }
        dxnorm = enorm(&wa2[..n]);
        let temp = fp;
        fp = dxnorm - delta;

        if fp.abs() <= P1 * delta || (parl == 0.0 && fp <= temp && temp < 0.0) || iter == 10 {
            break;
        }

        // Newton correction
        for j in 0..n {
            let l = ipvt[j];
            wa1[j] = diag[l] * (wa2[l] / dxnorm);
        }
        for j in 0..n {
            wa1[j] /= sdiag[j];
            let temp = wa1[j];
            for i in j + 1..n {
                wa1[i] -= r[i + j * ldr] * temp;
            }
        }
        let temp = enorm(&wa1[..n]);
        let parc = ((fp / delta) / temp) / temp;

        if fp > 0.0 {
            parl = parl.max(*par);
        }
        if fp < 0.0 {
            paru = paru.min(*par);
        }
        *par = parl.max(*par + parc);
    }
    if iter == 0 {
        *par = 0.0;
    }
}

/// Result of [`lmdif`], mirroring what scipy's `leastsq(full_output=True)` exposes.
pub struct LmResult {
    pub x: Vec<f64>,
    pub info: i32,
    pub nfev: i32,
    /// Column-major `m x n`; the upper triangle holds R of the last Jacobian.
    pub fjac: Vec<f64>,
    pub m: usize,
    /// 0-based column permutation of the QR factorisation.
    pub ipvt: Vec<usize>,
    /// First n components of Q^T * fvec at the last Jacobian.
    pub qtf: Vec<f64>,
    /// Residuals at the returned x.
    pub fvec: Vec<f64>,
}

/// Minimises the sum of squares of `f` (which fills `m` residuals) from `x0`.
#[allow(clippy::too_many_arguments)]
pub fn lmdif<F: FnMut(&[f64], &mut [f64])>(
    mut f: F,
    m: usize,
    x0: &[f64],
    ftol: f64,
    xtol: f64,
    gtol: f64,
    maxfev: i32,
    epsfcn: f64,
    factor: f64,
) -> LmResult {
    let n = x0.len();
    let mut x = x0.to_vec();
    let mut fvec = vec![0.0; m];
    let mut fjac = vec![0.0; m * n];
    let mut ipvt = vec![0usize; n];
    let mut diag = vec![0.0; n];
    let mut qtf = vec![0.0; n];
    let (mut wa1, mut wa2, mut wa3) = (vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    let mut wa4 = vec![0.0; m];
    let mut info = 0;

    let result = |x: Vec<f64>, info, nfev, fjac, ipvt, qtf, fvec| LmResult { x, info, nfev, fjac, m, ipvt, qtf, fvec };

    if n == 0 || m < n || ftol < 0.0 || xtol < 0.0 || gtol < 0.0 || maxfev <= 0 || factor <= 0.0 {
        return result(x, 0, 0, fjac, ipvt, qtf, fvec);
    }

    f(&x, &mut fvec);
    let mut nfev = 1;
    let mut fnorm = enorm(&fvec);

    let mut par = 0.0;
    let mut iter = 1;
    let mut delta = 0.0;
    let mut xnorm = 0.0;

    'outer: loop {
        // Jacobian by forward differences (fdjac2)
        {
            let eps = epsfcn.max(EPSMCH).sqrt();
            for j in 0..n {
                let temp = x[j];
                let mut h = eps * temp.abs();
                if h == 0.0 {
                    h = eps;
                }
                x[j] = temp + h;
                f(&x, &mut wa4);
                x[j] = temp;
                for i in 0..m {
                    fjac[i + j * m] = (wa4[i] - fvec[i]) / h;
                }
            }
        }
        nfev += n as i32;

        // wa1 = rdiag, wa2 = acnorm, wa3 = work
        qrfac(m, n, &mut fjac, m, &mut ipvt, &mut wa1, &mut wa2, &mut wa3);

        if iter == 1 {
            for j in 0..n {
                diag[j] = wa2[j];
                if wa2[j] == 0.0 {
                    diag[j] = 1.0;
                }
            }
            for j in 0..n {
                wa3[j] = diag[j] * x[j];
            }
            xnorm = enorm(&wa3);
            delta = factor * xnorm;
            if delta == 0.0 {
                delta = factor;
            }
        }

        // form (Q^T) fvec and keep the first n components in qtf
        wa4.copy_from_slice(&fvec);
        for j in 0..n {
            if fjac[j + j * m] != 0.0 {
                let mut sum = 0.0;
                for i in j..m {
                    sum += fjac[i + j * m] * wa4[i];
                }
                let temp = -sum / fjac[j + j * m];
                for i in j..m {
                    wa4[i] += fjac[i + j * m] * temp;
                }
            }
            fjac[j + j * m] = wa1[j];
            qtf[j] = wa4[j];
        }

        // norm of the scaled gradient
        let mut gnorm = 0.0_f64;
        if fnorm != 0.0 {
            for j in 0..n {
                let l = ipvt[j];
                if wa2[l] != 0.0 {
                    let mut sum = 0.0;
                    for i in 0..=j {
                        sum += fjac[i + j * m] * (qtf[i] / fnorm);
                    }
                    gnorm = gnorm.max((sum / wa2[l]).abs());
                }
            }
        }

        if gnorm <= gtol {
            info = 4;
        }
        if info != 0 {
            break 'outer;
        }

        for j in 0..n {
            diag[j] = diag[j].max(wa2[j]);
        }

        // inner loop: repeat until a step is accepted
        loop {
            // direction -> wa1, sdiag -> wa2, scratch -> wa3, wa4
            lmpar(n, &mut fjac, m, &ipvt, &diag, &qtf, delta, &mut par, &mut wa1, &mut wa2, &mut wa3, &mut wa4[..n]);
            for j in 0..n {
                wa1[j] = -wa1[j];
                wa2[j] = x[j] + wa1[j];
                wa3[j] = diag[j] * wa1[j];
            }
            let pnorm = enorm(&wa3);

            if iter == 1 {
                delta = delta.min(pnorm);
            }

            f(&wa2, &mut wa4);
            nfev += 1;
            let fnorm1 = enorm(&wa4);

            let mut actred = -1.0;
            if P1 * fnorm1 < fnorm {
                let t = fnorm1 / fnorm;
                actred = 1.0 - t * t;
            }

            for j in 0..n {
                wa3[j] = 0.0;
                let l = ipvt[j];
                let temp = wa1[l];
                for i in 0..=j {
                    wa3[i] += fjac[i + j * m] * temp;
                }
            }
            let temp1 = enorm(&wa3) / fnorm;
            let temp2 = (par.sqrt() * pnorm) / fnorm;
            let prered = temp1 * temp1 + temp2 * temp2 / P5;
            let dirder = -(temp1 * temp1 + temp2 * temp2);

            let ratio = if prered != 0.0 { actred / prered } else { 0.0 };

            if ratio <= P25 {
                let mut temp = if actred >= 0.0 { P5 } else { P5 * dirder / (dirder + P5 * actred) };
                if P1 * fnorm1 >= fnorm || temp < P1 {
                    temp = P1;
                }
                delta = temp * delta.min(pnorm / P1);
                par /= temp;
            } else if par == 0.0 || ratio >= P75 {
                delta = pnorm / P5;
                par *= P5;
            }

            if ratio >= P0001 {
                for j in 0..n {
                    x[j] = wa2[j];
                    wa2[j] = diag[j] * x[j];
                }
                fvec.copy_from_slice(&wa4);
                xnorm = enorm(&wa2);
                fnorm = fnorm1;
                iter += 1;
            }

            if actred.abs() <= ftol && prered <= ftol && P5 * ratio <= 1.0 {
                info = 1;
            }
            if delta <= xtol * xnorm {
                info = 2;
            }
            if actred.abs() <= ftol && prered <= ftol && P5 * ratio <= 1.0 && info == 2 {
                info = 3;
            }
            if info != 0 {
                break 'outer;
            }

            if nfev >= maxfev {
                info = 5;
            }
            if actred.abs() <= EPSMCH && prered <= EPSMCH && P5 * ratio <= 1.0 {
                info = 6;
            }
            if delta <= EPSMCH * xnorm {
                info = 7;
            }
            if gnorm <= EPSMCH {
                info = 8;
            }
            if info != 0 {
                break 'outer;
            }

            if ratio >= P0001 {
                break;
            }
        }
    }

    result(x, info, nfev, fjac, ipvt, qtf, fvec)
}
