//! The equilibrium of a zero-sum matrix game, by regret matching+.
//!
//! The matrix `a` (`m × n`, row-major) holds the value to the row player of
//! each pair of a row and a column; the column player loses what the row
//! player gains. Two imaginary players take turns: each keeps, per action,
//! how much better that action would have done than its mix so far (its
//! regret, floored at zero), and plays the next round in proportion to those
//! regrets. The average of their mixes, later rounds weighing more,
//! converges to the equilibrium (Tammelin 2014: alternating updates, linear
//! averaging). The **gap** reported is the sum of what each side could gain
//! by switching to its best single reply against the other's average mix:
//! the true value lies within half of it of the midpoint returned, and a
//! gap of zero is an exact equilibrium. No tolerance is needed to trust a
//! solution: the gap certifies it.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SolveOpts {
    /// Rounds at most.
    pub max_iter: usize,
    /// Stop once the gap is under this.
    pub tol: f64,
}

impl Default for SolveOpts {
    fn default() -> Self {
        SolveOpts { max_iter: 5000, tol: 1e-4 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Solution {
    /// The row player's mix.
    pub x: Vec<f64>,
    /// The column player's mix.
    pub y: Vec<f64>,
    /// The value to the row player: the midpoint of what each side's mix guarantees.
    pub value: f64,
    /// What the two sides could gain together by switching to their best single replies.
    pub gap: f64,
    pub iters: usize,
}

/// `a · y`: the value of each row against the column mix.
fn rows_against(a: &[f32], m: usize, n: usize, y: &[f64], out: &mut [f64]) {
    for (i, o) in out.iter_mut().enumerate().take(m) {
        let row = &a[i * n..(i + 1) * n];
        *o = row.iter().zip(y).map(|(&v, &p)| v as f64 * p).sum();
    }
}

/// `x · a`: the value of each column against the row mix.
fn cols_against(a: &[f32], m: usize, n: usize, x: &[f64], out: &mut [f64]) {
    out[..n].fill(0.0);
    for i in 0..m {
        let p = x[i];
        if p == 0.0 {
            continue;
        }
        for (o, &v) in out.iter_mut().zip(&a[i * n..(i + 1) * n]) {
            *o += v as f64 * p;
        }
    }
}

/// The row that does best against `y`, and its value (ties: the lower index).
pub fn best_row(a: &[f32], m: usize, n: usize, y: &[f64]) -> (usize, f64) {
    let mut u = vec![0.0; m];
    rows_against(a, m, n, y, &mut u);
    let mut best = 0;
    for i in 1..m {
        if u[i] > u[best] {
            best = i;
        }
    }
    (best, u[best])
}

/// The column that does best against `x` (lowest value to the row player), and that value.
pub fn best_col(a: &[f32], m: usize, n: usize, x: &[f64]) -> (usize, f64) {
    let mut v = vec![0.0; n];
    cols_against(a, m, n, x, &mut v);
    let mut best = 0;
    for j in 1..n {
        if v[j] < v[best] {
            best = j;
        }
    }
    (best, v[best])
}

/// What the two sides could gain together by switching to their best single replies.
pub fn exploitability(a: &[f32], m: usize, n: usize, x: &[f64], y: &[f64]) -> f64 {
    best_row(a, m, n, y).1 - best_col(a, m, n, x).1
}

fn normalize(regret: &[f64], out: &mut [f64]) {
    let total: f64 = regret.iter().sum();
    if total > 0.0 {
        for (o, r) in out.iter_mut().zip(regret) {
            *o = r / total;
        }
    } else {
        out.fill(1.0 / out.len() as f64);
    }
}

/// The equilibrium of the `m × n` matrix `a` (row-major, the value to the row player).
pub fn solve(a: &[f32], m: usize, n: usize, opts: SolveOpts) -> Solution {
    assert!(m >= 1 && n >= 1 && a.len() == m * n, "a non-empty m × n matrix");
    let (mut x, mut y) = (vec![1.0 / m as f64; m], vec![1.0 / n as f64; n]);
    let (mut rx, mut ry) = (vec![0.0; m], vec![0.0; n]);
    let (mut xs, mut ys) = (vec![0.0; m], vec![0.0; n]);
    let (mut u, mut v) = (vec![0.0; m], vec![0.0; n]);
    let (mut xbar, mut ybar) = (x.clone(), y.clone());
    let mut weight = 0.0;
    let mut iters = 0;
    let mut gap = f64::INFINITY;
    let mut value = 0.0;
    for t in 1..=opts.max_iter.max(1) {
        iters = t;
        // The row player answers the column mix.
        rows_against(a, m, n, &y, &mut u);
        let ux: f64 = u.iter().zip(&x).map(|(a, b)| a * b).sum();
        for i in 0..m {
            rx[i] = (rx[i] + u[i] - ux).max(0.0);
        }
        normalize(&rx, &mut x);
        // The column player answers the row mix just made (alternating).
        cols_against(a, m, n, &x, &mut v);
        let vy: f64 = v.iter().zip(&y).map(|(a, b)| a * b).sum();
        for j in 0..n {
            ry[j] = (ry[j] + vy - v[j]).max(0.0);
        }
        normalize(&ry, &mut y);
        // Linear averaging: round t weighs t.
        let w = t as f64;
        for i in 0..m {
            xs[i] += w * x[i];
        }
        for j in 0..n {
            ys[j] += w * y[j];
        }
        weight += w;
        for i in 0..m {
            xbar[i] = xs[i] / weight;
        }
        for j in 0..n {
            ybar[j] = ys[j] / weight;
        }
        let hi = best_row(a, m, n, &ybar).1;
        let lo = best_col(a, m, n, &xbar).1;
        gap = hi - lo;
        value = (hi + lo) / 2.0;
        if gap <= opts.tol {
            break;
        }
    }
    Solution { x: xbar, y: ybar, value, gap, iters }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    const OPTS: SolveOpts = SolveOpts { max_iter: 20_000, tol: 1e-6 };

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn matching_pennies_is_a_coin_flip() {
        let s = solve(&[1.0, -1.0, -1.0, 1.0], 2, 2, OPTS);
        assert!(close(s.value, 0.0, 1e-6), "{s:?}");
        assert!(close(s.x[0], 0.5, 1e-3) && close(s.y[0], 0.5, 1e-3), "{s:?}");
        assert!(s.gap <= OPTS.tol);
    }

    #[test]
    fn rock_paper_scissors_is_a_third_each() {
        let a = [0.0, -1.0, 1.0, 1.0, 0.0, -1.0, -1.0, 1.0, 0.0];
        let s = solve(&a, 3, 3, OPTS);
        assert!(close(s.value, 0.0, 1e-6));
        for p in s.x.iter().chain(&s.y) {
            assert!(close(*p, 1.0 / 3.0, 1e-3), "{s:?}");
        }
    }

    #[test]
    fn a_dominant_row_and_the_column_that_answers_it() {
        // Row 0 beats row 1 everywhere; against row 0 the column player prefers column 1.
        let s = solve(&[1.0, 0.5, 0.0, 0.2], 2, 2, OPTS);
        assert!(close(s.value, 0.5, 1e-6), "{s:?}");
        assert!(s.x[0] > 0.999 && s.y[1] > 0.999, "{s:?}");
    }

    #[test]
    fn one_row_or_one_column_is_a_min_or_a_max() {
        let s = solve(&[0.3, -0.2, 0.7], 1, 3, OPTS);
        assert!(close(s.value, -0.2, 1e-6) && s.y[1] > 0.999, "{s:?}");
        let s = solve(&[0.3, -0.2, 0.7], 3, 1, OPTS);
        assert!(close(s.value, 0.7, 1e-6) && s.x[2] > 0.999, "{s:?}");
        let s = solve(&[0.4], 1, 1, OPTS);
        assert!(close(s.value, 0.4, 1e-6) && s.gap == 0.0 && s.iters == 1, "{s:?}");
    }

    #[test]
    fn a_constant_matrix_and_duplicate_rows() {
        let s = solve(&[0.25; 12], 3, 4, OPTS);
        assert!(close(s.value, 0.25, 1e-9) && s.gap == 0.0, "{s:?}");
        let a = [1.0, -1.0, -1.0, 1.0];
        let twice = [1.0, -1.0, 1.0, -1.0, -1.0, 1.0];
        let s1 = solve(&a, 2, 2, OPTS);
        let s2 = solve(&twice, 3, 2, OPTS);
        // A duplicated row makes the equilibrium a continuum, which regret matching reaches slowly:
        // the gap still certifies the value.
        assert!(close(s1.value, s2.value, s1.gap + s2.gap), "{s1:?} {s2:?}");
        assert!(close(s2.x[0] + s2.x[1], 0.5, 1e-2), "the duplicated row shares its mass: {s2:?}");
    }

    #[test]
    fn random_matrices_are_certified_by_the_gap_and_repeat_exactly() {
        let mut rng = Rng::from_words([2026, 10, 13, 1]);
        let mut worst = 0usize;
        for case in 0..200 {
            let (m, n) = (1 + rng.below(25) as usize, 1 + rng.below(25) as usize);
            let a: Vec<f32> = (0..m * n).map(|_| rng.below(2001) as f32 / 1000.0 - 1.0).collect();
            let opts = SolveOpts { max_iter: 20_000, tol: 1e-4 };
            let s = solve(&a, m, n, opts);
            assert!(s.gap <= opts.tol, "case {case} ({m}×{n}): gap {} after {} rounds", s.gap, s.iters);
            assert!(close(exploitability(&a, m, n, &s.x, &s.y), s.gap, 1e-12));
            assert!(close(s.x.iter().sum::<f64>(), 1.0, 1e-9) && close(s.y.iter().sum::<f64>(), 1.0, 1e-9));
            assert_eq!(s, solve(&a, m, n, opts), "deterministic");
            worst = worst.max(s.iters);
        }
        eprintln!("random matrices: at most {worst} rounds to a gap of 1e-4");
    }
}

