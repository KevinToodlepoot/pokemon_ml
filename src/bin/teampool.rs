//! Turns team sheets collected by `scripts/scrape_teams.py` into a pool of teams the engine can play.
//!
//!     teampool teams/raw/2027-frankfurt.json [--out teams/2027-frankfurt.json]
//!              [--section TEXT] [--top N] [--play N] [--vary] [--seed S]
//!
//! Reads each sheet, fills in the stat points a sheet does not give (by rule
//! of thumb: see `vgc_engine::teams::guess_spread`), checks the team against
//! the regulation and writes the legal ones to `--out`. It says what it left
//! out and why. `--section` keeps only the teams whose heading at the source
//! contains the text (a division, say); `--top` only the first N of those.
//! `--play N` then plays N battles between teams of the pool, picked and
//! played at random, as a check that every one of them runs; with `--vary`
//! the teams are varied the way training will vary them
//! (`vgc_engine::teams::Variation`: stat points moved, Pokémon swapped in
//! from other teams).

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Deserialize;
use vgc_engine::data::{SPECIES, species_id, to_id};
use vgc_engine::format::Format;
use vgc_engine::rng::Rng;
use vgc_engine::teams::{Pool, PoolTeam, Sampler, Variation, guess_spread, read_sheet};
use vgc_engine::{ACTIVE, Battle};

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTeam {
    section: String,
    placing: Option<u32>,
    record: String,
    country: String,
    /// The Pokémon the tournament page pictured beside the player, as it names its pictures.
    species: Vec<String>,
    url: String,
    player: String,
    sheet: Vec<String>,
    error: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Raw {
    source: String,
    event: String,
    teams: Vec<RawTeam>,
}

/// The Pokémon a picture or a sheet names, whatever its forme.
fn family(name: &str) -> Option<&'static str> {
    let id = to_id(name);
    // The page names its pictures `garchomp-mega-z`, `meowstic-f-mega`: try shorter and shorter.
    let mut end = id.len();
    loop {
        if let Some(sp) = species_id(&id[..end]) {
            return Some(SPECIES[sp as usize].base_species);
        }
        if end <= 3 {
            return None;
        }
        end -= 1;
    }
}

fn main() {
    let mut path = None;
    let mut out = None;
    let mut section = None;
    let mut top = usize::MAX;
    let mut play = 0usize;
    let mut seed = 1u16;
    let mut vary = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut number = |what: &str| {
            args.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or_else(|| panic!("{what} takes a number"))
        };
        match a.as_str() {
            "--top" => top = number("--top"),
            "--play" => play = number("--play"),
            "--seed" => seed = number("--seed") as u16,
            "--vary" => vary = true,
            "--out" => out = args.next(),
            "--section" => section = args.next(),
            _ => path = Some(a),
        }
    }
    let path =
        path.expect("usage: teampool RAW.json [--out FILE] [--section TEXT] [--top N] [--play N] [--vary] [--seed S]");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let raw: Raw =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path} is not what scrape_teams.py writes: {e}"));
    let format = Format::current();

    // (A tournament's teams, never split into train and held-out: no side.)
    let mut pool =
        Pool { source: raw.source, event: raw.event, format: format.id.clone(), side: None, teams: Vec::new() };
    let mut left_out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut considered, mut pictures_disagree) = (0, Vec::new());
    for team in &raw.teams {
        if section.as_ref().is_some_and(|s| !team.section.to_lowercase().contains(&s.to_lowercase())) {
            continue;
        }
        if considered == top {
            break;
        }
        considered += 1;
        let who = if team.player.is_empty() { team.url.clone() } else { format!("{} ({})", team.player, team.url) };
        let mut skip = |why: String| left_out.entry(why).or_default().push(who.clone());
        if team.sheet.is_empty() {
            skip(team.error.clone().unwrap_or_else(|| "no team sheet".to_string()));
            continue;
        }
        let mut read = read_sheet(team.sheet.iter().map(String::as_str));
        if read.len() != 6 {
            skip(format!("{} Pokémon could be read off the sheet, not 6", read.len()));
            continue;
        }
        // Does the sheet show the Pokémon the tournament page pictured?
        let mut pictured: Vec<_> = team.species.iter().filter_map(|s| family(s)).collect();
        let mut listed: Vec<_> = read.iter().filter_map(|r| family(&r.set.species)).collect();
        pictured.sort_unstable();
        listed.sort_unstable();
        if pictured.len() == 6 && pictured != listed {
            pictures_disagree.push(format!("{who}: pictured {pictured:?}, sheet {listed:?}"));
        }
        let trick_room = read.iter().any(|r| r.set.moves.iter().any(|m| m == "Trick Room"));
        let spreads_guessed = read.iter().any(|r| !r.spread_given);
        let natures_guessed = read.iter().any(|r| !r.nature_given);
        read.iter_mut().for_each(|r| guess_spread(r, trick_room));
        let sets: Vec<_> = read.into_iter().map(|r| r.set).collect();
        let problems = format.check_showdown_team(&sets);
        if let Some(first) = problems.first() {
            skip(format!("not legal in {}: {first}", format.name));
            continue;
        }
        pool.teams.push(PoolTeam {
            player: team.player.clone(),
            placing: team.placing,
            record: team.record.clone(),
            country: team.country.clone(),
            section: team.section.clone(),
            url: team.url.clone(),
            spreads_guessed,
            natures_guessed,
            team: sets,
        });
    }

    println!("{}: {} of {considered} teams read and legal in {}", pool.event, pool.teams.len(), format.name);
    for (why, who) in &left_out {
        println!("  left out, {why}: {}", who.len());
        for w in who.iter().take(3) {
            println!("      {w}");
        }
    }
    if !pictures_disagree.is_empty() {
        println!("  {} sheets do not show the Pokémon the tournament page pictured:", pictures_disagree.len());
        pictures_disagree.iter().take(5).for_each(|d| println!("      {d}"));
    }
    let guessed = pool.teams.iter().filter(|t| t.spreads_guessed).count();
    let natures = pool.teams.iter().filter(|t| t.natures_guessed).count();
    println!("  stat points filled in by rule of thumb on {guessed} teams; natures too on {natures}");
    let mut usage: BTreeMap<&str, usize> = BTreeMap::new();
    for set in pool.teams.iter().flat_map(|t| &t.team) {
        *usage.entry(set.species.as_str()).or_default() += 1;
    }
    let mut usage: Vec<_> = usage.into_iter().collect();
    usage.sort_by_key(|&(name, n)| (std::cmp::Reverse(n), name));
    let most: Vec<_> = usage.iter().take(12).map(|(name, n)| format!("{name} {n}")).collect();
    println!("  {} different Pokémon; most used: {}", usage.len(), most.join(", "));
    let mut distinct: Vec<Vec<&vgc_engine::format::ShowdownSet>> = Vec::new();
    for t in &pool.teams {
        let sets: Vec<_> = t.team.iter().collect();
        if !distinct.contains(&sets) {
            distinct.push(sets);
        }
    }
    println!("  {} distinct teams", distinct.len());

    if pool.teams.is_empty() {
        // An empty pool is of no use to anyone, and a file of one looks like a result.
        eprintln!(
            "no team could be used, so no pool was written. The sheets in {path} are under \"sheet\" for each team."
        );
        std::process::exit(1);
    }
    let out = out.unwrap_or_else(|| {
        // teams/raw/x.json -> teams/x.json
        let p = std::path::Path::new(&path);
        let dir = p.parent().and_then(|d| d.parent()).unwrap_or(std::path::Path::new("."));
        dir.join(p.file_name().unwrap()).to_string_lossy().into_owned()
    });
    std::fs::write(&out, pool.to_json()).unwrap_or_else(|e| panic!("cannot write {out}: {e}"));
    println!("wrote {out}");

    if play > 0 {
        let sampler = Sampler::new(&pool).expect("the pool was just checked");
        let how = if vary { Variation::default() } else { Variation::NONE };
        let mut rng = Rng::from_words([seed, 2, 3, 4]);
        let (mut decisions, mut turns, mut swapped, mut respread) = (0u64, 0u64, 0u64, 0u64);
        let start = Instant::now();
        for _ in 0..play {
            let sides = [sampler.sample(&mut rng, &how), sampler.sample(&mut rng, &how)];
            swapped += sides.iter().map(|s| s.swapped as u64).sum::<u64>();
            respread += sides.iter().map(|s| s.respread as u64).sum::<u64>();
            let mut below = |n: usize| rng.below(n as u32) as usize;
            // Four of the six, in a random order.
            let mut picks = [[0usize; 4]; 2];
            for pick in &mut picks {
                let mut order = [0, 1, 2, 3, 4, 5];
                for k in 0..4 {
                    order.swap(k, k + below(6 - k));
                }
                pick.copy_from_slice(&order[..4]);
            }
            let seed = [below(65536) as u16, below(65536) as u16, below(65536) as u16, below(65536) as u16];
            let mut b = Battle::with_rosters([&sides[0].team, &sides[1].team], [&picks[0], &picks[1]], true, seed)
                .expect("a legal team starts a battle");
            while !b.ended && b.turn <= 300 {
                let mut choice = [[vgc_engine::Choice::Pass; ACTIVE]; 2];
                for (side, c) in choice.iter_mut().enumerate() {
                    let joint = b.joint_choices(side);
                    *c = joint[below(joint.len())];
                }
                b.choose(choice).expect("a legal choice");
                decisions += 1;
            }
            turns += b.turn as u64;
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "played {play} battles between teams of the pool at random: {:.0} battles/s, {:.0} decisions/s, {:.1} turns each",
            play as f64 / secs,
            decisions as f64 / secs,
            turns as f64 / play as f64
        );
        if vary {
            println!(
                "  varied: of {} teams fielded, {swapped} Pokémon were swapped in from other teams and {respread} had their stat points moved",
                2 * play
            );
        }
    }
}
