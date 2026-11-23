//! Cells: one joint action played for one turn under one set of dice, and
//! what the value function is handed about the leaf.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rayon::ThreadPool;

use crate::battle::Error;
use crate::env::{Baseline, Game, OBS_M};
use crate::obs::{OBS_F, OBS_I, Phase};
use crate::rng::Rng;

/// A cell of a search: a row action (the viewer's), a column action (the
/// other side's) and which set of dice, all as indices into the search's lists.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CellId {
    pub row: u32,
    pub col: u32,
    pub seed: u32,
}

/// The four seed words for the `k`th set of dice of a search seeded with
/// `root_seed` (splitmix64, so that neighbouring seeds give unrelated dice).
/// Every cell with the same `k` plays under the same words, whatever its
/// actions: common random numbers, so that two cells differ by their actions
/// before they differ by their luck.
pub fn words(root_seed: u64, k: u32) -> [u16; 4] {
    let mut z = root_seed.wrapping_add((k as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    Rng { seed: z ^ (z >> 31), calls: 0 }.words()
}

/// What each of a side's positions brings in if it is asked to switch in the
/// middle of the turn (U-turn, Volt Switch, Parting Shot, an Eject Button):
/// the engine action of the Pokémon to bring in (`40 + j` for the one
/// registered `j`th), or [`NO_PLAN`] for one of the dice's choosing. A joint
/// action with a plan is "U-turn into this Pokémon": one action, decided at
/// the root, not a switch left to chance.
pub type Plan = [u8; 2];
/// No plan for a position: a random legal replacement from the cell's dice.
pub const NO_PLAN: u8 = u8::MAX;
/// No plan for either position.
pub const NO_PLANS: Plan = [NO_PLAN, NO_PLAN];
const SWITCH_LO: u8 = 40;
const SWITCH_HI: u8 = 45;

/// What a cell does when the engine asks for a switch before the leaf.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Follow {
    /// Answer a switch request in the middle of the turn (U-turn, Eject
    /// Button) with a random legal replacement, and stop once the turn is
    /// over: at the next move request, at the request for replacing the
    /// fainted at the end of the turn, or at the end of the battle. The leaf
    /// is then a whole turn's worth of play, and nothing is assumed about
    /// what either side brings in afterwards.
    #[default]
    EndOfTurn,
    /// Answer every switch request with a random legal replacement, up to the
    /// next move request or the end of the battle.
    ToMove,
    /// Stop at whatever request comes next, even in the middle of the turn.
    None,
}

/// What the root of a search is, and so what one of its cells plays (D82). A search from a
/// request for replacements or from Team Preview has to ask for it: the default refuses them,
/// since a turn's worth of play from there is none at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// A move request: a cell plays the turn ([`Follow`] says what it does at a switch request).
    #[default]
    Move,
    /// A request for replacements at the end of a turn: a cell sends each side's in, and its
    /// leaf is the request that comes next (as a rule the next turn's move request): the
    /// one-ply evaluation of a replacement. Nothing in it is left to the dice but the rare
    /// tie, so a cell has one set of dice.
    Replacement,
    /// Team Preview: a cell is each side's pick of four, and its leaf the battle's first move
    /// request, with the leads in. One set of dice, which seeds the battle.
    Preview,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Move => "move",
            Mode::Replacement => "replacement",
            Mode::Preview => "preview",
        }
    }
}

/// Whether a search in `mode` can start from `root`: a move request for [`Mode::Move`], a request
/// for replacements at the end of a turn for [`Mode::Replacement`], Team Preview for
/// [`Mode::Preview`]. A finished battle, and a switch in the middle of a turn, have no search.
pub fn check_root(root: &Game, mode: Mode) -> Result<(), Error> {
    if root.ended() {
        return Err(Error::BadState("the battle is over".into()));
    }
    match (root.phase(), mode) {
        (Phase::Move, Mode::Move) | (Phase::Preview, Mode::Preview) => Ok(()),
        (Phase::Switch, Mode::Replacement) if root.turn_over() => Ok(()),
        (Phase::Switch, _) if !root.turn_over() => {
            Err(Error::BadState("a switch in the middle of a turn: its moves are still to run".into()))
        }
        (Phase::Switch, Mode::Move) => {
            Err(Error::BadState("a request for replacements: search it in mode \"replacement\"".into()))
        }
        (Phase::Preview, Mode::Move) => Err(Error::BadState("Team Preview: search it in mode \"preview\"".into())),
        (phase, mode) => Err(Error::BadState(format!("a {phase:?} root is not searched in mode {:?}", mode.name()))),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SimOpts {
    /// Write each leaf's observation for the viewer (what a network reads).
    pub observe: bool,
    /// Keep each leaf's [`Game`] (for a search that goes on from it).
    pub keep_states: bool,
    /// What a cell does at a switch request before its leaf, in [`Mode::Move`]. (A cell of the
    /// other modes is one step, whatever this says.)
    pub follow: Follow,
    pub mode: Mode,
}

impl Default for SimOpts {
    fn default() -> Self {
        SimOpts { observe: true, keep_states: false, follow: Follow::EndOfTurn, mode: Mode::Move }
    }
}

/// Numbers in a leaf's summary: the battle has ended; the outcome for the
/// viewer (1 won, -1 lost, 0 otherwise); each side's HP left as a sum of
/// fractions over the Pokémon it brought, and how many of them stand; the
/// phase ([`Phase`] as a number) and the turn.
pub const SUMMARY: usize = 8;
pub const ENDED: usize = 0;
pub const OUTCOME: usize = 1;
pub const HP_VIEW: usize = 2;
pub const HP_FOE: usize = 3;
pub const ALIVE_VIEW: usize = 4;
pub const ALIVE_FOE: usize = 5;
pub const PHASE: usize = 6;
pub const TURN: usize = 7;

/// The leaves of a batch of cells, in the order the cells were given.
#[derive(Clone, Debug, Default)]
pub struct Leaves {
    pub n: usize,
    /// `n × OBS_F` observation features for the viewer; empty unless asked for.
    pub f: Vec<f32>,
    /// `n × OBS_I` observation ids; empty unless asked for.
    pub i: Vec<i16>,
    /// `n × SUMMARY`.
    pub summary: Vec<f32>,
    /// The leaf positions; empty unless asked for.
    pub states: Vec<Game>,
}

impl Leaves {
    pub fn summary_of(&self, k: usize) -> &[f32] {
        &self.summary[k * SUMMARY..(k + 1) * SUMMARY]
    }

    /// Appends another batch (its cells after this one's).
    pub fn append(&mut self, mut other: Leaves) {
        self.n += other.n;
        self.f.append(&mut other.f);
        self.i.append(&mut other.i);
        self.summary.append(&mut other.summary);
        self.states.append(&mut other.states);
    }
}

/// A leaf's summary from the viewer's side.
pub fn summarize(game: &Game, view: usize) -> [f32; SUMMARY] {
    let mut s = [0.0; SUMMARY];
    let Some(b) = game.battle() else {
        return s;
    };
    let worth = |side: usize| -> (f32, f32) {
        let team = &b.sides[side].team[..b.sides[side].n as usize];
        let hp: f32 = team.iter().map(|m| m.hp as f32 / m.max_hp().max(1) as f32).sum();
        let alive = team.iter().filter(|m| !m.fainted && m.hp > 0).count() as f32;
        (hp, alive)
    };
    let (hp_view, alive_view) = worth(view);
    let (hp_foe, alive_foe) = worth(1 - view);
    s[ENDED] = b.ended as u8 as f32;
    s[OUTCOME] = match b.winner.filter(|_| b.ended) {
        Some(w) if w as usize == view => 1.0,
        Some(_) => -1.0,
        None => 0.0,
    };
    s[HP_VIEW] = hp_view;
    s[HP_FOE] = hp_foe;
    s[ALIVE_VIEW] = alive_view;
    s[ALIVE_FOE] = alive_foe;
    s[PHASE] = game.phase() as u8 as f32;
    s[TURN] = b.turn as f32;
    s
}

/// A hand score of a leaf for the viewer, in `[-1, 1]`: the HP it has left
/// less the other side's (as sums of fractions), `faint_bonus` for each
/// Pokémon standing on its side less each on the other, and `terminal_bonus`
/// for a battle won (less for one lost), divided by the most that can add up
/// to. The same ranking of leaves as a score that counts what each side
/// *lost* from a given root, since the root is a constant.
pub fn hand_value(summary: &[f32], faint_bonus: f32, terminal_bonus: f32) -> f32 {
    let s = summary;
    let mut v = s[HP_VIEW] - s[HP_FOE] + faint_bonus * (s[ALIVE_VIEW] - s[ALIVE_FOE]);
    if s[ENDED] > 0.5 {
        v += terminal_bonus * s[OUTCOME];
    }
    v / (4.0 * (1.0 + faint_bonus) + terminal_bonus)
}

struct Scratch {
    mask: Vec<u8>,
    rng: Rng,
}

/// The replacement a side planned for a switch in the middle of the turn,
/// where it is legal now: both positions' plans, else one of them with what
/// the dice chose for the other, else the dice's choice.
fn planned(g: &Game, side: usize, dice: [usize; 2], plan: Plan) -> [usize; 2] {
    if plan == NO_PLANS {
        return dice;
    }
    let legal = g.legal(side);
    let want = |k: usize| if plan[k] == NO_PLAN { dice[k] } else { plan[k] as usize };
    let tries = [[want(0), want(1)], [want(0), dice[1]], [dice[0], want(1)]];
    tries.into_iter().find(|a| legal.contains(a)).unwrap_or(dice)
}

/// A plan is a switch action or none for each position.
fn check_plans(plans: &[Plan], n: usize, what: &str) -> Result<(), Error> {
    if !plans.is_empty() && plans.len() != n {
        return Err(Error::BadChoice(format!("{what}: {} plans for {n} actions", plans.len())));
    }
    for p in plans {
        for &k in p {
            if k != NO_PLAN && !(SWITCH_LO..=SWITCH_HI).contains(&k) {
                return Err(Error::BadChoice(format!("{what}: a plan names a switch action 40 to 45 or none, not {k}")));
            }
        }
    }
    Ok(())
}

fn scratch() -> Scratch {
    Scratch { mask: vec![0; OBS_M], rng: Rng { seed: 0, calls: 0 } }
}

/// Plays one cell and writes its observation into `f` and `i` if they are not empty.
fn play(
    root: &Game,
    view: usize,
    mine: [usize; 2],
    theirs: [usize; 2],
    plans: [Plan; 2],
    k: u32,
    root_seed: u64,
    opts: &SimOpts,
    sc: &mut Scratch,
    f: &mut [f32],
    i: &mut [i16],
    kept: &mut Vec<Game>,
) -> Result<[f32; SUMMARY], Error> {
    let w = words(root_seed, k);
    let mut g = root.clone();
    g.reseed(w);
    let mut actions = [[0usize; 2]; 2];
    actions[view] = mine;
    actions[1 - view] = theirs;
    g.act(actions, w)?;
    // Replacements in the middle of the turn roll their own dice, apart from the battle's.
    sc.rng = Rng::from_words(words(root_seed ^ 0x5EED_5EED_5EED_5EED, k));
    // (A replacement or a Team Preview pick is one step: its leaf is the request after it.)
    while opts.mode == Mode::Move {
        if g.ended() || g.phase() != Phase::Switch {
            break;
        }
        let go_on = match opts.follow {
            Follow::None => false,
            Follow::EndOfTurn => !g.turn_over(),
            Follow::ToMove => true,
        };
        if !go_on {
            break;
        }
        let mut a = [g.baseline(0, Baseline::Random, &mut sc.rng), g.baseline(1, Baseline::Random, &mut sc.rng)];
        if !g.turn_over() {
            // A switch in the middle of the turn: what the action planned, where it is legal.
            for side in 0..2 {
                a[side] = planned(&g, side, a[side], plans[side]);
            }
        }
        g.act(a, w)?;
    }
    let s = summarize(&g, view);
    if !f.is_empty() {
        g.observe(view, f, i, &mut sc.mask);
    }
    if opts.keep_states {
        kept.push(g);
    }
    Ok(s)
}

/// A run of neighbouring cells and where their leaves go.
struct Chunk<'a> {
    at: usize,
    cells: &'a [CellId],
    summary: &'a mut [f32],
    f: &'a mut [f32],
    i: &'a mut [i16],
}

/// Cells a chunk: small enough that the threads share a call evenly, large enough that taking one costs nothing.
fn chunk_len(cells: usize, threads: usize) -> usize {
    (cells / (4 * threads.max(1))).clamp(4, 64)
}

/// Cells a thread should have to itself before it is worth waking.
const CELLS_A_HELPER: usize = 16;

/// Plays every cell of `cells` from `root` and returns their leaves in the
/// same order. `rows` are the viewer's joint actions and `cols` the other
/// side's; `seed` names the dice ([`words`]). A cell whose actions the
/// engine refuses is an error (the lists should come from [`Game::legal`]).
/// Switches in the middle of the turn are the dice's ([`simulate_planned`]
/// takes a plan per action).
#[allow(clippy::too_many_arguments)]
pub fn simulate(
    root: &Game,
    view: usize,
    rows: &[[usize; 2]],
    cols: &[[usize; 2]],
    cells: &[CellId],
    seed: u64,
    opts: &SimOpts,
    pool: &ThreadPool,
) -> Result<Leaves, Error> {
    simulate_planned(root, view, rows, cols, &[], &[], cells, seed, opts, pool)
}

/// [`simulate`] with a [`Plan`] per row and per column (empty lists for
/// none): what each action brings in if its position is asked to switch in
/// the middle of the turn. With no plans the leaves are exactly
/// [`simulate`]'s.
#[allow(clippy::too_many_arguments)]
pub fn simulate_planned(
    root: &Game,
    view: usize,
    rows: &[[usize; 2]],
    cols: &[[usize; 2]],
    row_plans: &[Plan],
    col_plans: &[Plan],
    cells: &[CellId],
    seed: u64,
    opts: &SimOpts,
    pool: &ThreadPool,
) -> Result<Leaves, Error> {
    check_plans(row_plans, rows.len(), "rows")?;
    check_plans(col_plans, cols.len(), "cols")?;
    let plans_of = |c: &CellId| -> [Plan; 2] {
        let mut p = [NO_PLANS; 2];
        p[view] = row_plans.get(c.row as usize).copied().unwrap_or(NO_PLANS);
        p[1 - view] = col_plans.get(c.col as usize).copied().unwrap_or(NO_PLANS);
        p
    };
    if view > 1 {
        return Err(Error::BadChoice("view is 0 or 1".into()));
    }
    check_root(root, opts.mode)?;
    for c in cells {
        if c.row as usize >= rows.len() || c.col as usize >= cols.len() {
            let (m, n) = (rows.len(), cols.len());
            return Err(Error::BadChoice(format!("cell ({}, {}) is outside the {m}×{n} lists", c.row, c.col)));
        }
    }
    let n = cells.len();
    let mut out = Leaves {
        n,
        f: vec![0.0; if opts.observe { n * OBS_F } else { 0 }],
        i: vec![0; if opts.observe { n * OBS_I } else { 0 }],
        summary: vec![0.0; n * SUMMARY],
        states: Vec::new(),
    };
    // The cells in chunks, each with its own part of the output: whichever thread plays a chunk, a leaf
    // lands where its cell stands in the list. The calling thread plays chunks itself and the pool's
    // threads join it as they wake, so a small call after a pause (the driver was valuing the last
    // leaves) never waits on a sleeping pool; nothing is allocated per cell unless the states are kept.
    let (observe, keep) = (opts.observe, opts.keep_states);
    let size = chunk_len(n, pool.current_num_threads());
    let mut work: Vec<Chunk> = Vec::with_capacity(n.div_ceil(size));
    let (mut summary, mut f, mut i) = (&mut out.summary[..], &mut out.f[..], &mut out.i[..]);
    for (at, cs) in cells.chunks(size).enumerate() {
        let (s0, s1) = std::mem::take(&mut summary).split_at_mut(cs.len() * SUMMARY);
        let (f0, f1) = std::mem::take(&mut f).split_at_mut(if observe { cs.len() * OBS_F } else { 0 });
        let (i0, i1) = std::mem::take(&mut i).split_at_mut(if observe { cs.len() * OBS_I } else { 0 });
        (summary, f, i) = (s1, f1, i1);
        work.push(Chunk { at, cells: cs, summary: s0, f: f0, i: i0 });
    }
    let chunks = work.len();
    work.reverse();
    let queue = Mutex::new(work);
    let kept: Mutex<Vec<(usize, Vec<Game>)>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<(usize, Error)>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let worker = || {
        let mut sc = scratch();
        while !stop.load(Ordering::Relaxed) {
            let Some(ch) = queue.lock().unwrap_or_else(|e| e.into_inner()).pop() else {
                break;
            };
            let mut states = Vec::with_capacity(if keep { ch.cells.len() } else { 0 });
            for (j, c) in ch.cells.iter().enumerate() {
                let (mine, theirs) = (rows[c.row as usize], cols[c.col as usize]);
                let (f, i) = if observe {
                    (&mut ch.f[j * OBS_F..(j + 1) * OBS_F], &mut ch.i[j * OBS_I..(j + 1) * OBS_I])
                } else {
                    (&mut [][..], &mut [][..])
                };
                match play(root, view, mine, theirs, plans_of(c), c.seed, seed, opts, &mut sc, f, i, &mut states) {
                    Ok(s) => ch.summary[j * SUMMARY..(j + 1) * SUMMARY].copy_from_slice(&s),
                    Err(e) => {
                        // The first cell in the list's order that fails is the error: chunks are taken in
                        // order and one already taken is played out, so the earliest failure is always seen.
                        let mut failed = failed.lock().unwrap_or_else(|e| e.into_inner());
                        if failed.as_ref().is_none_or(|(at, _)| ch.at < *at) {
                            *failed = Some((ch.at, e));
                        }
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
            if keep {
                kept.lock().unwrap_or_else(|e| e.into_inner()).push((ch.at, states));
            }
        }
    };
    let helpers = (pool.current_num_threads().saturating_sub(1)).min(n / CELLS_A_HELPER).min(chunks.saturating_sub(1));
    if helpers == 0 {
        worker();
    } else {
        pool.in_place_scope(|s| {
            for _ in 0..helpers {
                s.spawn(|_| worker());
            }
            worker();
        });
    }
    if let Some((_, e)) = failed.into_inner().unwrap_or_else(|e| e.into_inner()) {
        return Err(e);
    }
    if keep {
        let mut kept = kept.into_inner().unwrap_or_else(|e| e.into_inner());
        kept.sort_by_key(|(at, _)| *at);
        out.states = Vec::with_capacity(n);
        for (_, mut states) in kept {
            out.states.append(&mut states);
        }
    }
    Ok(out)
}

/// A thread pool of `threads` workers (0: one per core), made once and shared. The thread that calls
/// [`simulate`] plays cells itself, beside `threads - 1` of the pool's: `threads` at work in all.
pub fn pool(threads: usize) -> Arc<ThreadPool> {
    static POOLS: OnceLock<Mutex<HashMap<usize, Arc<ThreadPool>>>> = OnceLock::new();
    let pools = POOLS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut pools = pools.lock().unwrap_or_else(|e| e.into_inner());
    pools
        .entry(threads)
        .or_insert_with(|| {
            Arc::new(rayon::ThreadPoolBuilder::new().num_threads(threads).build().expect("a thread pool"))
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dice_differ_by_seed_and_repeat_exactly() {
        let all: Vec<[u16; 4]> = (0..1000).map(|k| words(20261013, k)).collect();
        let mut sorted = all.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), all.len(), "a thousand seeds give a thousand sets of dice");
        assert_eq!(words(20261013, 7), all[7]);
        assert_ne!(words(20261014, 7), all[7]);
    }

    #[test]
    fn the_hand_value_is_bounded_and_antisymmetric() {
        let mut s = [0.0f32; SUMMARY];
        s[HP_VIEW] = 4.0;
        s[ALIVE_VIEW] = 4.0;
        assert!((hand_value(&s, 1.0, 10.0) - 8.0 / 18.0).abs() < 1e-6, "all theirs down, all ours full, not over");
        s[ENDED] = 1.0;
        s[OUTCOME] = 1.0;
        assert!((hand_value(&s, 1.0, 10.0) - 1.0).abs() < 1e-6, "a clean win is 1");
        let mut t = [0.0f32; SUMMARY];
        t[HP_VIEW] = 1.5;
        t[HP_FOE] = 2.25;
        t[ALIVE_VIEW] = 2.0;
        t[ALIVE_FOE] = 3.0;
        let mut u = t;
        u.swap(HP_VIEW, HP_FOE);
        u.swap(ALIVE_VIEW, ALIVE_FOE);
        assert!((hand_value(&t, 1.0, 10.0) + hand_value(&u, 1.0, 10.0)).abs() < 1e-6);
        assert!(hand_value(&t, 1.0, 10.0).abs() <= 8.0 / 18.0);
    }
}
