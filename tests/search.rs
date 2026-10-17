//! The search module (`vgc_engine::search`): cells are deterministic and
//! play a whole turn, the double oracle finds the equilibrium of the whole
//! matrix without building all of it, and a cell is cheap.

use std::time::{Duration, Instant};

use vgc_engine::data::move_id;
use vgc_engine::env::{Baseline, Game};
use vgc_engine::format::Format;
use vgc_engine::obs::Phase;
use vgc_engine::position::{ActionState, BattleState, PokemonState, SideState};
use vgc_engine::rng::Rng;
use vgc_engine::search::cell::{PHASE, SUMMARY};
const PASS: usize = 46;
use vgc_engine::search::*;
use vgc_engine::{Battle, Choice, PokemonSet, Request};

fn teams(seed: u16) -> [Vec<PokemonSet>; 2] {
    let format = Format::current();
    let mut rng = Rng::from_words([seed, 1, 2, 3]);
    [format.random_team(&mut rng), format.random_team(&mut rng)]
}

/// A game between random teams played at random to the first move request at or after `turn`.
fn root(seed: u16, turn: u16, rng: &mut Rng) -> Option<Game> {
    let mut g = Game::new(teams(seed), true).unwrap();
    loop {
        if g.ended() {
            return None;
        }
        if g.phase() == Phase::Move && g.battle().map(|b| b.turn) >= Some(turn) {
            return Some(g);
        }
        let actions = [g.baseline(0, Baseline::Random, rng), g.baseline(1, Baseline::Random, rng)];
        g.act(actions, [rng.below(65536) as u16, 2, 3, 4]).unwrap();
    }
}

/// `n` roots at turns from `turns`, from games seeded from `first` on.
fn roots(n: usize, turns: &[u16], first: u16, rng: &mut Rng) -> Vec<Game> {
    let mut out = Vec::new();
    let mut seed = first;
    while out.len() < n {
        if let Some(g) = root(seed, turns[seed as usize % turns.len()], rng) {
            out.push(g);
        }
        seed += 1;
    }
    out
}

/// Up to `n` of a side's legal joint actions, distinct, in a random order.
fn some(game: &Game, side: usize, n: usize, rng: &mut Rng) -> Vec<[usize; 2]> {
    let mut legal = game.legal(side);
    let n_legal = legal.len();
    rng.shuffle(&mut legal, 0, n_legal);
    legal.truncate(n);
    legal
}

fn all_cells(rows: usize, cols: usize, seeds: usize) -> Vec<CellId> {
    let mut out = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            for k in 0..seeds {
                out.push(CellId { row: r as u32, col: c as u32, seed: k as u32 });
            }
        }
    }
    out
}

fn hand(leaves: &Leaves) -> Vec<f32> {
    (0..leaves.n).map(|k| hand_value(leaves.summary_of(k), 1.0, 10.0)).collect()
}

const NO_OBS: SimOpts = SimOpts { observe: false, keep_states: false, follow: Follow::EndOfTurn, mode: Mode::Move };

#[test]
fn a_cell_is_the_same_on_one_and_eight_threads_and_under_its_seed() {
    let mut rng = Rng::from_words([1, 2, 3, 4]);
    for g in roots(5, &[1, 3, 5], 100, &mut rng) {
        let (rows, cols) = (some(&g, 0, 6, &mut rng), some(&g, 1, 6, &mut rng));
        let cells = all_cells(rows.len(), cols.len(), 4);
        let opts = SimOpts::default();
        let one = simulate(&g, 0, &rows, &cols, &cells, 77, &opts, &pool(1)).unwrap();
        let eight = simulate(&g, 0, &rows, &cols, &cells, 77, &opts, &pool(8)).unwrap();
        assert_eq!(one.n, cells.len());
        assert!(one.f.len() == one.n * vgc_engine::obs::OBS_F && one.i.len() == one.n * vgc_engine::obs::OBS_I);
        assert!(one.summary == eight.summary && one.f == eight.f && one.i == eight.i, "threads change nothing");
        let again = simulate(&g, 0, &rows, &cols, &cells, 77, &opts, &pool(8)).unwrap();
        assert!(again.summary == one.summary && again.f == one.f, "a seed repeats exactly");
        let other = simulate(&g, 0, &rows, &cols, &cells, 78, &opts, &pool(8)).unwrap();
        assert_ne!(other.summary, one.summary, "other dice, other leaves (over {} cells)", cells.len());
        let bare = simulate(&g, 0, &rows, &cols, &cells, 77, &NO_OBS, &pool(8)).unwrap();
        assert!(bare.f.is_empty() && bare.i.is_empty() && bare.summary == one.summary);
    }
}

#[test]
fn the_hand_value_is_antisymmetric_in_the_view() {
    let mut rng = Rng::from_words([4, 3, 2, 1]);
    let mut ended = 0;
    for g in roots(8, &[2, 6, 10], 200, &mut rng) {
        let (rows, cols) = (some(&g, 0, 8, &mut rng), some(&g, 1, 8, &mut rng));
        let cells = all_cells(rows.len(), cols.len(), 4);
        let mirrored: Vec<CellId> = cells.iter().map(|c| CellId { row: c.col, col: c.row, seed: c.seed }).collect();
        let ours = simulate(&g, 0, &rows, &cols, &cells, 5, &NO_OBS, &pool(8)).unwrap();
        let theirs = simulate(&g, 1, &cols, &rows, &mirrored, 5, &NO_OBS, &pool(8)).unwrap();
        for k in 0..cells.len() {
            let (a, b) = (hand_value(ours.summary_of(k), 1.0, 10.0), hand_value(theirs.summary_of(k), 1.0, 10.0));
            assert_eq!(a, -b, "cell {:?}: {:?} against {:?}", cells[k], ours.summary_of(k), theirs.summary_of(k));
            let s = ours.summary_of(k);
            if s[vgc_engine::search::cell::ENDED] > 0.5 {
                ended += 1;
                assert!(a.abs() >= 11.0 / 18.0 - 1e-6, "a finished battle is worth at least the terminal bonus: {a}");
            } else {
                assert!(a.abs() <= 8.0 / 18.0 + 1e-6, "an unfinished one at most the HP and faints: {a}");
            }
        }
    }
    eprintln!("hand value: {ended} finished leaves seen");
}

fn mon(species: &str, moves: &[&str]) -> PokemonState {
    PokemonState::new(species, moves)
}

fn side(left: PokemonState, right: PokemonState) -> SideState {
    let bench = [mon("Milotic", &["Scald", "Protect"]), mon("Garchomp", &["Earthquake", "Protect"])];
    SideState::new(vec![left, right, bench[0].clone(), bench[1].clone()])
}

fn bystander() -> PokemonState {
    mon("Snorlax", &["Protect", "Rest"])
}

#[test]
fn turn_over_tells_a_switch_in_the_middle_of_a_turn_from_one_at_its_end() {
    // Scizor has used U-turn and waits for its replacement; Jolteon's Thunderbolt is still to come.
    let mut leaving = mon("Scizor", &["U-turn", "Protect"]);
    leaving.switch_flag = true;
    leaving.switch_move = "uturn".into();
    let mut state = BattleState {
        turn: 5,
        sides: [side(leaving, bystander()), side(mon("Jolteon", &["Thunderbolt"]), bystander())],
        ..Default::default()
    };
    state.request = Request::Switch;
    state.pending = vec![ActionState::mv(1, 0, "Thunderbolt", 1)];
    let mut b = Battle::from_state(&state).unwrap();
    assert!(!b.turn_over(), "moves are still to run");
    b.choose([[Choice::Switch { to: 2 }, Choice::Pass], [Choice::Pass; 2]]).unwrap();
    assert!(b.turn_over() && b.request == Request::Move && b.turn == 6);
    // Sylveon fainted; the turn has been played out and a replacement is asked for.
    let mut down = mon("Sylveon", &["Moonblast"]);
    down.fainted = true;
    let mut state = BattleState {
        turn: 5,
        sides: [side(down, bystander()), side(bystander(), bystander())],
        ..Default::default()
    };
    state.request = Request::Switch;
    let b = Battle::from_state(&state).unwrap();
    assert!(b.turn_over(), "the end of the turn has been played");
}

#[test]
fn a_cell_plays_a_switch_in_the_middle_of_the_turn_out_and_stops_at_the_end() {
    let pivots: Vec<u16> =
        ["uturn", "voltswitch", "flipturn", "partingshot", "batonpass", "chillyreception", "teleport", "shedtail"]
            .iter()
            .filter_map(|m| move_id(m))
            .collect();
    assert!(pivots.len() >= 4, "the tables know the pivot moves");
    let mut rng = Rng::from_words([9, 9, 9, 9]);
    let (mut found, mut tried, mut planned_cells) = (0, 0, 0);
    let keep = SimOpts { observe: false, keep_states: true, follow: Follow::None, mode: Mode::Move };
    let play_out = SimOpts { observe: false, keep_states: true, follow: Follow::EndOfTurn, mode: Mode::Move };
    'games: for seed in 300..900u16 {
        let Some(g) = root(seed, 1 + seed % 5, &mut rng) else { continue };
        let b = g.battle().unwrap();
        for side in 0..2 {
            for pos in 0..2 {
                let m = b.mon(b.active(side, pos));
                for (slot, ms) in m.moves.iter().enumerate() {
                    if !pivots.contains(&ms.id) {
                        continue;
                    }
                    // That move at a foe's position, with the partner's first legal choice; the foe as it likes.
                    let legal = g.legal(side);
                    let at_a_foe = |a: &&[usize; 2]| a[pos] / 10 == slot && (1..=2).contains(&(a[pos] % 10 / 2));
                    let Some(&mine) = legal.iter().find(at_a_foe) else { continue };
                    let theirs = some(&g, 1 - side, 6, &mut rng);
                    let rows = vec![mine];
                    let cells = all_cells(1, theirs.len(), 4);
                    tried += 1;
                    let stop = simulate(&g, side, &rows, &theirs, &cells, 1, &keep, &pool(8)).unwrap();
                    let mid: Vec<usize> = (0..stop.n)
                        .filter(|&k| stop.summary_of(k)[PHASE] == Phase::Switch as u8 as f32)
                        .filter(|&k| !stop.states[k].turn_over())
                        .collect();
                    if mid.is_empty() {
                        continue;
                    }
                    found += 1;
                    let played = simulate(&g, side, &rows, &theirs, &cells, 1, &play_out, &pool(8)).unwrap();
                    for &k in &mid {
                        let leaf = &played.states[k];
                        assert!(leaf.turn_over(), "played to the end of the turn");
                        let before = stop.states[k].battle().unwrap();
                        let after = leaf.battle().unwrap();
                        assert!(after.ended || after.turn > before.turn || after.request == Request::Switch);
                        assert!(after.ended || after.turn >= before.turn);
                    }
                    // A plan: the pivot brings in the Pokémon the action named, not the dice's choice
                    // (S3.4). Every replacement the mid-turn request allows is planned in turn.
                    let no_plans = vec![NO_PLANS; rows.len()];
                    let same = simulate_planned(&g, side, &rows, &theirs, &no_plans, &[], &cells, 1, &play_out, &pool(8))
                        .unwrap();
                    assert_eq!(same.summary, played.summary, "no plans: the dice's choices, as before");
                    let k0 = mid[0];
                    let options: Vec<usize> = stop.states[k0]
                        .legal(side)
                        .iter()
                        .map(|a| a[pos])
                        .filter(|&a| (40..=45).contains(&a))
                        .collect();
                    assert!(!options.is_empty());
                    for &p in &options {
                        let mut plan = NO_PLANS;
                        plan[pos] = p as u8;
                        let plans = vec![plan; rows.len()];
                        let kept = simulate_planned(&g, side, &rows, &theirs, &plans, &[], &cells, 1, &play_out, &pool(8))
                            .unwrap();
                        for &k in &mid {
                            // (Only where this side is the one asked at that position: the request may be
                            // the other side's pivot, or an Eject Button on it.)
                            let mut mine = NO_PLANS.map(|_| PASS);
                            mine[pos] = p;
                            let asked = stop.states[k].legal(side).iter().any(|a| a[pos] == p);
                            if !asked {
                                continue;
                            }
                            let b = kept.states[k].battle().unwrap();
                            let registered = kept.states[k].rosters()[side][p - 40].species;
                            let there = b.mon(b.active(side, pos)).base_species;
                            assert_eq!(there, registered, "the planned Pokémon came in at position {pos} ({mine:?})");
                            planned_cells += 1;
                        }
                    }
                    if found >= 5 {
                        break 'games;
                    }
                }
            }
        }
    }
    eprintln!("mid-turn switches: {found} positions with one, of {tried} pivot moves tried; {planned_cells} planned cells");
    assert!(found >= 1, "random teams should pivot now and then ({tried} tried)");
    assert!(planned_cells >= 1);
}

#[test]
fn plans_are_checked_and_tell_the_same_action_apart() {
    let mut rng = Rng::from_words([7, 7, 7, 7]);
    let g = roots(1, &[2], 6000, &mut rng).remove(0);
    let (rows, cols) = (g.legal(0), g.legal(1));
    let cells = all_cells(1, 1, 2);
    let bad = vec![[7u8, NO_PLAN]; rows.len()];
    assert!(simulate_planned(&g, 0, &rows, &cols, &bad, &[], &cells, 1, &NO_OBS, &pool(8)).is_err(), "7 is no switch");
    let short = vec![NO_PLANS; 1];
    let err = rows.len() > 1 && simulate_planned(&g, 0, &rows, &cols, &short, &[], &cells, 1, &NO_OBS, &pool(8)).is_err();
    assert!(rows.len() == 1 || err, "a plan per action, or none");
    // The same action with two plans is two rows; the same pair twice is a duplicate.
    let twice = vec![rows[0], rows[0]];
    let plans = vec![[40u8, NO_PLAN], [41u8, NO_PLAN]];
    let s = Search::with_plans(g.clone(), 0, twice.clone(), cols.clone(), plans.clone(), Vec::new(), Budget::default(), 1);
    assert!(s.is_ok(), "{:?}", s.err());
    assert_eq!(s.unwrap().row_plans(), &plans[..]);
    let dup = vec![[40u8, NO_PLAN]; 2];
    assert!(Search::with_plans(g.clone(), 0, twice.clone(), cols.clone(), dup, Vec::new(), Budget::default(), 1).is_err());
    assert!(Search::with_plans(g.clone(), 0, twice, cols.clone(), Vec::new(), Vec::new(), Budget::default(), 1).is_err());
    assert!(Search::new(g.clone(), 0, rows.clone(), cols, Budget::default(), 1).unwrap().row_plans().is_empty());
}

/// The full matrix over `rows × cols` solved once, and the double oracle over the same lists.
fn full_and_oracle(g: &Game, rows: &[[usize; 2]], cols: &[[usize; 2]], seeds: usize, seed: u64) -> (Report, Report) {
    let full = Budget { initial: rows.len().max(cols.len()), max_steps: 0, seeds, ..Budget::default() };
    let oracle = Budget { seeds, ..Budget::default() };
    let mut both = vec![
        Search::new(g.clone(), 0, rows.to_vec(), cols.to_vec(), full, seed).unwrap(),
        Search::new(g.clone(), 0, rows.to_vec(), cols.to_vec(), oracle, seed).unwrap(),
    ];
    run(&mut both, &mut hand, &NO_OBS, &pool(8)).unwrap();
    let (f, o) = (both[0].report(), both[1].report());
    assert_eq!(f.status, Some(Status::Capped(Cap::Steps)));
    assert_eq!(f.rows_in.len() * f.cols_in.len(), rows.len() * cols.len(), "the full matrix was built");
    assert_eq!(o.status, Some(Status::Converged { complete: true }));
    (f, o)
}

/// The double oracle's mix spread over the whole lists, and its exploitability in the full matrix.
fn check_against_full(f: &Report, o: &Report, m: usize, n: usize) -> (f64, f64, f64) {
    let mut a = vec![f32::NAN; m * n];
    for c in &f.cells {
        a[c.row * n + c.col] = c.mean;
    }
    assert!(a.iter().all(|v| v.is_finite()));
    for c in &o.cells {
        assert_eq!(c.mean, a[c.row * n + c.col], "the same dice give the same cell");
    }
    let (mut x, mut y) = (vec![0.0; m], vec![0.0; n]);
    for (k, &r) in o.rows_in.iter().enumerate() {
        x[r] = o.x[k];
    }
    for (k, &c) in o.cols_in.iter().enumerate() {
        y[c] = o.y[k];
    }
    let expl = exploitability(&a, m, n, &x, &y);
    let eps = Budget::default().eps;
    let dv = (o.value - f.value).abs();
    assert!(dv <= eps + o.gap + f.gap + 1e-6, "value {} vs full {} (gaps {} {})", o.value, f.value, o.gap, f.gap);
    assert!(expl <= 2.0 * eps + o.gap + 1e-6, "exploitable by {expl} in the full matrix (gap {})", o.gap);
    (dv, expl, (o.rows_in.len() * o.cols_in.len()) as f64 / (m * n) as f64)
}

#[test]
fn the_double_oracle_matches_the_full_matrix_on_small_positions() {
    let mut rng = Rng::from_words([20, 26, 10, 13]);
    let games = roots(100, &[2, 3, 4, 5, 6, 7, 8], 1000, &mut rng);
    for (width, label) in [(6usize, "six a side"), (20, "twenty a side")] {
        let (mut n_pos, mut worst_dv, mut worst_expl, mut fill) = (0, 0.0f64, 0.0f64, 0.0f64);
        for (k, g) in games.iter().enumerate() {
            let (rows, cols) = (some(g, 0, width, &mut rng), some(g, 1, width, &mut rng));
            if rows.len() < 2 || cols.len() < 2 {
                continue;
            }
            let (f, o) = full_and_oracle(g, &rows, &cols, 16, 500 + k as u64);
            let (dv, expl, frac) = check_against_full(&f, &o, rows.len(), cols.len());
            n_pos += 1;
            worst_dv = worst_dv.max(dv);
            worst_expl = worst_expl.max(expl);
            fill += frac;
        }
        eprintln!(
            "{label}: {n_pos} positions, worst value difference {worst_dv:.2e}, worst exploitability {worst_expl:.2e}, \
             mean share of the matrix built {:.2}",
            fill / n_pos as f64
        );
        assert!(n_pos >= 100 || width > 6, "{label}: only {n_pos} positions");
        if width == 20 {
            assert!((fill / n_pos as f64) < 0.75, "the oracle should stop short of the full matrix");
        }
    }
}

#[test]
fn the_double_oracle_matches_the_full_matrix_on_naturally_small_positions() {
    // Late in random games few joint actions are left: positions with at most eight a side, as they
    // come (at most six a side, the lists above, is rare in random play: one Pokémon left with few targets).
    let mut rng = Rng::from_words([13, 10, 26, 20]);
    let mut found = Vec::new();
    let mut sizes = Vec::new();
    for seed in 2000..3500u16 {
        let Some(mut g) = root(seed, 4, &mut rng) else { continue };
        loop {
            if g.ended() {
                break;
            }
            if g.phase() == Phase::Move {
                let (a, b) = (g.legal(0).len(), g.legal(1).len());
                if (2..=8).contains(&a) && (2..=8).contains(&b) {
                    found.push(g.clone());
                    sizes.push(a * b);
                    break;
                }
            }
            let actions = [g.baseline(0, Baseline::Random, &mut rng), g.baseline(1, Baseline::Random, &mut rng)];
            g.act(actions, [rng.below(65536) as u16, 2, 3, 4]).unwrap();
        }
        if found.len() >= 100 {
            break;
        }
    }
    let (mut worst_dv, mut worst_expl) = (0.0f64, 0.0f64);
    for (k, g) in found.iter().enumerate() {
        let (rows, cols) = (g.legal(0), g.legal(1));
        let (f, o) = full_and_oracle(g, &rows, &cols, 16, 900 + k as u64);
        let (dv, expl, _) = check_against_full(&f, &o, rows.len(), cols.len());
        worst_dv = worst_dv.max(dv);
        worst_expl = worst_expl.max(expl);
    }
    eprintln!(
        "naturally small: {} positions ({} with at most six a side), worst value difference {worst_dv:.2e}, \
         worst exploitability {worst_expl:.2e}",
        found.len(),
        sizes.iter().filter(|&&s| s <= 36).count()
    );
    assert!(found.len() >= 20, "random play reaches small positions: {}", found.len());
}

#[test]
fn the_search_is_deterministic_under_a_seed_and_refuses_bad_lists() {
    let mut rng = Rng::from_words([5, 5, 5, 5]);
    let g = roots(1, &[3], 3000, &mut rng).remove(0);
    let (rows, cols) = (g.legal(0), g.legal(1));
    let twice: Vec<Report> = (0..2)
        .map(|_| {
            let mut s = [Search::new(g.clone(), 0, rows.clone(), cols.clone(), Budget::default(), 42).unwrap()];
            run(&mut s, &mut hand, &NO_OBS, &pool(8)).unwrap();
            let mut r = s[0].report();
            r.elapsed = Duration::ZERO;
            r
        })
        .collect();
    assert_eq!(twice[0], twice[1]);
    assert_eq!(twice[0].status, Some(Status::Converged { complete: true }));
    let scanned = twice[0].scanned;
    assert!(twice[0].leaves >= 16);
    assert_eq!(scanned, [rows.len() - 1, cols.len() - 1], "every action but the first was a candidate");
    let mut dup = rows.clone();
    dup.push(rows[0]);
    assert!(Search::new(g.clone(), 0, dup, cols.clone(), Budget::default(), 1).is_err());
    let pass = vec![[46, 46]];
    let passing = Search::new(g.clone(), 0, pass, cols.clone(), Budget::default(), 1);
    assert!(passing.is_err(), "not legal at a move request");
    assert!(Search::new(g.clone(), 0, Vec::new(), cols.clone(), Budget::default(), 1).is_err());
    let mut s = Search::new(g.clone(), 0, rows.clone(), cols.clone(), Budget::default(), 1).unwrap();
    assert_eq!(s.advance(), Status::NeedCells);
    let w = s.wanted();
    assert_eq!(w.len(), 16, "the first action of each side, sixteen dice");
    assert!(s.supply(&w, &vec![f32::NAN; 16]).is_err(), "no NaN values");
    assert!(s.supply(&w[..1], &[0.0, 0.0]).is_err(), "one value a cell");
}

#[test]
fn caps_windows_and_deepening() {
    let mut rng = Rng::from_words([6, 6, 6, 6]);
    let g = roots(1, &[2], 4000, &mut rng).remove(0);
    let (rows, cols) = (g.legal(0), g.legal(1));
    // Two oracle steps at most, over a window of two candidates.
    let capped = Budget { candidates: Candidates::Next(2), max_steps: 2, ..Budget::default() };
    let mut s = [Search::new(g.clone(), 0, rows.clone(), cols.clone(), capped, 3).unwrap()];
    run(&mut s, &mut hand, &NO_OBS, &pool(8)).unwrap();
    let r = s[0].report();
    let short = matches!(r.status, Some(Status::Capped(Cap::Steps)) | Some(Status::Converged { complete: false }));
    assert!(short, "{:?}", r.status);
    assert!(r.steps <= 2 && r.scanned[0] <= 2 * 3 && r.rows_in.len() <= 3);
    // No time at all: the first solve is the last.
    let timed = Budget { time_cap: Some(Duration::ZERO), ..Budget::default() };
    let mut s = [Search::new(g.clone(), 0, rows.clone(), cols.clone(), timed, 3).unwrap()];
    run(&mut s, &mut hand, &NO_OBS, &pool(8)).unwrap();
    let r = s[0].report();
    assert_eq!((r.status, r.steps, r.rows_in.len(), r.cols_in.len()), (Some(Status::Capped(Cap::Time)), 0, 1, 1));
    assert!(r.value.is_finite());
    // Top-K: the first three of each list solved once.
    let top = Budget { initial: 3, max_steps: 0, ..Budget::default() };
    let mut s = [Search::new(g.clone(), 0, rows.clone(), cols.clone(), top, 3).unwrap()];
    run(&mut s, &mut hand, &NO_OBS, &pool(8)).unwrap();
    let r = s[0].report();
    let got = (r.status, r.rows_in.len(), r.cols_in.len(), r.leaves);
    assert_eq!(got, (Some(Status::Capped(Cap::Steps)), 3, 3, 9 * 16));
    // Deepening a cell asks for more dice for it alone and solves again.
    let mut s = [Search::new(g.clone(), 0, rows.clone(), cols.clone(), Budget::default(), 3).unwrap()];
    run(&mut s, &mut hand, &NO_OBS, &pool(8)).unwrap();
    let before = s[0].report();
    let (r0, c0) = (before.rows_in[0], before.cols_in[0]);
    s[0].deepen(r0, c0, 16).unwrap();
    assert_eq!(s[0].advance(), Status::NeedCells);
    let w = s[0].wanted();
    assert!(w.len() == 16 && w.iter().all(|c| c.row as usize == r0 && c.col as usize == c0 && c.seed >= 16), "{w:?}");
    run(&mut s, &mut hand, &NO_OBS, &pool(8)).unwrap();
    let after = s[0].report();
    let cell = after.cells.iter().find(|c| c.row == r0 && c.col == c0).unwrap();
    assert_eq!(cell.n, 32);
    let values = |r: &Report| r.cells.iter().map(|c| c.n).sum::<usize>();
    assert!(after.status.is_some_and(|st| st.is_done()));
    assert_eq!(after.leaves - before.leaves, values(&after) - values(&before), "every new value was asked for");
}

#[test]
fn the_heuristic_order_puts_the_greedy_pick_first() {
    let mut rng = Rng::from_words([8, 8, 8, 8]);
    for g in roots(5, &[1, 4], 5000, &mut rng) {
        for side in 0..2 {
            let legal = g.legal(side);
            let order = heuristic_order(&g, side, &mut Rng::from_words([1, 2, 3, 4]));
            let mut sorted = order.clone();
            sorted.sort_unstable();
            let mut all = legal.clone();
            all.sort_unstable();
            assert_eq!(sorted, all, "a permutation of the legal list");
            assert_eq!(order[0], g.baseline(side, Baseline::Greedy, &mut Rng::from_words([1, 2, 3, 4])));
            assert_eq!(order, heuristic_order(&g, side, &mut Rng::from_words([1, 2, 3, 4])), "deterministic");
        }
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn a_cell_costs_under_25_microseconds_on_one_thread() {
    let mut rng = Rng::from_words([3, 1, 4, 1]);
    let games = roots(32, &[4], 6000, &mut rng);
    let opts = SimOpts::default();
    let one = pool(1);
    let (mut cells_done, mut seconds) = (0usize, 0.0f64);
    for (k, g) in games.iter().enumerate() {
        let (rows, cols) = (some(g, 0, 8, &mut rng), some(g, 1, 8, &mut rng));
        let cells = all_cells(rows.len(), cols.len(), 16);
        let _ = simulate(g, 0, &rows, &cols, &cells, k as u64, &opts, &one).unwrap();
        let t = Instant::now();
        for rep in 0..3 {
            let _ = simulate(g, 0, &rows, &cols, &cells, k as u64 + rep, &opts, &one).unwrap();
            cells_done += cells.len();
        }
        seconds += t.elapsed().as_secs_f64();
    }
    let us = 1e6 * seconds / cells_done as f64;
    eprintln!("cell: {us:.2} µs on one thread over {cells_done} cells (turn 4, a whole turn, observed)");
    assert!(us <= 25.0, "{us:.1} µs per cell");
    assert_eq!(SUMMARY, 8);
}

/// Drives one search to its end with the hand value.
fn finish(s: Search) -> Report {
    let mut s = [s];
    run(&mut s, &mut hand, &NO_OBS, &pool(8)).unwrap();
    s[0].report()
}

#[test]
fn the_window_slides_over_every_action() {
    // D69: a window of k candidates looks at k actions not looked at before at every oracle step,
    // so it reaches the end of a list of n within ceil((n - 1) / k) windows, and a search that is
    // let run converges to the equilibrium of the whole lists, as `All` does.
    let mut rng = Rng::from_words([20, 26, 10, 17]);
    let games = roots(24, &[2, 3, 4, 5, 6], 5000, &mut rng);
    let (mut checked, mut grew) = (0, 0);
    for k in [2usize, 8] {
        for (n, g) in games.iter().enumerate() {
            let (rows, cols) = (some(g, 0, 20, &mut rng), some(g, 1, 20, &mut rng));
            if rows.len() < 3 || cols.len() < 3 {
                continue;
            }
            let seed = 700 + n as u64;
            let window = |max_steps: usize| Budget { candidates: Candidates::Next(k), max_steps, ..Budget::default() };
            let start = |budget| Search::new(g.clone(), 0, rows.clone(), cols.clone(), budget, seed).unwrap();
            // Every action is looked at (or in the matrix) once enough windows have gone by: a
            // search of `max_steps` s scans s windows (none at 0: the matrix it starts with, solved).
            let longest = rows.len().max(cols.len());
            let windows = (longest - 1).div_ceil(k);
            let r = finish(start(window(windows)));
            for (side, (len, in_)) in [(rows.len(), &r.rows_in), (cols.len(), &r.cols_in)].into_iter().enumerate() {
                let outside = (0..len).filter(|i| !in_.contains(i)).count();
                assert!(r.scanned[side] >= outside, "k {k}: side {side}: {} of {outside} looked at", r.scanned[side]);
            }
            // And what has been looked at grows with every step until nothing is left.
            let mut before = 0;
            for steps in 1..=windows {
                let r = finish(start(window(steps)));
                let total = r.scanned[0] + r.scanned[1];
                let all = rows.len() + cols.len() - r.rows_in.len().min(rows.len()) - r.cols_in.len().min(cols.len());
                if r.status != Some(Status::Capped(Cap::Steps)) {
                    break;
                }
                assert!(total > before || before >= all, "k {k}, {steps} steps: {total} looked at, {before} before");
                before = total;
                grew += 1;
            }
            // Let run, it converges over the whole lists, to the full matrix's equilibrium.
            let (f, _) = full_and_oracle(g, &rows, &cols, 16, seed);
            let o = finish(start(window(usize::MAX)));
            assert_eq!(o.status, Some(Status::Converged { complete: true }), "k {k}");
            check_against_full(&f, &o, rows.len(), cols.len());
            checked += 1;
        }
    }
    eprintln!("windows: {checked} searches converged to the full matrix's value; {grew} steps checked to grow");
    assert!(checked >= 30 && grew >= 60, "{checked} {grew}");
}

#[test]
fn a_value_outside_minus_one_to_one_is_refused_and_a_replaced_one_is_not_a_new_leaf() {
    let mut rng = Rng::from_words([5, 6, 7, 8]);
    let g = roots(1, &[3], 3100, &mut rng).remove(0);
    let (rows, cols) = (g.legal(0), g.legal(1));
    let mut s = Search::new(g.clone(), 0, rows, cols, Budget::default(), 1).unwrap();
    assert_eq!(s.advance(), Status::NeedCells);
    let w = s.wanted();
    for bad in [1.5f32, -1.5, f32::NAN, f32::INFINITY] {
        assert!(s.supply(&w[..1], &[bad]).is_err(), "{bad} is refused");
    }
    s.supply(&w, &vec![1.0; w.len()]).unwrap();
    assert_eq!(s.report().leaves, w.len());
    s.supply(&w[..3], &[-1.0, 0.5, 0.0]).unwrap();
    assert_eq!(s.report().leaves, w.len(), "values given again replace the old ones, as no new leaves");
}

#[test]
fn a_solve_above_its_tolerance_stops_the_oracle() {
    // D84: no candidate is compared against a solution the solver cannot vouch for.
    let mut rng = Rng::from_words([7, 7, 7, 7]);
    let g = roots(1, &[3], 3200, &mut rng).remove(0);
    let (rows, cols) = (g.legal(0), g.legal(1));
    let starved = Budget { initial: 4, solver: SolveOpts { max_iter: 1, tol: 1e-9 }, ..Budget::default() };
    let r = finish(Search::new(g.clone(), 0, rows.clone(), cols.clone(), starved, 2).unwrap());
    assert_eq!((r.status, r.steps, r.scanned), (Some(Status::Capped(Cap::Solver)), 0, [0, 0]));
    assert!(r.gap > 1e-9 && r.iters == 1);
    assert_eq!(SolveOpts::default().max_iter, 20_000);
}

/// A game at a request for replacements at the end of a turn, from random play.
fn a_replacement_request(seed: u16, rng: &mut Rng) -> Option<Game> {
    let mut g = Game::new(teams(seed), true).unwrap();
    while !g.ended() {
        if g.phase() == Phase::Switch && g.turn_over() {
            return Some(g);
        }
        let actions = [g.baseline(0, Baseline::Random, rng), g.baseline(1, Baseline::Random, rng)];
        g.act(actions, [rng.below(65536) as u16, 2, 3, 4]).unwrap();
    }
    None
}

#[test]
fn replacement_and_preview_roots_are_searched_in_their_own_modes() {
    // D82: the default refuses them; in their mode a cell is one step under one set of dice.
    let mut rng = Rng::from_words([8, 2, 8, 2]);
    let one = SimOpts { mode: Mode::Replacement, ..NO_OBS };
    let (mut roots_seen, mut leaves_seen) = (0, 0);
    for seed in 6000..6200u16 {
        let Some(g) = a_replacement_request(seed, &mut rng) else { continue };
        let (rows, cols) = (g.legal(0), g.legal(1));
        let refused = Search::new(g.clone(), 0, rows.clone(), cols.clone(), Budget::default(), 1);
        assert!(refused.is_err(), "a request for replacements is not a move request");
        let sixteen = Budget { mode: Mode::Replacement, ..Budget::default() };
        assert!(Search::new(g.clone(), 0, rows.clone(), cols.clone(), sixteen, 1).is_err(), "one set of dice");
        let cells = all_cells(rows.len(), cols.len(), 1);
        assert!(simulate(&g, 0, &rows, &cols, &cells, 1, &NO_OBS, &pool(8)).is_err(), "nor as cells of a turn");
        let initial = rows.len().max(cols.len());
        let budget = Budget { mode: Mode::Replacement, seeds: 1, initial, max_steps: 0, ..Budget::default() };
        let mut s = Search::new(g.clone(), 0, rows.clone(), cols.clone(), budget, 1).unwrap();
        assert!(s.deepen(0, 0, 1).is_err(), "no second set of dice");
        assert_eq!(s.advance(), Status::NeedCells);
        let w = s.wanted();
        assert!(w.iter().all(|c| c.seed == 0) && w.len() == rows.len() * cols.len());
        let kept = SimOpts { keep_states: true, ..one };
        let leaves = s.simulate(&w, &kept, &pool(8)).unwrap();
        for k in 0..leaves.n {
            // The replacements are in: the next request (a move request, unless a hazard KO'd one).
            let leaf = &leaves.states[k];
            assert!(leaf.ended() || leaf.phase() == Phase::Move || leaf.phase() == Phase::Switch && leaf.turn_over());
            let before = g.battle().unwrap();
            assert!(leaf.ended() || leaf.battle().unwrap().turn >= before.turn);
            leaves_seen += 1;
        }
        s.supply(&w, &hand(&leaves)).unwrap();
        assert!(s.advance().is_done());
        assert_eq!(s.report().mode, Mode::Replacement);
        // The same cell under the same seed is the same leaf; the dice beyond it are not used.
        let again = simulate(&g, 0, &rows, &cols, &w, 1, &one, &pool(8)).unwrap();
        assert_eq!(again.summary, leaves.summary);
        roots_seen += 1;
        if roots_seen >= 10 {
            break;
        }
    }
    assert!(roots_seen >= 5, "{roots_seen} requests for replacements found");
    // Team Preview: refused by default; in its mode each pair of picks starts the battle.
    let g = Game::new(teams(77), true).unwrap();
    let (rows, cols) = (some(&g, 0, 6, &mut rng), some(&g, 1, 6, &mut rng));
    assert!(Search::new(g.clone(), 0, rows.clone(), cols.clone(), Budget::default(), 1).is_err());
    let budget = Budget { mode: Mode::Preview, seeds: 1, initial: 6, max_steps: 0, ..Budget::default() };
    let mut s = Search::new(g.clone(), 0, rows.clone(), cols.clone(), budget, 3).unwrap();
    assert_eq!(s.advance(), Status::NeedCells);
    let w = s.wanted();
    assert_eq!(w.len(), rows.len() * cols.len());
    let kept = SimOpts { keep_states: true, mode: Mode::Preview, ..NO_OBS };
    let leaves = s.simulate(&w, &kept, &pool(8)).unwrap();
    for leaf in &leaves.states {
        let b = leaf.battle().unwrap();
        assert!(b.turn == 1 && leaf.phase() == Phase::Move, "the leads are in, at the first move request");
    }
    s.supply(&w, &hand(&leaves)).unwrap();
    assert!(s.advance().is_done());
    // A move request is not searched as a replacement or as Team Preview.
    let g = roots(1, &[2], 3300, &mut rng).remove(0);
    let (rows, cols) = (g.legal(0), g.legal(1));
    for mode in [Mode::Replacement, Mode::Preview] {
        let budget = Budget { mode, seeds: 1, ..Budget::default() };
        assert!(Search::new(g.clone(), 0, rows.clone(), cols.clone(), budget, 1).is_err(), "{mode:?}");
    }
    eprintln!("modes: {roots_seen} requests for replacements, {leaves_seen} one-step leaves");
}

#[test]
fn a_switch_in_the_middle_of_a_turn_is_not_a_root() {
    let pivots: Vec<u16> =
        ["uturn", "voltswitch", "flipturn", "partingshot"].iter().filter_map(|m| move_id(m)).collect();
    let mut rng = Rng::from_words([4, 4, 4, 4]);
    let keep = SimOpts { observe: false, keep_states: true, follow: Follow::None, mode: Mode::Move };
    for seed in 300..900u16 {
        let Some(g) = root(seed, 1 + seed % 5, &mut rng) else { continue };
        let b = g.battle().unwrap();
        for side in 0..2 {
            for pos in 0..2 {
                let m = b.mon(b.active(side, pos));
                let Some(slot) = m.moves.iter().position(|ms| pivots.contains(&ms.id)) else { continue };
                let legal = g.legal(side);
                let at_a_foe = |a: &&[usize; 2]| a[pos] / 10 == slot && (1..=2).contains(&(a[pos] % 10 / 2));
                let Some(&mine) = legal.iter().find(at_a_foe) else { continue };
                let theirs = some(&g, 1 - side, 4, &mut rng);
                let cells = all_cells(1, theirs.len(), 2);
                let stop = simulate(&g, side, &[mine], &theirs, &cells, 1, &keep, &pool(8)).unwrap();
                let Some(mid) = stop.states.iter().find(|l| l.phase() == Phase::Switch && !l.turn_over()) else {
                    continue;
                };
                for mode in [Mode::Move, Mode::Replacement] {
                    let (rows, cols) = (mid.legal(0), mid.legal(1));
                    let budget = Budget { mode, seeds: if mode == Mode::Move { 16 } else { 1 }, ..Budget::default() };
                    let refused = Search::new(mid.clone(), 0, rows, cols, budget, 1);
                    assert!(refused.is_err(), "{mode:?}: a switch in the middle of a turn has moves still to run");
                }
                return;
            }
        }
    }
    panic!("random teams should pivot now and then");
}
