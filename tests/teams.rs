//! Team sheets from outside (`vgc_engine::teams`): reading them, filling in
//! what a sheet leaves out, and keeping a pool. `scripts/check_scraper.py`
//! checks the whole path from a tournament's web page to a pool.

use vgc_engine::env::{Config, OBS_M, VecEnv};
use vgc_engine::format::{Format, ShowdownSet};
use vgc_engine::obs::{OBS_F, OBS_I};
use vgc_engine::rng::Rng;
use vgc_engine::teams::{Pool, PoolTeam, Sampler, Variation, guess_spread, read_sheet};
use vgc_engine::{Battle, Error, PokemonSet};

/// An open team sheet as a paste site shows it: no stat points.
const SHEET: &str = "\
Garchomp @ Garchompite Z
Ability: Rough Skin
Level: 50
Jolly Nature
- Earthquake
- Rock Slide
- Dragon Claw
- Protect

Incineroar @ Sitrus Berry
Ability: Intimidate
Level: 50
Careful Nature
- Fake Out
- Flare Blitz
- Parting Shot
- Snarl

Rillaboom (M) @ Assault Vest
Ability: Grassy Surge
Level: 50
Adamant Nature
- Grassy Glide
- Wood Hammer
- U-turn
- Fake Out

Milotic @ Leftovers
Ability: Competitive
Level: 50
Bold Nature
- Scald
- Icy Wind
- Recover
- Protect

Sylveon @ Throat Spray
Ability: Pixilate
Level: 50
Modest Nature
- Hyper Voice
- Moonblast
- Calm Mind
- Protect

Kommo-o @ Leppa Berry
Ability: Soundproof
Level: 50
Timid Nature
- Clanging Scales
- Aura Sphere
- Flamethrower
- Protect
";

#[test]
fn a_sheet_in_showdowns_format() {
    let team = read_sheet(SHEET.lines());
    let species: Vec<_> = team.iter().map(|r| r.set.species.as_str()).collect();
    assert_eq!(species, ["Garchomp", "Incineroar", "Rillaboom", "Milotic", "Sylveon", "Kommo-o"]);
    let garchomp = &team[0];
    assert_eq!((garchomp.set.item.as_str(), garchomp.set.ability.as_str()), ("Garchompite Z", "Rough Skin"));
    assert_eq!(garchomp.set.moves, ["Earthquake", "Rock Slide", "Dragon Claw", "Protect"]);
    assert_eq!(garchomp.set.nature, "Jolly");
    // A sheet gives the nature and never the stat points.
    assert!(team.iter().all(|r| r.nature_given && !r.spread_given));

    // Nicknames, genders and stat points, where a paste has them.
    let full =
        "Rex (Garchomp) (F) @ Life Orb\nAbility: Rough Skin\nEVs: 32 Atk / 2 SpD / 32 Spe\nJolly Nature\n- Earthquake";
    let rex = &read_sheet(full.lines())[0];
    assert_eq!((rex.set.species.as_str(), rex.set.item.as_str()), ("Garchomp", "Life Orb"));
    assert!(rex.spread_given);
    assert_eq!((rex.set.evs.atk, rex.set.evs.spd, rex.set.evs.spe, rex.set.evs.hp), (32, 2, 32, 0));
}

#[test]
fn a_sheet_laid_out_some_other_way() {
    // One fact to a line and no punctuation, as a page of cards reads once its markup is gone,
    // between lines that have nothing to do with the team.
    let lines = [
        "VR Pastes - Victory Road",
        "Beta",
        "Somebody's Regional Championships OTS",
        "Garchomp",
        "Life Orb",
        "Rough Skin",
        "Jolly",
        "Earthquake",
        "Rock Slide",
        "Protect",
        "Copy",
        "Milotic",
        "Leftovers",
        "Competitive",
        "Scald",
        "Recover",
        "Share",
    ];
    let team = read_sheet(lines.iter().copied());
    assert_eq!(team.len(), 2);
    assert_eq!((team[0].set.item.as_str(), team[0].set.ability.as_str()), ("Life Orb", "Rough Skin"));
    assert_eq!(team[0].set.moves, ["Earthquake", "Rock Slide", "Protect"]);
    assert!(team[0].nature_given && !team[1].nature_given);
    assert_eq!(team[1].set.moves, ["Scald", "Recover"]);

    // A strip of the six Pokémon first, and the whole sheet twice (a page and the copy in its
    // script): still one team.
    let twice = format!("Garchomp\nIncineroar\nRillaboom\nMilotic\nSylveon\nKommo-o\n{SHEET}\n{SHEET}");
    let team = read_sheet(twice.lines());
    assert_eq!(team.len(), 6);
    assert!(team.iter().all(|r| r.set.moves.len() == 4));
}

#[test]
fn stat_points_are_filled_in_by_rule_of_thumb() {
    let mut team = read_sheet(SHEET.lines());
    team.iter_mut().for_each(|r| guess_spread(r, false));
    let points = |i: usize| {
        let e = team[i].set.evs;
        [e.hp, e.atk, e.def, e.spa, e.spd, e.spe]
    };
    // Jolly: attack and speed. Careful: HP and Special Defense. Adamant: HP and attack.
    assert_eq!(points(0), [2, 32, 0, 0, 0, 32]);
    assert_eq!(points(1), [32, 0, 2, 0, 32, 0]);
    assert_eq!(points(2), [32, 32, 2, 0, 0, 0]);
    // Bold: HP and Defense. Modest: HP and special attack. Timid: special attack and speed.
    assert_eq!(points(3), [32, 0, 32, 0, 2, 0]);
    assert_eq!(points(4), [32, 0, 2, 32, 0, 0]);
    assert_eq!(points(5), [2, 0, 0, 32, 0, 32]);
    assert!(team.iter().all(|r| {
        r.set.evs.hp + r.set.evs.atk + r.set.evs.def + r.set.evs.spa + r.set.evs.spd + r.set.evs.spe == 66
    }));

    // With no nature on the sheet one is chosen to go with the points: a fast attacker is
    // Jolly, a slow one Adamant (Brave on a Trick Room team), a Pokémon with no attacks Bold.
    let sheet = "Garchomp\n- Earthquake\n- Rock Slide\n\nSnorlax\n- Body Slam\n- Double-Edge\n\nSnorlax\n- Body Slam\n\nClefable\n- Follow Me";
    let mut bare = read_sheet(sheet.lines());
    assert_eq!(bare.len(), 3, "the second Snorlax is the first over again");
    bare.iter_mut().for_each(|r| guess_spread(r, false));
    let natures: Vec<_> = bare.iter().map(|r| r.set.nature.as_str()).collect();
    assert_eq!(natures, ["Jolly", "Adamant", "Bold"]);
    let mut slow = read_sheet("Snorlax\n- Body Slam\n- Double-Edge".lines());
    guess_spread(&mut slow[0], true);
    assert_eq!(slow[0].set.nature, "Brave");
}

#[test]
fn one_attack_does_not_make_an_attacker() {
    let spread = |sheet: &str| {
        let mut read = read_sheet(sheet.lines());
        guess_spread(&mut read[0], false);
        let e = read[0].set.evs;
        ([e.hp, e.atk, e.def, e.spa, e.spd, e.spe], read[0].set.nature.clone())
    };
    // Whimsicott with Moonblast beside three other moves is a fast support: HP and Speed,
    // not Special Attack. The same with no nature on the sheet, where one is chosen for it.
    let whimsicott =
        "Whimsicott @ Focus Sash\nAbility: Prankster\nTimid Nature\n- Tailwind\n- Encore\n- Moonblast\n- Protect";
    assert_eq!(spread(whimsicott), ([32, 0, 2, 0, 0, 32], "Timid".to_string()));
    assert_eq!(spread(&whimsicott.replace("Timid Nature\n", "")), ([32, 0, 2, 0, 0, 32], "Timid".to_string()));
    // A slow one is HP and Defense: Incineroar with only Fake Out to hit with.
    let incineroar = "Incineroar\n- Fake Out\n- Parting Shot\n- Protect\n- Taunt";
    assert_eq!(spread(incineroar), ([32, 0, 32, 0, 2, 0], "Impish".to_string()));
    // Unless the nature says it means to hit: Modest, and one attack.
    let sylveon = "Sylveon\nModest Nature\n- Hyper Voice\n- Protect\n- Calm Mind\n- Helping Hand";
    assert_eq!(spread(sylveon).0, [32, 0, 2, 32, 0, 0]);
}

/// A pool of legal teams to draw from (random ones: what they are does not matter here).
fn pool_of(n: usize) -> Pool {
    let format = Format::current();
    let mut rng = Rng::from_words([9, 8, 7, 6]);
    let teams = (0..n)
        .map(|i| PoolTeam {
            player: format!("Player {i}"),
            team: format.random_team(&mut rng).iter().map(ShowdownSet::from_set).collect(),
            ..Default::default()
        })
        .collect();
    Pool { format: format.id.clone(), teams, ..Default::default() }
}

#[test]
fn teams_are_handed_out_varied_and_always_legal() -> Result<(), Error> {
    let pool = pool_of(30);
    let sampler = Sampler::new(&pool)?;
    let format = Format::current();
    let mut rng = Rng::from_words([1, 2, 3, 4]);

    // With no variation a team comes out as it is in the pool.
    for _ in 0..50 {
        let s = sampler.sample(&mut rng, &Variation::NONE);
        assert_eq!((s.swapped, s.respread), (0, 0));
        assert_eq!(s.team, pool.teams[s.from].sets()?);
    }

    // Stat points only: the same six Pokémon, each with as many points as it had, spent otherwise.
    let points_only = Variation { respread: 0.5, max_shift: 12, swap_one: 0.0, swap_two: 0.0 };
    let mut moved = 0;
    for _ in 0..500 {
        let s = sampler.sample(&mut rng, &points_only);
        let was = pool.teams[s.from].sets()?;
        assert!(format.check_team(&s.team).is_empty());
        let mut changed = 0;
        for (a, b) in was.iter().zip(&s.team) {
            assert_eq!(
                (a.species, &a.moves, a.item, a.ability, a.nature),
                (b.species, &b.moves, b.item, b.ability, b.nature)
            );
            let total = |p: &[u8; 6]| p.iter().map(|&v| v as u32).sum::<u32>();
            assert_eq!(total(&a.stat_points), total(&b.stat_points));
            assert!(b.stat_points.iter().all(|&v| v <= 32));
            changed += (a.stat_points != b.stat_points) as u8;
        }
        assert!(changed <= s.respread);
        moved += s.respread as u32;
    }
    assert!((1200..1800).contains(&moved), "about half of 3,000 Pokémon: {moved}");

    // The default: now and then a Pokémon or two from another team. Whatever comes out is legal.
    let how = Variation::default();
    let (mut none, mut one, mut two) = (0, 0, 0);
    for _ in 0..4000 {
        let s = sampler.sample(&mut rng, &how);
        let problems = format.check_team(&s.team);
        assert!(problems.is_empty(), "{}", problems[0]);
        let was = pool.teams[s.from].sets()?;
        let kept = was.iter().zip(&s.team).filter(|(a, b)| a.species == b.species && a.moves == b.moves).count();
        assert!(kept >= 6 - s.swapped as usize);
        match s.swapped {
            0 => none += 1,
            1 => one += 1,
            _ => two += 1,
        }
    }
    assert!(
        (450..750).contains(&one) && (120..290).contains(&two),
        "of 4,000: {none} as they were, {one} with one swapped, {two} with two"
    );

    // The same seed gives the same teams.
    let draw = |seed: u16| {
        let mut rng = Rng::from_words([seed, 0, 0, 1]);
        (0..20).map(|_| sampler.sample(&mut rng, &how)).collect::<Vec<_>>()
    };
    assert_eq!(draw(5), draw(5));
    assert_ne!(draw(5), draw(6));
    Ok(())
}

#[test]
fn a_pool_of_teams_can_be_saved_loaded_and_played() -> Result<(), Error> {
    let mut read = read_sheet(SHEET.lines());
    read.iter_mut().for_each(|r| guess_spread(r, false));
    let team: Vec<_> = read.into_iter().map(|r| r.set).collect();
    let format = Format::current();
    let problems = format.check_showdown_team(&team);
    assert!(problems.is_empty(), "{:?}", problems.iter().map(|p| p.to_string()).collect::<Vec<_>>());

    let entry =
        PoolTeam { player: "Somebody".into(), placing: Some(1), spreads_guessed: true, team, ..Default::default() };
    let pool =
        Pool { event: "A tournament".into(), format: format.id.clone(), teams: vec![entry], ..Default::default() };
    let back = Pool::from_json(&pool.to_json())?;
    assert_eq!(back, pool);

    // Six registered, four brought, sheets open: a mirror match.
    let sets = back.teams[0].sets()?;
    let battle = Battle::with_rosters([&sets, &sets], [&[0, 1, 2, 3], &[0, 1, 4, 5]], true, [1, 2, 3, 4])?;
    let shown = battle.shown(1);
    assert_eq!(shown.roster.len(), 6);
    assert_eq!(shown.roster[0].sheet.as_ref().unwrap().item, "garchompitez");
    Ok(())
}

/// A pool of teams the regulation refuses (a Garchomp with Levitate and
/// Moonblast) is refused by the sampler as a rule, and handed out as it is
/// when the check is switched off; the environment then plays it.
#[test]
fn an_unchecked_sampler_hands_out_teams_the_regulation_refuses() -> Result<(), Error> {
    let mut pool = pool_of(3);
    let odd = PokemonSet::from_names("Garchomp", &["Moonblast", "Protect", "Earthquake"], "Jolly", [11; 6])?
        .ability("Levitate")?
        .item("Life Orb")?;
    pool.teams[0].team[0] = ShowdownSet::from_set(&odd);
    let format = Format::current();
    assert!(!format.check_team(&pool.teams[0].sets()?).is_empty());
    assert!(matches!(Sampler::new(&pool), Err(Error::BadTeam(_))));

    let sampler = Sampler::with_check(&pool, false)?;
    assert!(!sampler.checked() && Sampler::new(&pool_of(3))?.checked());
    let mut rng = Rng::from_words([4, 4, 4, 4]);
    let handed = (0..60).map(|_| sampler.sample(&mut rng, &Variation::NONE)).find(|s| s.from == 0).unwrap();
    assert_eq!(handed.team[0], odd, "as it is in the pool");

    let config =
        Config { envs: 8, seed: 3, threads: 1, variation: Variation::NONE, check_teams: false, ..Config::default() };
    assert!(matches!(VecEnv::new(&pool, Config { check_teams: true, ..config.clone() }), Err(Error::BadTeam(_))));
    let mut env = VecEnv::new(&pool, config)?;
    let n = env.len();
    let (mut f, mut i, mut m) = (vec![0.0; 2 * n * OBS_F], vec![0; 2 * n * OBS_I], vec![0; 2 * n * OBS_M]);
    let (mut reward, mut done, mut actions) = (vec![0.0; 2 * n], vec![0; n], vec![0; 4 * n]);
    for _ in 0..40 {
        env.baseline(vgc_engine::env::Baseline::Random, &mut actions);
        env.step(&actions, &mut f, &mut i, &mut m, &mut reward, &mut done)?;
    }
    assert!(env.stats().decisions == 40 * n as u64);
    Ok(())
}
