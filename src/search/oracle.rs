//! The double oracle: a matrix of joint actions grown from each side's
//! likeliest action outwards, solved after every step.
//!
//! A [`Search`] holds a root [`Game`], the viewer whose value it computes,
//! and each side's list of joint actions in the order the caller thinks
//! likely (the **lists**; the first of each starts the matrix). It is driven
//! from outside: [`Search::wanted`] names the cells whose values it needs,
//! the driver simulates them ([`Search::simulate`]) and values the leaves
//! however it likes, [`Search::supply`] takes the values, and
//! [`Search::advance`] solves and looks for an improvement. The loop:
//!
//! 1. **Fill**: every cell of the matrix needs its `seeds` sets of dice.
//! 2. **Solve**: the matrix of mean cell values is solved ([`solve`]).
//! 3. **Scan**: for each side, the candidate actions outside the matrix
//!    (every one, or a window of the next few in its list not yet looked
//!    at) are valued against the other side's mix; the best candidate is
//!    added if it beats the current value by more than `eps`. If neither
//!    side can improve, a window slides on to the next actions not yet
//!    looked at (D69); once every action of a side has been looked at, its
//!    scans take every action outside the matrix, as with `All`. When no
//!    candidate improves and nothing is left unlooked at, the search has
//!    converged: the mix is an equilibrium of the whole game over the lists,
//!    to within `eps` and the solver's gap. Otherwise back to 1. No scan runs
//!    on a solve whose gap is above the solver's `tol` (D84): the search stops
//!    there, capped by the solver.
//!
//! Cell values are kept per set of dice, so a value may be replaced (a leaf
//! searched further) and a cell may be given more dice ([`Search::deepen`]).
//! `max_steps` and `time_cap` stop the loop early; with no steps at all the
//! matrix is each side's first `initial` actions solved once.

use std::time::{Duration, Instant};

use rayon::ThreadPool;

use crate::battle::Error;
use crate::env::{Baseline, Game};
use crate::obs::Phase;
use crate::rng::Rng;
use crate::search::cell::{CellId, Leaves, Mode, NO_PLANS, Plan, SimOpts, check_root, simulate_planned};
use crate::search::solve::{SolveOpts, Solution, solve};

/// Which actions outside the matrix an oracle step considers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Candidates {
    /// Every action in the list.
    All,
    /// The next so many in the list, in its order, that have not been looked at yet; every
    /// action outside the matrix once all have been.
    Next(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Budget {
    pub candidates: Candidates,
    /// Sets of dice per cell.
    pub seeds: usize,
    /// How many of each side's list the matrix starts with.
    pub initial: usize,
    /// Oracle steps at most.
    pub max_steps: usize,
    pub time_cap: Option<Duration>,
    /// A candidate is added only if it beats the current value by more than this.
    pub eps: f64,
    pub solver: SolveOpts,
    /// What the root is ([`Mode`]): a move request unless asked otherwise.
    pub mode: Mode,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            candidates: Candidates::All,
            seeds: 16,
            initial: 1,
            max_steps: usize::MAX,
            time_cap: None,
            eps: 1e-3,
            solver: SolveOpts::default(),
            mode: Mode::Move,
        }
    }
}

/// Why a search stopped short of convergence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cap {
    Steps,
    Time,
    /// The solver did not bring its gap under `tol` within `max_iter` rounds, and no candidate
    /// is compared against a solution it cannot vouch for (D84).
    Solver,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Values are needed for the cells [`Search::wanted`] names.
    NeedCells,
    /// Neither side can improve. `complete`: every action in both lists was
    /// considered. Always so since D69 (a window slides on until every action has
    /// been looked at); `false` is kept for readers of older reports.
    Converged { complete: bool },
    Capped(Cap),
}

impl Status {
    pub fn is_done(self) -> bool {
        !matches!(self, Status::NeedCells)
    }
}

/// A cell's values, one per set of dice, and what they add up to.
#[derive(Clone, Debug, PartialEq)]
pub struct CellStat {
    pub row: usize,
    pub col: usize,
    pub n: usize,
    pub mean: f32,
    /// The standard deviation of the values (0 below two).
    pub std: f32,
}

/// What a search found.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The rows and columns in the matrix, as indices into the lists.
    pub rows_in: Vec<usize>,
    pub cols_in: Vec<usize>,
    /// The viewer's mix over `rows_in` and the other side's over `cols_in`.
    pub x: Vec<f64>,
    pub y: Vec<f64>,
    /// The value to the viewer.
    pub value: f64,
    /// The solver's gap (see [`solve`]).
    pub gap: f64,
    /// Every cell with a value, scanned candidates included.
    pub cells: Vec<CellStat>,
    /// The mass of each mix above the support threshold.
    pub supp_mass: [f64; 2],
    pub steps: usize,
    /// Leaf values taken in.
    pub leaves: usize,
    pub elapsed: Duration,
    /// `None` while the search is still going.
    pub status: Option<Status>,
    /// How many distinct actions outside the matrix each side's scans considered.
    pub scanned: [usize; 2],
    /// How many actions each side's list has.
    pub lists: [usize; 2],
    /// Rounds the last solve took.
    pub iters: usize,
    pub mode: Mode,
}

/// Below this, an action is not in a mix's support.
pub const SUPPORT: f64 = 1e-4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase_ {
    Fill,
    Solve,
    Scan,
    Done,
}

#[derive(Clone, Debug, Default)]
struct Scan {
    cand_rows: Vec<usize>,
    cand_cols: Vec<usize>,
    supp_rows: Vec<usize>,
    supp_cols: Vec<usize>,
    /// The candidates are every action outside the matrix, on both sides: a scan that finds
    /// nothing better is then a proof of convergence.
    full: bool,
}

pub struct Search {
    root: Game,
    view: usize,
    rows: Vec<[usize; 2]>,
    cols: Vec<[usize; 2]>,
    /// A plan per row and per column (empty: none), see [`Plan`].
    row_plans: Vec<Plan>,
    col_plans: Vec<Plan>,
    budget: Budget,
    seed: u64,
    rows_in: Vec<usize>,
    cols_in: Vec<usize>,
    /// Per (row, col) of the lists: the value under each set of dice.
    cache: Vec<Vec<Option<f32>>>,
    phase: Phase_,
    solution: Option<Solution>,
    scan: Scan,
    steps: usize,
    leaves: usize,
    started: Instant,
    status: Option<Status>,
    scanned: [Vec<bool>; 2],
}

/// Whether every (action, plan) in a list is its own.
fn distinct(list: &[[usize; 2]], plans: &[Plan]) -> bool {
    let mut seen: Vec<([usize; 2], Plan)> =
        list.iter().enumerate().map(|(k, a)| (*a, plans.get(k).copied().unwrap_or(NO_PLANS))).collect();
    seen.sort_unstable();
    seen.windows(2).all(|w| w[0] != w[1])
}

impl Search {
    /// A search from `root` for `view`, over `rows` (the viewer's joint
    /// actions, likeliest first) and `cols` (the other side's). Refuses a
    /// finished battle, a root the budget's [`Mode`] does not search, an empty
    /// or repetitive list, and an action the root does not allow.
    pub fn new(
        root: Game,
        view: usize,
        rows: Vec<[usize; 2]>,
        cols: Vec<[usize; 2]>,
        budget: Budget,
        seed: u64,
    ) -> Result<Search, Error> {
        Search::with_plans(root, view, rows, cols, Vec::new(), Vec::new(), budget, seed)
    }

    /// [`Search::new`] with a [`Plan`] per row and per column (empty for
    /// none): what each action brings in if its position is asked to switch
    /// in the middle of the turn. The same action may be listed with
    /// different plans; it is the pair that must be its own.
    #[allow(clippy::too_many_arguments)]
    pub fn with_plans(
        root: Game,
        view: usize,
        rows: Vec<[usize; 2]>,
        cols: Vec<[usize; 2]>,
        row_plans: Vec<Plan>,
        col_plans: Vec<Plan>,
        budget: Budget,
        seed: u64,
    ) -> Result<Search, Error> {
        if view > 1 {
            return Err(Error::BadChoice("view is 0 or 1".into()));
        }
        check_root(&root, budget.mode)?;
        if budget.mode != Mode::Move && budget.seeds != 1 {
            return Err(Error::BadChoice(format!(
                "a search in mode {:?} plays one step from a choice: one set of dice, not {}",
                budget.mode.name(),
                budget.seeds
            )));
        }
        if rows.is_empty() || cols.is_empty() {
            return Err(Error::BadChoice("each side needs at least one joint action".into()));
        }
        if !row_plans.is_empty() && row_plans.len() != rows.len() || !col_plans.is_empty() && col_plans.len() != cols.len()
        {
            return Err(Error::BadChoice("a plan per action, or none".into()));
        }
        if !distinct(&rows, &row_plans) || !distinct(&cols, &col_plans) {
            return Err(Error::BadChoice("a joint action is listed twice".into()));
        }
        for (side, list) in [(view, &rows), (1 - view, &cols)] {
            let legal = root.legal(side);
            if let Some(a) = list.iter().find(|a| !legal.contains(a)) {
                return Err(Error::BadChoice(format!("p{}: {a:?} is not legal at the root", side + 1)));
            }
        }
        if budget.seeds == 0 || budget.initial == 0 {
            return Err(Error::BadChoice("seeds and initial are at least 1".into()));
        }
        let (m, n) = (rows.len(), cols.len());
        Ok(Search {
            root,
            view,
            rows_in: (0..budget.initial.min(m)).collect(),
            cols_in: (0..budget.initial.min(n)).collect(),
            cache: vec![Vec::new(); m * n],
            phase: Phase_::Fill,
            solution: None,
            scan: Scan::default(),
            steps: 0,
            leaves: 0,
            started: Instant::now(),
            status: None,
            scanned: [vec![false; m], vec![false; n]],
            rows,
            cols,
            row_plans,
            col_plans,
            budget,
            seed,
        })
    }

    /// The plan of each row and column (empty when none were given).
    pub fn row_plans(&self) -> &[Plan] {
        &self.row_plans
    }
    pub fn col_plans(&self) -> &[Plan] {
        &self.col_plans
    }

    pub fn root(&self) -> &Game {
        &self.root
    }
    pub fn view(&self) -> usize {
        self.view
    }
    pub fn rows(&self) -> &[[usize; 2]] {
        &self.rows
    }
    pub fn cols(&self) -> &[[usize; 2]] {
        &self.cols
    }
    pub fn budget(&self) -> &Budget {
        &self.budget
    }
    pub fn is_done(&self) -> bool {
        self.phase == Phase_::Done
    }

    fn slots(&self, r: usize, c: usize) -> &Vec<Option<f32>> {
        &self.cache[r * self.cols.len() + c]
    }

    /// The sets of dice a cell still lacks: up to `seeds`, or more once deepened.
    fn missing(&self, r: usize, c: usize) -> impl Iterator<Item = CellId> + '_ {
        let slots = self.slots(r, c);
        let want = self.budget.seeds.max(slots.len());
        (0..want).filter(move |&k| slots.get(k).copied().flatten().is_none()).map(move |k| CellId {
            row: r as u32,
            col: c as u32,
            seed: k as u32,
        })
    }

    fn mean(&self, r: usize, c: usize) -> Option<f32> {
        let vals: Vec<f32> = self.slots(r, c).iter().flatten().copied().collect();
        (!vals.is_empty()).then(|| vals.iter().sum::<f32>() / vals.len() as f32)
    }

    /// The cells whose values the next step needs and does not have.
    pub fn wanted(&self) -> Vec<CellId> {
        let mut out = Vec::new();
        match self.phase {
            Phase_::Fill => {
                for &r in &self.rows_in {
                    for &c in &self.cols_in {
                        out.extend(self.missing(r, c));
                    }
                }
            }
            Phase_::Scan => {
                for &r in &self.scan.cand_rows {
                    for &c in &self.scan.supp_cols {
                        out.extend(self.missing(r, c));
                    }
                }
                for &c in &self.scan.cand_cols {
                    for &r in &self.scan.supp_rows {
                        out.extend(self.missing(r, c));
                    }
                }
            }
            Phase_::Solve | Phase_::Done => {}
        }
        out
    }

    /// Plays `cells` from the root (see [`simulate_planned`]), in the search's [`Mode`].
    pub fn simulate(&self, cells: &[CellId], opts: &SimOpts, pool: &ThreadPool) -> Result<Leaves, Error> {
        let (rows, cols, rp, cp) = (&self.rows, &self.cols, &self.row_plans, &self.col_plans);
        let opts = SimOpts { mode: self.budget.mode, ..*opts };
        simulate_planned(&self.root, self.view, rows, cols, rp, cp, cells, self.seed, &opts, pool)
    }

    /// Takes in the value of each cell's leaf, in `[-1, 1]` (anything else is refused), one per
    /// cell, replacing any it had; a replaced value is not a new leaf.
    pub fn supply(&mut self, cells: &[CellId], values: &[f32]) -> Result<(), Error> {
        if cells.len() != values.len() {
            return Err(Error::BadChoice(format!("{} cells but {} values", cells.len(), values.len())));
        }
        let (m, n) = (self.rows.len(), self.cols.len());
        for (c, &v) in cells.iter().zip(values) {
            if !v.is_finite() {
                return Err(Error::BadChoice(format!("cell {c:?}: the value {v} is not a number")));
            }
            if !(-1.0..=1.0).contains(&v) {
                return Err(Error::BadChoice(format!("cell {c:?}: the value {v} is outside [-1, 1]")));
            }
            let (r, k) = (c.row as usize, c.seed as usize);
            if r >= m || c.col as usize >= n {
                return Err(Error::BadChoice(format!("cell {c:?} is outside the {m}×{n} lists")));
            }
            let slots = &mut self.cache[r * n + c.col as usize];
            if slots.len() <= k {
                slots.resize(k + 1, None);
            }
            if slots[k].replace(v).is_none() {
                self.leaves += 1;
            }
        }
        Ok(())
    }

    /// Asks for `extra` more sets of dice for a cell. The search solves again before it goes on.
    /// (A search of one step, [`Mode::Replacement`] or [`Mode::Preview`], has one set.)
    pub fn deepen(&mut self, row: usize, col: usize, extra: usize) -> Result<(), Error> {
        let (m, n) = (self.rows.len(), self.cols.len());
        if row >= m || col >= n {
            return Err(Error::BadChoice(format!("cell ({row}, {col}) is outside the {m}×{n} lists")));
        }
        if extra > 0 && self.budget.mode != Mode::Move {
            return Err(Error::BadChoice(format!("a search in mode {:?} has one set of dice", self.budget.mode.name())));
        }
        let slots = &mut self.cache[row * n + col];
        let have = slots.len().max(self.budget.seeds);
        slots.resize(have + extra, None);
        if !self.rows_in.contains(&row) {
            self.rows_in.push(row);
        }
        if !self.cols_in.contains(&col) {
            self.cols_in.push(col);
        }
        self.phase = Phase_::Fill;
        self.status = None;
        Ok(())
    }

    fn matrix(&self) -> Vec<f32> {
        let mut a = Vec::with_capacity(self.rows_in.len() * self.cols_in.len());
        for &r in &self.rows_in {
            for &c in &self.cols_in {
                a.push(self.mean(r, c).expect("the matrix is filled before it is solved"));
            }
        }
        a
    }

    fn finish(&mut self, status: Status) -> Status {
        self.phase = Phase_::Done;
        self.status = Some(status);
        status
    }

    /// Moves the search on as far as it can without new values.
    pub fn advance(&mut self) -> Status {
        loop {
            match self.phase {
                Phase_::Done => return self.status.expect("a finished search has a status"),
                Phase_::Fill => {
                    if self.wanted().is_empty() {
                        self.phase = Phase_::Solve;
                    } else {
                        return Status::NeedCells;
                    }
                }
                Phase_::Solve => {
                    let a = self.matrix();
                    let s = solve(&a, self.rows_in.len(), self.cols_in.len(), self.budget.solver);
                    let unsure = s.gap > self.budget.solver.tol;
                    self.solution = Some(s);
                    if let Some(capped) = self.capped() {
                        return self.finish(capped);
                    }
                    if unsure {
                        return self.finish(Status::Capped(Cap::Solver));
                    }
                    self.plan_scan();
                    self.phase = Phase_::Scan;
                }
                Phase_::Scan => {
                    if !self.wanted().is_empty() {
                        return Status::NeedCells;
                    }
                    if self.oracle_step() {
                        self.steps += 1;
                        self.phase = Phase_::Fill;
                        continue;
                    }
                    // Nothing in the window improved on the value: the window slides on to the
                    // actions not yet looked at, against the same solution (D69); once all have
                    // been, one scan of every action outside the matrix settles it.
                    if self.unscanned() || !self.scan.full {
                        self.steps += 1;
                        if let Some(capped) = self.capped() {
                            return self.finish(capped);
                        }
                        self.plan_scan();
                        continue;
                    }
                    // (Every action is in the matrix or has been looked at: `complete`.)
                    return self.finish(Status::Converged { complete: !self.unscanned() });
                }
            }
        }
    }

    /// Whether the time cap or the step cap stops the search here.
    fn capped(&self) -> Option<Status> {
        if self.budget.time_cap.is_some_and(|cap| self.started.elapsed() >= cap) {
            return Some(Status::Capped(Cap::Time));
        }
        (self.steps >= self.budget.max_steps).then_some(Status::Capped(Cap::Steps))
    }

    /// Whether either side has an action outside the matrix that no scan has looked at.
    fn unscanned(&self) -> bool {
        let left = |side: usize, in_: &[usize]| {
            (0..self.scanned[side].len()).any(|k| !self.scanned[side][k] && !in_.contains(&k))
        };
        left(0, &self.rows_in) || left(1, &self.cols_in)
    }

    /// Which candidates the scan considers and which support they are valued against. A
    /// window takes the next actions not yet looked at, and every action outside the matrix
    /// once there are none left (D69).
    fn plan_scan(&mut self) {
        let s = self.solution.as_ref().expect("solved");
        let support = |mix: &[f64], in_: &[usize]| -> Vec<usize> {
            mix.iter().enumerate().filter(|(_, p)| **p > SUPPORT).map(|(k, _)| in_[k]).collect()
        };
        let supp_rows = support(&s.x, &self.rows_in);
        let supp_cols = support(&s.y, &self.cols_in);
        // A side's candidates, and whether they are every action outside the matrix.
        let outside = |side: usize, in_: &[usize]| -> (Vec<usize>, bool) {
            let rest = || (0..self.scanned[side].len()).filter(move |k| !in_.contains(k));
            match self.budget.candidates {
                Candidates::All => (rest().collect(), true),
                Candidates::Next(m) => {
                    let fresh: Vec<usize> = rest().filter(|&k| !self.scanned[side][k]).take(m).collect();
                    if fresh.is_empty() { (rest().collect(), true) } else { (fresh, false) }
                }
            }
        };
        let (cand_rows, all_rows) = outside(0, &self.rows_in);
        let (cand_cols, all_cols) = outside(1, &self.cols_in);
        for &r in &cand_rows {
            self.scanned[0][r] = true;
        }
        for &c in &cand_cols {
            self.scanned[1][c] = true;
        }
        self.scan = Scan { cand_rows, cand_cols, supp_rows, supp_cols, full: all_rows && all_cols };
    }

    /// Adds each side's best candidate if it improves on the current value; whether any was added.
    fn oracle_step(&mut self) -> bool {
        let s = self.solution.as_ref().expect("solved");
        let value = s.value;
        // The viewer: a row that does better against the other side's mix.
        let mut best_row: Option<(usize, f64)> = None;
        for &r in &self.scan.cand_rows {
            let mut total = 0.0;
            for (k, &c) in self.cols_in.iter().enumerate() {
                let p = s.y[k];
                if p > SUPPORT {
                    total += p * self.mean(r, c).expect("scanned") as f64;
                }
            }
            if best_row.is_none_or(|(_, v)| total > v) {
                best_row = Some((r, total));
            }
        }
        // The other side: a column that holds the viewer to less.
        let mut best_col: Option<(usize, f64)> = None;
        for &c in &self.scan.cand_cols {
            let mut total = 0.0;
            for (k, &r) in self.rows_in.iter().enumerate() {
                let p = s.x[k];
                if p > SUPPORT {
                    total += p * self.mean(r, c).expect("scanned") as f64;
                }
            }
            if best_col.is_none_or(|(_, v)| total < v) {
                best_col = Some((c, total));
            }
        }
        let eps = self.budget.eps;
        let mut added = false;
        if let Some((r, v)) = best_row {
            if v > value + eps {
                self.rows_in.push(r);
                added = true;
            }
        }
        if let Some((c, v)) = best_col {
            if v < value - eps {
                self.cols_in.push(c);
                added = true;
            }
        }
        added
    }

    pub fn report(&self) -> Report {
        let (m, n) = (self.rows.len(), self.cols.len());
        let mut cells = Vec::new();
        for r in 0..m {
            for c in 0..n {
                let vals: Vec<f32> = self.slots(r, c).iter().flatten().copied().collect();
                if vals.is_empty() {
                    continue;
                }
                let k = vals.len() as f32;
                let mean = vals.iter().sum::<f32>() / k;
                let squares: f32 = vals.iter().map(|v| (v - mean) * (v - mean)).sum();
                let var = if vals.len() > 1 { squares / (k - 1.0) } else { 0.0 };
                cells.push(CellStat { row: r, col: c, n: vals.len(), mean, std: var.sqrt() });
            }
        }
        let (x, y, value, gap) = match &self.solution {
            Some(s) => (s.x.clone(), s.y.clone(), s.value, s.gap),
            None => (Vec::new(), Vec::new(), f64::NAN, f64::NAN),
        };
        let mass = |mix: &[f64]| mix.iter().filter(|&&p| p > SUPPORT).sum::<f64>();
        Report {
            rows_in: self.rows_in.clone(),
            cols_in: self.cols_in.clone(),
            supp_mass: [mass(&x), mass(&y)],
            x,
            y,
            value,
            gap,
            cells,
            steps: self.steps,
            leaves: self.leaves,
            elapsed: self.started.elapsed(),
            status: self.status,
            scanned: [self.scanned[0].iter().filter(|&&b| b).count(), self.scanned[1].iter().filter(|&&b| b).count()],
            lists: [m, n],
            iters: self.solution.as_ref().map_or(0, |s| s.iters),
            mode: self.budget.mode,
        }
    }
}

/// Drives searches to their end together: each round, every search still
/// going names its cells, all of them are simulated, their leaves are valued
/// in one call, and the values handed back. `value` returns one number per
/// leaf, in the order given.
pub fn run(
    searches: &mut [Search],
    value: &mut dyn FnMut(&Leaves) -> Vec<f32>,
    opts: &SimOpts,
    pool: &ThreadPool,
) -> Result<(), Error> {
    loop {
        let mut batch = Leaves::default();
        let mut asked: Vec<(usize, Vec<CellId>)> = Vec::new();
        for (k, s) in searches.iter_mut().enumerate() {
            if s.is_done() {
                continue;
            }
            if s.advance() == Status::NeedCells {
                let cells = s.wanted();
                batch.append(s.simulate(&cells, opts, pool)?);
                asked.push((k, cells));
            }
        }
        if asked.is_empty() {
            return Ok(());
        }
        let values = value(&batch);
        if values.len() != batch.n {
            return Err(Error::BadChoice(format!("{} leaves but {} values", batch.n, values.len())));
        }
        let mut at = 0;
        for (k, cells) in asked {
            searches[k].supply(&cells, &values[at..at + cells.len()])?;
            at += cells.len();
        }
    }
}

/// A side's joint actions with the likeliest first, without a model: what
/// the engine's greedy player would do, then its look-ahead player, then
/// the rest as the engine lists them. At Team Preview or a switch request,
/// the engine's order.
pub fn heuristic_order(game: &Game, side: usize, rng: &mut Rng) -> Vec<[usize; 2]> {
    let legal = game.legal(side);
    if game.ended() || game.phase() != Phase::Move || legal.len() <= 1 {
        return legal;
    }
    let mut out = Vec::with_capacity(legal.len());
    for kind in [Baseline::Greedy, Baseline::Lookahead] {
        let pick = game.baseline(side, kind, rng);
        if legal.contains(&pick) && !out.contains(&pick) {
            out.push(pick);
        }
    }
    let picked = out.clone();
    out.extend(legal.iter().copied().filter(|a| !picked.contains(a)));
    out
}
