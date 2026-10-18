//! The training environment (`vgc_engine::env`) and what it shows a model
//! (`vgc_engine::obs`): actions and choices agree, the masks say what is
//! legal, nothing hidden reaches the other side, and many games run at once.

use vgc_engine::data::{ABILITIES, ITEMS, MOVES, SPECIES, ab, ability_id, item_id, move_id, species_id};
use vgc_engine::env::{Baseline, Config, Game, GameState, N_ACTIONS, N_PREVIEW, OBS_M, VecEnv, preview_table};
use vgc_engine::format::{Format, ShowdownSet};
use vgc_engine::obs::{
    ACT_COUNTS, ACT_F, ACT_VOLATILES, ACTIVES, COUNTED_VOLATILES, FIELD_F, HIDDEN_VOLATILES, ID_BASE, ID_NONE,
    ID_UNKNOWN, MON_F, MON_IDS, MONS, MOVE_F, OBS_F, OBS_I, OBS_LAYOUT, Phase, ROSTER, TIMED_SLOTS, TIMED_VOLATILES,
    item_table, layout, move_table, species_table, vocab,
};
use vgc_engine::position::{BattleState, CondState, PokemonState, SideState};
use vgc_engine::rng::Rng;
use vgc_engine::teams::{Pool, PoolTeam, Variation};
use vgc_engine::data::{SlotCond, VolKind};
use vgc_engine::{Choice, PokemonSet, Request};

fn pool(teams: usize, seed: u16) -> Pool {
    let format = Format::current();
    let mut rng = Rng::from_words([seed, 1, 2, 3]);
    let team = |k: usize, rng: &mut Rng| PoolTeam {
        player: format!("player {k}"),
        placing: Some(k as u32 + 1),
        record: String::new(),
        country: String::new(),
        section: String::new(),
        url: String::new(),
        spreads_guessed: false,
        natures_guessed: false,
        team: format.random_team(rng).iter().map(ShowdownSet::from_set).collect(),
    };
    Pool {
        source: String::new(),
        event: "made up".into(),
        format: format.id.clone(),
        teams: (0..teams).map(|k| team(k, &mut rng)).collect(),
    }
}

fn teams(seed: u16) -> [Vec<PokemonSet>; 2] {
    let p = pool(2, seed);
    [p.teams[0].sets().unwrap(), p.teams[1].sets().unwrap()]
}

struct Obs {
    f: Vec<f32>,
    i: Vec<i16>,
    m: Vec<u8>,
}

impl Obs {
    fn of(game: &Game, view: usize) -> Obs {
        let mut o = Obs { f: vec![9.0; OBS_F], i: vec![9; OBS_I], m: vec![9; OBS_M] };
        game.observe(view, &mut o.f, &mut o.i, &mut o.m);
        o
    }
    fn mon_feats(&self, token: usize) -> &[f32] {
        &self.f[FIELD_F + token * MON_F..FIELD_F + (token + 1) * MON_F]
    }
    fn mon_ids(&self, token: usize) -> &[i16] {
        &self.i[token * MON_IDS..(token + 1) * MON_IDS]
    }
    fn act_feats(&self, slot: usize) -> &[f32] {
        let at = FIELD_F + MONS * MON_F + slot * ACT_F;
        &self.f[at..at + ACT_F]
    }
    fn act_ids(&self, slot: usize) -> &[i16] {
        &self.i[MONS * MON_IDS + 2 * slot..MONS * MON_IDS + 2 * slot + 2]
    }
    fn info(&self) -> &[i16] {
        &self.i[OBS_I - 4..]
    }
    fn move_feats(&self, token: usize) -> &[f32] {
        let at = FIELD_F + MONS * MON_F + ACTIVES * ACT_F + token * MOVE_F;
        &self.f[at..at + MOVE_F]
    }
}

/// Plays a game to its end with random legal actions, calling `at` at every decision.
fn play(game: &mut Game, rng: &mut Rng, mut at: impl FnMut(&Game)) {
    loop {
        at(game);
        let mut actions = [[0usize; 2]; 2];
        for (side, a) in actions.iter_mut().enumerate() {
            *a = game.baseline(side, Baseline::Random, rng);
        }
        let seed = [rng.below(65536) as u16, 2, 3, 4];
        game.act(actions, seed).unwrap();
        let b = game.battle().unwrap();
        if b.ended || b.turn > 150 {
            return;
        }
    }
}

#[test]
fn the_layout_adds_up() {
    let (f, i) = layout();
    let end = |parts: &[(&str, usize, Vec<usize>)]| {
        let (_, at, shape) = parts.last().unwrap();
        at + shape.iter().product::<usize>()
    };
    assert_eq!((end(&f), end(&i)), (OBS_F, OBS_I));
    assert_eq!(OBS_M, N_ACTIONS * (N_ACTIONS + 1));
    let [species, items, abilities, moves] = vocab();
    assert_eq!(species, SPECIES.len() + 2);
    assert_eq!((species_table().rows, item_table().rows, move_table().rows), (species, items, moves));
    assert_eq!(abilities, ABILITIES.len() + 2);
    for t in [species_table(), item_table(), move_table()] {
        assert_eq!(t.data.len(), t.rows * t.cols);
        assert!(t.data[..2 * t.cols].iter().all(|&v| v == 0.0), "the rows for nothing and unknown are blank");
        assert!(t.data.iter().all(|v| v.is_finite() && v.abs() <= 4.0));
    }
}

#[test]
fn team_preview_offers_every_way_to_bring_four() {
    let table = preview_table();
    let mut seen = std::collections::HashSet::new();
    for pick in table {
        let mut sorted = *pick;
        sorted.sort_unstable();
        assert!(sorted.windows(2).all(|w| w[0] < w[1]) && sorted[3] < 6, "{pick:?}");
        assert!(pick[0] < pick[1] && pick[2] < pick[3]);
        assert!(seen.insert(*pick));
    }
    assert_eq!(seen.len(), N_PREVIEW);

    // The pick decides who is brought and who leads.
    let mut game = Game::new(teams(5), true).unwrap();
    assert_eq!(game.phase(), Phase::Preview);
    let action = table.iter().position(|p| *p == [1, 4, 0, 5]).unwrap();
    let registered: Vec<u16> = game.rosters()[0].iter().map(|s| s.species).collect();
    game.act([[action, 0], [0, 0]], [1, 2, 3, 4]).unwrap();
    let b = game.battle().unwrap();
    let brought: Vec<u16> = (0..4).map(|p| b.mon(b.active_at(0, p)).species).collect();
    assert_eq!(brought, [registered[1], registered[4], registered[0], registered[5]]);
    assert_eq!(b.sides[0].n, 4);
    assert!(b.open_team_sheets());
}

#[test]
fn actions_and_choices_agree_and_the_masks_say_what_is_legal() {
    let mut rng = Rng::from_words([7, 7, 7, 7]);
    let (mut decisions, mut switches, mut megas, mut forced) = (0, 0, 0, 0);
    for seed in 0..60 {
        let mut game = Game::new(teams(100 + seed), seed % 2 == 0).unwrap();
        play(&mut game, &mut rng, |game| {
            let Some(b) = game.battle() else { return };
            decisions += 1;
            for side in 0..2 {
                let joint = b.joint_choices(side);
                let legal = game.legal(side);
                assert_eq!(joint.len(), legal.len());
                let mut mask = vec![0u8; OBS_M];
                assert_eq!(game.masks(side, &mut mask), joint.len());
                for (c, a) in joint.iter().zip(&legal) {
                    for pos in 0..2 {
                        // A number names one choice, and that choice has that number.
                        assert_eq!(game.choice_of(side, a[pos]), Some(c[pos]), "{:?} as {}", c[pos], a[pos]);
                        assert!(a[pos] < N_ACTIONS);
                    }
                    assert_eq!(mask[a[0]], 1);
                    assert_eq!(mask[N_ACTIONS * (1 + a[0]) + a[1]], 1);
                    switches += (40..46).contains(&a[0]) as usize;
                    megas += (a[0] < 40 && a[0] % 2 == 1) as usize;
                }
                // And nothing else is marked legal.
                let firsts: std::collections::HashSet<usize> = legal.iter().map(|a| a[0]).collect();
                assert_eq!(mask[..N_ACTIONS].iter().filter(|&&x| x == 1).count(), firsts.len());
                assert_eq!(mask[N_ACTIONS..].iter().filter(|&&x| x == 1).count(), legal.len());
                forced += (b.request == Request::Switch && legal.len() == 1) as usize;
            }
        });
    }
    assert!(decisions > 500 && switches > 500 && megas > 100 && forced > 20, "{decisions} {switches} {megas} {forced}");
}

/// The id a name has in an observation.
fn named<T>(name: &str, find: impl Fn(&str) -> Option<T>) -> i16
where
    T: Into<i64>,
{
    find(name).unwrap_or_else(|| panic!("no {name}")).into() as i16 + ID_BASE
}

#[test]
fn the_other_side_is_given_as_the_battle_has_shown_it() {
    let mut rng = Rng::from_words([3, 1, 4, 1]);
    let (mut checked, mut unknown_items, mut known_items, mut lost) = (0, 0, 0, 0);
    let (mut disabled, mut active_seen) = (0, 0);
    for seed in 0..80 {
        let open = seed % 2 == 1;
        let mut game = Game::new(teams(300 + seed), open).unwrap();
        play(&mut game, &mut rng, |game| {
            let Some(b) = game.battle() else { return };
            for view in 0..2 {
                let o = Obs::of(game, view);
                assert!(o.f.iter().all(|v| v.is_finite() && (-1.01..=4.0).contains(v)), "a feature out of range");
                let vocab = vocab();
                let shown = b.shown(1 - view);
                let on_show = shown.active.iter().flatten().chain(&shown.bench);
                for (j, listed) in shown.roster.iter().enumerate() {
                    let ids = o.mon_ids(ROSTER + j);
                    let feats = o.mon_feats(ROSTER + j);
                    let mon = on_show.clone().find(|m| m.listed == Some(j as u8));
                    let sheet = listed.sheet.as_ref();
                    assert_eq!(sheet.is_some(), open);
                    let species = mon.map_or(listed.species.as_str(), |m| m.species.as_str());
                    assert_eq!(ids[0], named(species, species_id), "species of {j}");
                    // Item: what the battle showed, else what the sheet says, else unknown.
                    let (item, was) = match (mon.and_then(|m| m.item.as_deref()), sheet) {
                        (Some(""), _) => (named("", |_| Some(0u16)), mon.unwrap().item_lost.as_str()),
                        (Some(held), _) => (named(held, item_id), ""),
                        (None, Some(s)) if s.item.is_empty() => (ID_BASE, ""),
                        (None, Some(s)) => (named(&s.item, item_id), ""),
                        (None, None) => (ID_UNKNOWN, ""),
                    };
                    assert_eq!(ids[1], item, "item of {j}");
                    assert_eq!(ids[2], if was.is_empty() { ID_NONE } else { named(was, item_id) });
                    let ability = match (mon.and_then(|m| m.ability.as_deref()), sheet) {
                        (Some(""), _) => ab::NOABILITY as i16 + ID_BASE,
                        (Some(a), _) => named(a, ability_id),
                        (None, Some(s)) => named(&s.ability, ability_id),
                        (None, None) => ID_UNKNOWN,
                    };
                    assert_eq!(ids[3], ability, "ability of {j}");
                    let moves: Vec<i16> = match (sheet, mon) {
                        (Some(s), _) => s.moves.iter().map(|m| named(m, move_id)).collect(),
                        // What it has used, then what it has been worked out to know.
                        (None, Some(m)) => {
                            let mut known: Vec<&String> = m.moves.iter().take(4).collect();
                            for told in &listed.told {
                                if !known.contains(&told) && known.len() < 4 {
                                    known.push(told);
                                }
                            }
                            known.iter().map(|m| named(m, move_id)).collect()
                        }
                        (None, None) => Vec::new(),
                    };
                    let rest = if open { ID_NONE } else { ID_UNKNOWN };
                    for k in 0..4 {
                        assert_eq!(ids[4 + k], moves.get(k).copied().unwrap_or(rest), "move {k} of {j}");
                    }
                    for (k, &id) in ids.iter().enumerate() {
                        let size = [vocab[0], vocab[1], vocab[1], vocab[2], vocab[3], vocab[3], vocab[3], vocab[3]][k];
                        assert!((0..size as i16).contains(&id));
                    }
                    // Seen, on the field, fainted, HP as the bar shows it, status.
                    let active = shown.active.iter().flatten().any(|m| m.listed == Some(j as u8));
                    assert_eq!((feats[0], feats[1]), (1.0, 0.0));
                    assert_eq!(feats[4], mon.is_some() as u8 as f32, "seen");
                    assert_eq!(feats[5], active as u8 as f32, "on the field");
                    assert_eq!(feats[6], mon.is_some_and(|m| m.fainted) as u8 as f32, "fainted");
                    assert_eq!(feats[7], mon.map_or(1.0, |m| m.hp as f32 / 100.0), "hp of {j}: {mon:?}\n{shown:#?}");
                    let status = mon.map_or(0, |m| {
                        ["", "brn", "par", "psn", "tox", "slp", "frz"].iter().position(|s| *s == m.status).unwrap()
                    });
                    assert_eq!(feats[8 + status], 1.0, "status");
                    // Stats are never given.
                    assert!(feats[16..23].iter().all(|&v| v == 0.0));
                    checked += 1;
                    unknown_items += (ids[1] == ID_UNKNOWN) as usize;
                    known_items += (ids[1] > ID_BASE && !open) as usize;
                    lost += (ids[2] != ID_NONE) as usize;
                }
                // The positions on the field point at the Pokémon standing there.
                for slot in 0..ACTIVES {
                    let token = o.act_ids(slot)[0];
                    if token == 0 {
                        assert!(o.act_feats(slot).iter().all(|&v| v == 0.0));
                        continue;
                    }
                    let token = token as usize - 1;
                    assert_eq!(token < ROSTER, slot < 2, "a side's positions hold its own Pokémon");
                    assert_eq!(o.mon_feats(token)[5], 1.0, "the Pokémon in a position is on the field");
                    active_seen += 1;
                    // What the game keeps to itself is given to neither side.
                    for kind in HIDDEN_VOLATILES {
                        assert_eq!(o.act_feats(slot)[ACT_VOLATILES + kind as usize], 0.0, "{kind:?} is given");
                    }
                    // What a side is told of its own instead: a move it cannot pick.
                    if slot < 2 && o.info()[0] == 1 {
                        disabled += (0..4).filter(|k| o.move_feats(4 * slot + k)[2] == 1.0).count();
                    }
                }
                let info = o.info();
                assert_eq!(info[0], game.phase() as i16);
                assert_eq!(info[1] as usize, game.legal(view).len());
            }
        });
    }
    // The check saw every case it is there for.
    assert!(
        checked > 10_000 && unknown_items > 2_000 && known_items > 300 && lost > 300,
        "{checked} {unknown_items} {known_items} {lost}"
    );
    assert!(active_seen > 5_000 && disabled > 50, "{active_seen} {disabled}");
    let _ = (ITEMS.len(), MOVES.len());
}

/// Two opponents that differ only in what Team Preview does not show give
/// the same observation at Team Preview with closed sheets, and a different
/// one with open sheets.
#[test]
fn team_preview_shows_species_and_no_more_unless_sheets_are_open() {
    let [ours, theirs] = teams(42);
    // The same six species with every hidden thing changed.
    let format = Format::current();
    let mut rng = Rng::from_words([9, 9, 9, 9]);
    let mut other = theirs.clone();
    for set in &mut other {
        let rule = format.rule(set.species).unwrap();
        for _ in 0..50 {
            let again = format.random_set(rule, &mut rng);
            if again.species == set.species && again.gender == set.gender && again != *set {
                *set = again;
                break;
            }
        }
    }
    assert_ne!(other, theirs);
    for open in [false, true] {
        let a = Obs::of(&Game::new([ours.clone(), theirs.clone()], open).unwrap(), 0);
        let b = Obs::of(&Game::new([ours.clone(), other.clone()], open).unwrap(), 0);
        assert_eq!(a.f == b.f && a.i == b.i, !open, "open sheets: {open}");
        // Its own team it sees in full either way.
        for j in 0..ROSTER {
            assert!([0, 1, 3, 4].iter().all(|&k| a.mon_ids(j)[k] >= ID_BASE));
            assert_eq!(a.mon_feats(j)[22], 1.0, "its own stats are known");
            let unknown = a.mon_ids(ROSTER + j)[1] == ID_UNKNOWN;
            assert_eq!(unknown, !open);
        }
        assert_eq!(a.info(), [Phase::Preview as i16, N_PREVIEW as i16, 0, 1]);
    }
}

#[test]
fn many_games_run_at_once_and_start_over() {
    let pool = pool(12, 1);
    let config = Config { envs: 64, seed: 5, max_turns: 60, threads: 2, ..Config::default() };
    let mut env = VecEnv::new(&pool, config.clone()).unwrap();
    let n = env.len();
    let (mut f, mut i, mut m) = (vec![0f32; 2 * n * OBS_F], vec![0i16; 2 * n * OBS_I], vec![0u8; 2 * n * OBS_M]);
    let (mut reward, mut done, mut actions) = (vec![0f32; 2 * n], vec![0u8; n], vec![0i32; 4 * n]);
    env.observe(&mut f, &mut i, &mut m);
    assert!((0..2 * n).all(|k| i[(k + 1) * OBS_I - 4] == Phase::Preview as i16));
    let (mut finished, mut total) = (0u64, 0.0f32);
    for step in 0..400 {
        env.baseline(if step % 2 == 0 { Baseline::Random } else { Baseline::Greedy }, &mut actions);
        // What a baseline gives is legal by the mask handed out with the observation.
        for k in 0..2 * n {
            let (info, mask) = (&i[(k + 1) * OBS_I - 4..(k + 1) * OBS_I], &m[k * OBS_M..(k + 1) * OBS_M]);
            let (a0, a1) = (actions[2 * k] as usize, actions[2 * k + 1] as usize);
            if info[0] == Phase::Preview as i16 {
                assert!(a0 < N_PREVIEW);
            } else {
                assert!(mask[a0] == 1 && mask[N_ACTIONS * (1 + a0) + a1] == 1, "game {} step {step}", k / 2);
            }
        }
        env.step(&actions, &mut f, &mut i, &mut m, &mut reward, &mut done).unwrap();
        for g in 0..n {
            let r = &reward[2 * g..2 * g + 2];
            if done[g] == 1 {
                finished += 1;
                assert!(r == [1.0, -1.0] || r == [-1.0, 1.0] || r == [0.0, 0.0]);
                // A new game has taken its place.
                assert_eq!(i[(2 * g + 1) * OBS_I - 4], Phase::Preview as i16);
            } else {
                assert_eq!(r, [0.0, 0.0]);
            }
            total += r[0] + r[1];
        }
    }
    let stats = env.stats();
    assert_eq!((stats.games, stats.decisions), (finished, 400 * n as u64));
    assert!(finished > 300 && total == 0.0, "{finished} games");
    assert!(stats.ties * 10 < stats.games, "{} of {} games tied or ran out of turns", stats.ties, stats.games);

    // The same seed plays the same games; an illegal action is refused.
    let mut run = |steps: usize| {
        let mut env = VecEnv::new(&pool, config.clone()).unwrap();
        let (mut f, mut i, mut m) = (vec![0f32; 2 * n * OBS_F], vec![0i16; 2 * n * OBS_I], vec![0u8; 2 * n * OBS_M]);
        for _ in 0..steps {
            env.baseline(Baseline::Random, &mut actions);
            env.step(&actions, &mut f, &mut i, &mut m, &mut reward, &mut done).unwrap();
        }
        (f, i, m)
    };
    assert!(run(50) == run(50));
    let mut env = VecEnv::new(&pool, config).unwrap();
    env.baseline(Baseline::Random, &mut actions);
    env.step(&actions, &mut f, &mut i, &mut m, &mut reward, &mut done).unwrap();
    actions.fill(46);
    assert!(env.step(&actions, &mut f, &mut i, &mut m, &mut reward, &mut done).is_err());
}

/// The share of games the first kind of player wins against the second, over `steps` steps of 128 games.
fn duel(first: Baseline, second: Baseline, steps: usize) -> (usize, usize) {
    let pool = pool(20, 3);
    let config = Config { envs: 128, seed: 9, variation: Variation::NONE, threads: 2, ..Config::default() };
    let mut env = VecEnv::new(&pool, config).unwrap();
    let n = env.len();
    let (mut f, mut i, mut m) = (vec![0f32; 2 * n * OBS_F], vec![0i16; 2 * n * OBS_I], vec![0u8; 2 * n * OBS_M]);
    let (mut reward, mut done) = (vec![0f32; 2 * n], vec![0u8; n]);
    let (mut ours, mut theirs) = (vec![0i32; 4 * n], vec![0i32; 4 * n]);
    let (mut wins, mut games) = (0, 0);
    for _ in 0..steps {
        env.baseline(first, &mut ours);
        env.baseline(second, &mut theirs);
        // The first player takes the first side of even games and the second side of odd ones.
        for g in 0..n {
            let at = 4 * g + 2 * (g % 2);
            theirs[at..at + 2].copy_from_slice(&ours[at..at + 2]);
        }
        env.step(&theirs, &mut f, &mut i, &mut m, &mut reward, &mut done).unwrap();
        for g in 0..n {
            if done[g] == 1 {
                games += 1;
                wins += (reward[2 * g + g % 2] > 0.0) as usize;
            }
        }
    }
    (wins, games)
}

#[test]
fn each_scripted_player_beats_the_one_below_it() {
    let (wins, games) = duel(Baseline::Greedy, Baseline::Random, 600);
    assert!(games > 1000 && wins * 100 > games * 80, "greedy won {wins} of {games} against random");
    let (wins, games) = duel(Baseline::Lookahead, Baseline::Greedy, 300);
    println!("look-ahead won {wins} of {games} against greedy");
    assert!(games > 1000 && wins * 100 > games * 60, "look-ahead won {wins} of {games} against greedy");
}

/// Rain from a Pelipper holding a Damp Rock lasts eight turns for five, and
/// nothing shows the rock. Until the fifth turn is over, the other side's
/// view is the same with the rock and without: it is told how long the rain
/// has been up, not how long it has left.
#[test]
fn a_hidden_item_that_extends_the_weather_does_not_show_in_its_timer() {
    let set = |species: &str, ability: &str, item: &str| {
        PokemonSet::from_names(species, &["Protect"], "Hardy", [0; 6])
            .unwrap()
            .ability(ability)
            .unwrap()
            .item(item)
            .unwrap()
    };
    let rest = |first: PokemonSet| {
        vec![
            first,
            set("Snorlax", "Thick Fat", ""),
            set("Garchomp", "Rough Skin", ""),
            set("Milotic", "Competitive", ""),
            set("Sylveon", "Pixilate", ""),
            set("Arcanine", "Intimidate", ""),
        ]
    };
    let game = |rock: bool| {
        let pelipper = set("Pelipper", "Drizzle", if rock { "Damp Rock" } else { "" });
        let mut g = Game::new([rest(pelipper), rest(set("Kingambit", "Defiant", ""))], false).unwrap();
        g.act([[0, 0], [0, 0]], [4, 3, 2, 1]).unwrap();
        g
    };
    let (mut with, mut without) = (game(true), game(false));
    // Where the field's features say how long the weather has been up.
    let elapsed = 3 + 1 + 1 + 5;
    for turn in 1..=7 {
        let (a, b) = (Obs::of(&with, 1), Obs::of(&without, 1));
        let raining = |o: &Obs| o.f[5 + 1] == 1.0;
        assert!(raining(&a), "turn {turn}");
        assert_eq!(a.f[elapsed], (turn - 1) as f32 / 8.0, "turn {turn}");
        if turn <= 5 {
            assert!(a.f == b.f && a.i == b.i, "the rock shows on turn {turn}");
        } else {
            // Without the rock the rain has stopped, which everyone sees.
            assert!(!raining(&b), "turn {turn}");
        }
        // The side holding the rock knows what it holds.
        assert_ne!(Obs::of(&with, 0).mon_ids(0)[1], Obs::of(&without, 0).mon_ids(0)[1]);
        for g in [&mut with, &mut without] {
            g.act([[0, 0], [0, 0]], [1, 1, 1, 1]).unwrap();
        }
    }
}

/// With closed sheets a player works some things out for itself. A move of
/// its own that it is told it cannot choose, for no reason on show, has been
/// sealed by the other side's Imprison: so whoever has Imprison up knows that
/// move, used or not. Showdown tells this to all but the last of a side to
/// choose, and so does the simulator.
#[test]
fn a_move_sealed_by_imprison_is_known_to_be_the_sealers() {
    let set = |species: &str, moves: &[&str]| PokemonSet::from_names(species, moves, "Hardy", [0; 6]).unwrap();
    let team = |first: PokemonSet, second: PokemonSet| {
        vec![
            first,
            second,
            set("Snorlax", &["Calm Mind"]),
            set("Arcanine", &["Calm Mind"]),
            set("Kingambit", &["Calm Mind"]),
            set("Volcarona", &["Calm Mind"]),
        ]
    };
    let mv = |slot: u8| Choice::Move { slot, target: 0, mega: false };
    // Ninetales seals Protect. `first`: whether it is our first Pokémon or our last that has Protect.
    let game = |first: bool, open: bool| {
        let with = set("Milotic", &["Calm Mind", "Protect"]);
        let without = set("Sylveon", &["Calm Mind", "Hyper Voice"]);
        let ours = if first { team(with, without) } else { team(without, with) };
        let theirs = team(set("Ninetales", &["Imprison", "Protect", "Flamethrower"]), set("Garchomp", &["Calm Mind"]));
        let mut g = Game::new([ours, theirs], open).unwrap();
        g.start([&[0, 1, 2, 3], &[0, 1, 2, 3]], [1, 2, 3, 4]).unwrap();
        g.choose([[mv(0), mv(0)], [mv(0), mv(0)]]).unwrap();
        g
    };
    let (imprison, protect) = (named("imprison", move_id), named("protect", move_id));
    let ninetales = |g: &Game| Obs::of(g, 0).mon_ids(ROSTER)[4..8].to_vec();

    // Our first Pokémon is told its Protect is barred: Ninetales has Protect.
    let mut g = game(true, false);
    assert_eq!(ninetales(&g), [imprison, protect, ID_UNKNOWN, ID_UNKNOWN]);
    assert_eq!(g.battle().unwrap().shown(1).roster[0].told, ["protect"]);
    // It stays known when Ninetales goes, and whatever Ninetales shows later joins it.
    g.choose([[mv(0), mv(0)], [Choice::Switch { to: 2 }, mv(0)]]).unwrap();
    assert_eq!(ninetales(&g), [imprison, protect, ID_UNKNOWN, ID_UNKNOWN]);
    // Nothing of the kind is noted about our own side, whose Imprison is nobody's.
    assert!(g.battle().unwrap().shown(0).roster.iter().all(|l| l.told.is_empty()));

    // Our last Pokémon to choose is not told what bars it, and nothing is worked out.
    let g = game(false, false);
    assert_eq!(ninetales(&g), [imprison, ID_UNKNOWN, ID_UNKNOWN, ID_UNKNOWN]);
    // ...though the move is barred all the same.
    let b = g.battle().unwrap();
    assert!(!b.legal_choices(0, 1).contains(&mv(1)) && b.legal_choices(0, 1).contains(&mv(0)));

    // With open sheets there is nothing to work out: the sheet has the moves.
    let g = game(true, true);
    assert!(g.battle().unwrap().shown(1).roster[0].told.is_empty());
    assert_eq!(ninetales(&g)[..3], [imprison, protect, named("flamethrower", move_id)]);

    // A move stopped in the act says the same, and says it to everyone: Milotic is slower
    // than Ninetales, whose Imprison is up by the time Milotic tries the Calm Mind they share.
    let ours = team(set("Milotic", &["Calm Mind", "Surf"]), set("Sylveon", &["Hyper Voice"]));
    let theirs = team(set("Ninetales", &["Imprison", "Calm Mind"]), set("Garchomp", &["Swords Dance"]));
    let mut g = Game::new([ours, theirs], false).unwrap();
    g.start([&[0, 1, 2, 3], &[0, 1, 2, 3]], [1, 2, 3, 4]).unwrap();
    g.choose([[mv(0), mv(0)], [mv(0), mv(0)]]).unwrap();
    assert_eq!(ninetales(&g), [imprison, named("calmmind", move_id), ID_UNKNOWN, ID_UNKNOWN]);
}

/// The types one side's observation gives a position (its own two, then the other side's).
/// (They come after whether a Pokémon is there, its HP, its status and its seven stat stages.)
fn types_shown(g: &Game, view: usize, slot: usize) -> Vec<usize> {
    let o = Obs::of(g, view);
    (0..18).filter(|&t| o.act_feats(slot)[16 + t] == 1.0).collect()
}

fn types_of(list: &[vgc_engine::data::Type]) -> Vec<usize> {
    list.iter().map(|&t| t as usize).collect()
}

#[test]
fn a_type_added_to_a_disguised_pokemon_does_not_give_it_away() {
    use vgc_engine::data::Type;
    let set = |species: &str, moves: &[&str]| PokemonSet::from_names(species, moves, "Hardy", [0; 6]).unwrap();
    // Zoroark-Hisui (Normal/Ghost) leads, looking like the last of the four brought: Garchomp (Dragon/Ground).
    let ours = vec![
        set("Trevenant", &["Forest's Curse", "Calm Mind"]),
        set("Milotic", &["Soak", "Calm Mind"]),
        set("Snorlax", &["Calm Mind"]),
        set("Arcanine", &["Calm Mind"]),
        set("Kingambit", &["Calm Mind"]),
        set("Volcarona", &["Calm Mind"]),
    ];
    let theirs = vec![
        set("Zoroark-Hisui", &["Calm Mind"]).ability("Illusion").unwrap(),
        set("Sylveon", &["Calm Mind"]),
        set("Volcarona", &["Calm Mind"]),
        set("Garchomp", &["Swords Dance"]),
        set("Snorlax", &["Calm Mind"]),
        set("Arcanine", &["Calm Mind"]),
    ];
    let mut g = Game::new([ours, theirs], false).unwrap();
    g.start([&[0, 1, 2, 3], &[0, 1, 2, 3]], [1, 2, 3, 4]).unwrap();
    let mv = |slot: u8, target: i8| Choice::Move { slot, target, mega: false };
    let (types, of) = (types_shown, types_of);
    // (Which of the six listed the Pokémon in their first place is taken to be: the fourth.)
    let taken_for = |g: &Game| Obs::of(g, 0).i[MONS * MON_IDS + 2 * 2] as usize - 1 - ROSTER;
    assert_eq!(taken_for(&g), 3);
    assert_eq!(types(&g, 0, 2), of(&[Type::Ground, Type::Dragon]));
    assert_eq!(types(&g, 1, 0), of(&[Type::Normal, Type::Ghost]));

    // Forest's Curse: "Grass was added" is all anyone is told. The other side goes on seeing
    // Garchomp's types, with Grass; its own side sees what it is.
    g.choose([[mv(0, 1), mv(1, 0)], [mv(0, 0), mv(0, 0)]]).unwrap();
    assert_eq!(types(&g, 0, 2), of(&[Type::Ground, Type::Grass, Type::Dragon]));
    assert_eq!(types(&g, 1, 0), of(&[Type::Normal, Type::Ghost, Type::Grass]));

    // Soak: "it became the Water type" is said in so many words, and Water is what everyone sees.
    g.choose([[mv(1, 0), mv(0, 1)], [mv(0, 0), mv(0, 0)]]).unwrap();
    assert_eq!(types(&g, 0, 2), of(&[Type::Water]));
    assert_eq!(types(&g, 1, 0), of(&[Type::Water]));
    // (It is still taken for Garchomp.)
    assert_eq!(taken_for(&g), 3);
}

#[test]
fn after_reflect_type_each_side_has_the_types_it_was_told() {
    use vgc_engine::data::Type;
    let set = |species: &str, moves: &[&str]| PokemonSet::from_names(species, moves, "Hardy", [0; 6]).unwrap();
    let ours = vec![
        set("Starmie", &["Reflect Type", "Calm Mind"]),
        set("Scizor", &["U-turn"]),
        set("Snorlax", &["Calm Mind"]),
        set("Arcanine", &["Calm Mind"]),
        set("Kingambit", &["Calm Mind"]),
        set("Volcarona", &["Calm Mind"]),
    ];
    // Zoroark-Hisui (Normal/Ghost) leads, looking like the last of the four brought: Garchomp (Dragon/Ground).
    let theirs = vec![
        set("Zoroark-Hisui", &["Calm Mind"]).ability("Illusion").unwrap(),
        set("Stunfisk", &["Calm Mind", "Reflect Type"]),
        set("Volcarona", &["Calm Mind"]),
        set("Garchomp", &["Swords Dance"]),
        set("Snorlax", &["Calm Mind"]),
        set("Arcanine", &["Calm Mind"]),
    ];
    let mut g = Game::new([ours, theirs], false).unwrap();
    g.start([&[0, 1, 2, 3], &[0, 1, 2, 3]], [1, 2, 3, 4]).unwrap();
    let mv = |slot: u8, target: i8| Choice::Move { slot, target, mega: false };
    let (types, of) = (types_shown, types_of);
    let (garchomp, zoroark) = (of(&[Type::Ground, Type::Dragon]), of(&[Type::Normal, Type::Ghost]));
    // (For side 0, Starmie is its own first position; for side 1, the other side's first.)
    let starmie = |g: &Game| [types(g, 0, 0), types(g, 1, 2)];

    // Starmie copies the types of "Garchomp". The log says whose types, not which: its own
    // side takes them for Garchomp's; the other side knows its Zoroark. Scizor's U-turn then
    // stops the turn for a replacement, before anything more is said.
    g.choose([[mv(0, 1), mv(0, 2)], [mv(0, 0), mv(0, 0)]]).unwrap();
    assert_eq!(g.phase(), Phase::Switch);
    assert_eq!(starmie(&g), [garchomp.clone(), zoroark.clone()]);
    // At the end of the turn Showdown says what Starmie's types have become.
    g.choose([[Choice::Pass, Choice::Switch { to: 2 }], [Choice::Pass, Choice::Pass]]).unwrap();
    assert_eq!(g.phase(), Phase::Move);
    assert_eq!(starmie(&g), [zoroark.clone(), zoroark.clone()]);
    // Copying it again changes nothing, so nothing is said: they are what they were.
    g.choose([[mv(0, 1), mv(0, 0)], [mv(0, 0), mv(0, 0)]]).unwrap();
    assert_eq!(starmie(&g), [zoroark.clone(), zoroark.clone()]);

    // Stunfisk copies its partner, the Zoroark. Its own side knows what it has taken; the
    // other side sees it take Garchomp's types, and is never told otherwise.
    let stunfisk = |g: &Game| [types(g, 0, 3), types(g, 1, 1)];
    assert_eq!(stunfisk(&g), [of(&[Type::Ground, Type::Electric]), of(&[Type::Ground, Type::Electric])]);
    g.choose([[mv(1, 0), mv(0, 0)], [mv(0, 0), mv(1, -1)]]).unwrap();
    assert_eq!(stunfisk(&g), [garchomp.clone(), zoroark.clone()]);
    g.choose([[mv(1, 0), mv(0, 0)], [mv(0, 0), mv(0, 0)]]).unwrap();
    assert_eq!(stunfisk(&g), [garchomp, zoroark]);
}

/// A game written down and taken up again (`Game::to_state`, `Game::from_state`)
/// shows both sides the same and plays on the same, at every decision of
/// random games, through JSON. The bookkeeping that goes with the battle (how
/// long conditions have been up, what each side can tell of the other's
/// Speed, the public counts of hidden-length effects) comes back with it.
#[test]
fn a_game_taken_up_from_its_state_observes_and_plays_the_same() {
    let mut rng = Rng::from_words([7, 7, 7, 7]);
    let (mut decisions, mut with_timers, mut with_counts) = (0, 0, 0);
    for seed in 0..40 {
        let open = seed % 3 != 0;
        let mut game = Game::new(teams(700 + seed), open).unwrap();
        loop {
            let st = game.to_state();
            assert_eq!(GameState::from_json(&st.to_json()).unwrap(), st, "through JSON and back");
            let mut again = Game::from_state(&st).unwrap();
            assert_eq!(again.phase(), game.phase());
            for view in 0..2 {
                let (a, b) = (Obs::of(&game, view), Obs::of(&again, view));
                assert!(a.f == b.f && a.i == b.i && a.m == b.m, "seed {seed}, view {view}: the observation changed");
            }
            if let Some(t) = &st.timers {
                with_timers += (t.weather.is_some() || t.terrain.is_some()) as u32;
            }
            if let Some(c) = &st.counts {
                let any = |a: &[[u8; 6]; 2]| a.iter().flatten().any(|&n| n > 0);
                with_counts += (any(&c.sleep) || any(&c.confusion)) as u32;
            }
            decisions += 1;
            let mut actions = [[0usize; 2]; 2];
            for (side, a) in actions.iter_mut().enumerate() {
                *a = game.baseline(side, Baseline::Random, &mut rng);
            }
            let seed = [rng.below(65536) as u16, 2, 3, 4];
            game.act(actions, seed).unwrap();
            again.act(actions, seed).unwrap();
            assert_eq!(game.battle().unwrap().position_diff(again.battle().unwrap()), None, "after acting");
            for view in 0..2 {
                let (a, b) = (Obs::of(&game, view), Obs::of(&again, view));
                assert!(a.f == b.f && a.i == b.i && a.m == b.m, "seed {seed:?}, view {view}: diverged after a step");
            }
            let b = game.battle().unwrap();
            if b.ended || b.turn > 60 {
                break;
            }
        }
    }
    assert!(decisions > 500, "{decisions} decisions");
    assert!(with_timers > 20 && with_counts > 5, "{with_timers} positions with a timer, {with_counts} with a count");
}

/// Bookkeeping left out of a game state is taken as if nothing had been seen
/// before: the game still runs, and a state with no battle is at Team Preview.
#[test]
fn bookkeeping_left_out_of_a_game_state_is_taken_as_fresh() {
    let mut rng = Rng::from_words([9, 9, 9, 9]);
    let mut game = Game::new(teams(55), true).unwrap();
    while game.battle().map(|b| b.turn).unwrap_or(0) < 4 {
        let actions = [game.baseline(0, Baseline::Random, &mut rng), game.baseline(1, Baseline::Random, &mut rng)];
        game.act(actions, [rng.below(65536) as u16, 1, 1, 1]).unwrap();
        assert!(!game.ended());
    }
    let full = game.to_state();
    let bare = GameState { timers: None, speeds: None, counts: None, ..full.clone() };
    let mut fresh = Game::from_state(&bare).unwrap();
    assert_eq!(fresh.phase(), Phase::Move);
    assert_eq!(fresh.counts(), &vgc_engine::env::Counts::default());
    // Everything the sheets allow: no narrower than what the game has worked out.
    for side in 0..2 {
        for entry in 0..ROSTER {
            let species = full.rosters[side][entry].to_set().unwrap().species;
            let (lo, hi) = fresh.speeds().belief(side, entry).range(species);
            let (glo, ghi) = game.speeds().belief(side, entry).range(species);
            assert!(lo <= glo && hi >= ghi);
        }
    }
    let actions = [fresh.baseline(0, Baseline::Random, &mut rng), fresh.baseline(1, Baseline::Random, &mut rng)];
    fresh.act(actions, [1, 2, 3, 4]).unwrap();
    let preview = GameState { battle: None, ..bare };
    let at_preview = Game::from_state(&preview).unwrap();
    assert_eq!(at_preview.phase(), Phase::Preview);
    assert_eq!(at_preview.legal(0).len(), N_PREVIEW);
    // The game and its battle must agree on whether the sheets are open.
    let mut odd = full.clone();
    odd.open = !odd.open;
    assert!(Game::from_state(&odd).is_err());
}

/// The public counts of the effects whose length is drawn at random: how
/// many turns a Pokémon has slept, how many move attempts it has made while
/// confused, how many turns a binding move has hurt it.
#[test]
fn the_counts_of_hidden_length_effects_advance_as_they_run() {
    let mon = |species: &str, moves: &[&str]| PokemonState::new(species, moves);
    let set = |species: &str, moves: &[&str]| {
        ShowdownSet::from_set(&PokemonSet::from_names(species, moves, "Hardy", [0; 6]).unwrap())
    };
    let roster = |a: &str, b: &str, c: &str, d: &str| {
        vec![
            set(a, &["Body Slam", "Protect"]),
            set(b, &["Scald", "Protect"]),
            set(c, &["Earthquake", "Protect"]),
            set(d, &["Moonblast", "Protect"]),
            set("Arcanine", &["Flare Blitz", "Protect"]),
            set("Rotom", &["Thunderbolt", "Protect"]),
        ]
    };
    let mut snorlax = mon("Snorlax", &["Body Slam", "Protect"]).status("slp");
    snorlax.status_turns = 3;
    let milotic = mon("Milotic", &["Scald", "Protect"]).volatile(CondState::confusion(4));
    let mut kingambit = mon("Kingambit", &["Kowtow Cleave", "Protect"]);
    kingambit.volatiles.push(CondState { value: 8, ..CondState::new("partiallytrapped").turns(5).source(0, 1) });
    let (garchomp, sylveon) = (mon("Garchomp", &["Earthquake", "Protect"]), mon("Sylveon", &["Moonblast", "Protect"]));
    let ours = SideState::new(vec![snorlax, milotic, garchomp.clone(), sylveon.clone()]);
    let theirs = SideState::new(vec![kingambit, mon("Pelipper", &["Hurricane", "Protect"]), garchomp, sylveon]);
    let battle = BattleState { turn: 5, sides: [ours, theirs], ..Default::default() };
    let st = GameState {
        rosters: [
            roster("Snorlax", "Milotic", "Garchomp", "Sylveon"),
            roster("Kingambit", "Pelipper", "Garchomp", "Sylveon"),
        ],
        open: false,
        battle: Some(battle),
        ..Default::default()
    };
    let mut g = Game::from_state(&st).unwrap();
    assert_eq!(g.counts(), &vgc_engine::env::Counts::default());
    let protect = |g: &Game, side: usize| {
        [g.action_of(side, Choice::mv(1, 0)).unwrap(), g.action_of(side, Choice::mv(1, 0)).unwrap()]
    };
    for turn in 1..=2u8 {
        let actions = [protect(&g, 0), protect(&g, 1)];
        g.act(actions, [turn as u16, 2, 3, 4]).unwrap();
        let c = g.counts();
        assert_eq!((c.sleep[0][0], c.confusion[0][1], c.trap[1][0]), (turn, turn, turn), "after turn {turn}: {c:?}");
        assert_eq!(c.sleep[1], [0; 6]);
        // The counts go with the game state.
        assert_eq!(Game::from_state(&g.to_state()).unwrap().counts(), c);
    }
}

#[test]
fn every_timed_count_has_a_length_and_the_counts_fit_the_token() {
    assert_eq!(OBS_LAYOUT, "obs-2");
    for kind in TIMED_VOLATILES {
        assert!(kind.data().duration >= 2, "{} lasts more than the turn it begins", kind.id());
        assert!(!HIDDEN_VOLATILES.contains(&kind), "{} is shown", kind.id());
    }
    for (kind, _) in COUNTED_VOLATILES {
        assert!(!HIDDEN_VOLATILES.contains(&kind), "{} is shown", kind.id());
    }
    // The counts, the slot flags, their counts and who moves first fill the rest of the token.
    let n = TIMED_VOLATILES.len() + COUNTED_VOLATILES.len() + SlotCond::ALL.len() + TIMED_SLOTS.len() + 6;
    assert_eq!(ACT_COUNTS + n, ACT_F);
}

/// A game at a move request a few turns in, with a Pokémon standing in each side's first place.
fn a_few_turns_in(seed: u16) -> Game {
    let mut rng = Rng::from_words([seed, 9, 9, 9]);
    for k in 0.. {
        let mut g = Game::new(teams(seed + k), true).unwrap();
        g.act([[0, 0], [0, 0]], [seed, 1, 2, 3]).unwrap();
        while !g.ended() && g.battle().unwrap().turn < 4 {
            let actions = [g.baseline(0, Baseline::Random, &mut rng), g.baseline(1, Baseline::Random, &mut rng)];
            g.act(actions, [rng.below(65536) as u16, 2, 3, 4]).unwrap();
        }
        let ok = |g: &Game| {
            let b = g.battle().unwrap();
            (0..2).all(|s| {
                let m = b.mon(b.active(s, 0));
                m.is_active && !m.fainted && m.status as u8 == 0
            })
        };
        if !g.ended() && g.phase() == Phase::Move && ok(&g) {
            return g;
        }
    }
    unreachable!()
}

#[test]
fn every_condition_with_a_limit_is_counted() {
    // (A seed whose sleeper keeps its sleep: a Lum Berry or Insomnia would cure it as the battle is set up.)
    for seed in 31.. {
        if counted_in(seed) {
            return;
        }
    }
}

/// [`every_condition_with_a_limit_is_counted`] from one game; `false` if its sleeper was cured.
fn counted_in(seed: u16) -> bool {
    let g = a_few_turns_in(seed);
    let b = g.battle().unwrap();
    let turn = b.turn as i16;
    let (idx, other) = (b.active(0, 0).idx as usize, b.active(1, 0).idx as usize);
    let mut v = serde_json::to_value(g.to_state()).unwrap();
    let first = v["battle"]["sides"][0]["pokemon"][idx]["moves"][0]["id"].clone();
    let cond = |id: &str, turns: Option<u8>| {
        let mut c = serde_json::json!({ "id": id });
        if let Some(t) = turns {
            c["turns"] = t.into();
        }
        c
    };
    let mut encore = cond("encore", Some(3));
    encore["move_id"] = first.clone();
    let mut disable = cond("disable", Some(3));
    disable["move_id"] = first.clone();
    let mut confusion = cond("confusion", None);
    confusion["value"] = 2.into();
    let vols = [
        cond("taunt", Some(2)),
        encore,
        disable,
        cond("healblock", Some(1)),
        cond("yawn", Some(1)),
        cond("magnetrise", Some(5)),
        confusion,
    ];
    let me = &mut v["battle"]["sides"][0]["pokemon"][idx];
    me["volatiles"] = serde_json::Value::Array(vols.to_vec());
    // Wish made last turn comes true at the end of this one; Future Sight made last turn, at the end of the next.
    let mut wish = cond("wish", None);
    (wish["value"], wish["a"]) = (50.into(), (turn - 2).into());
    let mut future = cond("futuremove", None);
    future["a"] = turn.into();
    v["battle"]["sides"][0]["slot_conditions"] = serde_json::json!([[wish, future], []]);
    // The other side's: asleep for one attempt so far; one of its own frozen for one turn.
    let them = &mut v["battle"]["sides"][1]["pokemon"][other];
    (them["status"], them["status_turns"]) = ("slp".into(), 2.into());
    v["counts"]["sleep"][1][other] = 1.into();
    v["counts"]["confusion"][0][idx] = 2.into();
    let st: GameState = serde_json::from_value(v).unwrap();
    let g = Game::from_state(&st).unwrap();
    let b = g.battle().unwrap();
    if b.mon(b.active(1, 0)).status as u8 == 0 {
        return false;
    }
    let left = |kind: VolKind| b.vols(b.active(0, 0)).get(kind).unwrap().duration;
    assert_eq!((left(VolKind::Taunt), left(VolKind::Healblock)), (2, 1), "the turns left as written");
    for view in 0..2 {
        let o = Obs::of(&g, view);
        // The position of side 0's first Pokémon: the viewer's own first, or the other side's.
        let a = o.act_feats(if view == 0 { 0 } else { 2 });
        let count = |kind: VolKind| a[ACT_COUNTS + TIMED_VOLATILES.iter().position(|&k| k == kind).unwrap()];
        assert_eq!(count(VolKind::Taunt), 1.0 / 3.0, "a Taunt with 2 of 3 turns left has run 1");
        assert_eq!(count(VolKind::Encore), 0.0, "an Encore that began after its target moved: 3 left, so 0");
        assert_eq!(count(VolKind::Disable), 2.0 / 5.0);
        assert_eq!(count(VolKind::Healblock), 4.0 / 5.0, "a Heal Block in its last turn reads as one of 5");
        assert_eq!(count(VolKind::Yawn), 1.0 / 2.0);
        assert_eq!(count(VolKind::Magnetrise), 0.0);
        assert_eq!(count(VolKind::Throatchop), 0.0, "absent");
        let after = ACT_COUNTS + TIMED_VOLATILES.len();
        assert_eq!(a[after], 2.0 / 5.0, "confused for two attempts");
        assert_eq!(a[after + 1], 0.0, "not bound");
        let slots = after + COUNTED_VOLATILES.len();
        assert_eq!(&a[slots..slots + 2], &[1.0, 1.0], "Wish and Future Sight are up");
        let timed = slots + SlotCond::ALL.len();
        assert_eq!(a[timed], 1.0 / 2.0, "Wish comes true at the end of this turn");
        assert_eq!(a[timed + 1], 1.0 / 3.0, "Future Sight at the end of the next");
    }
    // The sleeper's token, seen from either side: one attempt of three slept through. (A
    // position's first id is the token of the Pokémon standing there, plus one.)
    let (own, theirs) = (Obs::of(&g, 1), Obs::of(&g, 0));
    let (j, k) = (own.act_ids(0)[0] as usize - 1, theirs.act_ids(2)[0] as usize - 1);
    assert_eq!(own.mon_feats(j)[52], 1.0 / 3.0, "its own");
    assert_eq!(theirs.mon_feats(k)[52], 1.0 / 3.0, "the other side's, shown asleep");
    assert_eq!(theirs.mon_feats(k)[53], 0.0, "not frozen");
    true
}

#[test]
fn a_freeze_is_counted_in_turns() {
    let g = a_few_turns_in(47);
    let b = g.battle().unwrap();
    let idx = b.active(1, 0).idx as usize;
    let mut v = serde_json::to_value(g.to_state()).unwrap();
    let them = &mut v["battle"]["sides"][1]["pokemon"][idx];
    // (`status_turns` holds what the engine keeps: for a freeze, the attempts it has left.)
    (them["status"], them["status_turns"]) = ("frz".into(), 2.into());
    let g = Game::from_state(&serde_json::from_value(v).unwrap()).unwrap();
    let (theirs, own) = (Obs::of(&g, 0), Obs::of(&g, 1));
    let (k, j) = (theirs.act_ids(2)[0] as usize - 1, own.act_ids(0)[0] as usize - 1);
    assert_eq!(theirs.mon_feats(k)[53], 1.0 / 3.0, "frozen for one turn of three");
    assert_eq!(own.mon_feats(j)[53], 1.0 / 3.0);
    assert_eq!(own.mon_feats(j)[52], 0.0, "not asleep");
}

#[test]
fn a_state_without_its_battle_keeps_the_speed_beliefs_it_carries() {
    // D72: belief's probe takes the battle out of a state and asks which Speed points each
    // Pokémon can still have; it must be told what the order of moves has shown, not the sheets.
    let mut rng = Rng::from_words([7, 2, 7, 2]);
    let (mut checked, mut narrower) = (0, 0);
    for seed in 0..80u16 {
        if checked >= 300 {
            break;
        }
        let mut g = Game::new(teams(seed), true).unwrap();
        play(&mut g, &mut rng, |g| {
            if g.battle().is_none() || checked >= 300 {
                return;
            }
            let mut st = g.to_state();
            st.battle = None;
            let probe = Game::from_state(&st).unwrap();
            for side in 0..2 {
                for j in 0..ROSTER {
                    let points = g.speed_points(side, j);
                    assert_eq!(probe.speed_points(side, j), points, "seed {seed}, side {side}, entry {j}");
                    assert!(points.contains(&g.rosters()[side][j].stat_points[5]), "the truth is left");
                    narrower += (points.len() < 33) as usize;
                }
            }
            checked += 1;
        });
    }
    eprintln!("battle-less probes: {checked} decisions, {narrower} beliefs narrower than the sheets");
    assert!(checked >= 200 && narrower > 0, "{checked} {narrower}");
}
