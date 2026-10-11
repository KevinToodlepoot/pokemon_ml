//! Starting a battle from a position: `Battle::to_state` and `Battle::from_state`.
//!
//! That a rebuilt battle is the same battle is checked on recorded Showdown
//! battles (`difftest --rebuild`, and `parity.rs` on the fixture). These
//! tests are about positions written by hand: that what is left unsaid comes
//! out sensibly, and that what is said has its effect. They only use
//! outcomes that do not depend on the random number generator.

use vgc_engine::data::{Pseudo, SideCond, Type, Weather};
use vgc_engine::position::{ActionState, BattleState, CondState, PokemonState, SideState};
use vgc_engine::{Battle, Choice, Error, PokemonSet, Request, VolKind};

fn mon(species: &str, moves: &[&str]) -> PokemonState {
    PokemonState::new(species, moves)
}

/// A side with these two on the field and two bystanders on the bench.
fn side(left: PokemonState, right: PokemonState) -> SideState {
    SideState::new(vec![
        left,
        right,
        mon("Milotic", &["Scald", "Protect"]),
        mon("Garchomp", &["Earthquake", "Protect"]),
    ])
}

fn bystander() -> PokemonState {
    mon("Snorlax", &["Protect", "Rest"])
}

fn position(p1: SideState, p2: SideState) -> BattleState {
    BattleState { turn: 5, sides: [p1, p2], ..Default::default() }
}

fn moves_offered(b: &Battle, side: usize, pos: usize) -> Vec<u8> {
    let mut slots: Vec<u8> = b
        .legal_choices(side, pos)
        .into_iter()
        .filter_map(|c| match c {
            Choice::Move { slot, .. } => Some(slot),
            _ => None,
        })
        .collect();
    slots.sort();
    slots.dedup();
    slots
}

const PROTECT: [Choice; 2] = [Choice::Move { slot: 0, target: 0, mega: false }; 2];

#[test]
fn a_choice_refusal_records_an_attempt_without_using_the_move_or_pp() -> Result<(), Error> {
    let mut locked = mon("Snorlax", &["Body Slam", "Rest"])
        .item("Choice Scarf")
        .volatile(CondState { move_id: "bodyslam".into(), ..CondState::new("choicelock") });
    locked.last_move = "bodyslam".into();
    let mut state = position(side(mon("Jolteon", &["Magic Room"]), bystander()), side(locked, bystander()));
    state.pseudo_weather.push(CondState::new("magicroom").turns(2));
    let mut b = Battle::from_state(&state)?;
    let r = b.active(1, 0);
    let before = b.mon(r).moves[1].pp;
    b.choose([[PROTECT[0], PROTECT[0]], [Choice::Move { slot: 1, target: 0, mega: false }, PROTECT[0]]])?;
    let m = b.mon(r);
    let rest = vgc_engine::data::move_id("rest").unwrap();
    assert_eq!(m.last_attempted_move, Some(rest));
    assert_eq!(m.last_move, vgc_engine::data::move_id("bodyslam").unwrap());
    assert_eq!(m.moves[1].pp, before, "refusal occurs before PP deduction");
    let rebuilt = Battle::from_state(&BattleState::from_json(&b.to_state().to_json())?)?;
    assert_eq!(rebuilt.mon(r).last_attempted_move, Some(rest));
    assert_eq!(rebuilt.mon(r).last_move, m.last_move);
    Ok(())
}

#[test]
fn an_ordinary_failed_move_is_used_and_spends_pp() -> Result<(), Error> {
    let mut user = mon("Snorlax", &["Body Slam", "Rest"]);
    user.last_move = "bodyslam".into();
    let mut b = Battle::from_state(&position(side(user, bystander()), side(bystander(), bystander())))?;
    let r = b.active(0, 0);
    let before = b.mon(r).moves[1].pp;
    b.choose([[Choice::Move { slot: 1, target: 0, mega: false }, PROTECT[0]], PROTECT])?;
    let m = b.mon(r);
    let rest = vgc_engine::data::move_id("rest").unwrap();
    assert_eq!((m.last_move, m.last_attempted_move), (rest, Some(rest)));
    assert_eq!(m.moves[1].pp, before - 1, "Rest fails at full HP after beginning");
    Ok(())
}

#[test]
fn benched_pp_uncertainty_survives_rebuild_and_clears_on_a_move_request() -> Result<(), Error> {
    let mut state = position(side(bystander(), bystander()), side(bystander(), bystander()));
    let benched = &mut state.sides[0].pokemon[2];
    benched.unknown_pp = 1;
    benched.moves[0].pp = Some(7);
    let b = Battle::from_state(&state)?;
    let saved = BattleState::from_json(&b.to_state().to_json())?;
    let mut b = Battle::from_state(&saved)?;
    assert_eq!((b.sides[0].team[2].unknown_pp, b.sides[0].team[2].moves[0].pp), (1, 7));
    b.choose([[Choice::Switch { to: 2 }, PROTECT[0]], PROTECT])?;
    let m = b.mon(b.active(0, 0));
    assert_eq!((m.unknown_pp, m.moves[0].pp), (0, 7), "request reveals PP without altering it");
    Ok(())
}

#[test]
fn a_rebuilt_battle_plays_out_the_same() -> Result<(), Error> {
    let set = |species, moves: &[&str], ability, item| {
        PokemonSet::from_names(species, moves, "Hardy", [0; 6])?.ability(ability)?.item(item)
    };
    let p1 = [
        set("Garchomp", &["Earthquake", "Rock Slide", "Dragon Claw", "Protect"], "Rough Skin", "Choice Scarf")?,
        set("Sylveon", &["Hyper Voice", "Moonblast", "Calm Mind", "Protect"], "Pixilate", "Leftovers")?,
        set("Arcanine", &["Flare Blitz", "Extreme Speed", "Will-O-Wisp", "Protect"], "Intimidate", "Sitrus Berry")?,
        set("Milotic", &["Scald", "Ice Beam", "Recover", "Icy Wind"], "Competitive", "Rocky Helmet")?,
    ];
    let p2 = [
        set("Dragonite", &["Extreme Speed", "Iron Head", "Dragon Dance", "Protect"], "Multiscale", "Lum Berry")?,
        set("Gardevoir", &["Psychic", "Dazzling Gleam", "Thunder Wave", "Protect"], "Trace", "Focus Sash")?,
        set("Excadrill", &["High Horsepower", "Iron Head", "Rock Slide", "Swords Dance"], "Mold Breaker", "Life Orb")?,
        set("Charizard", &["Heat Wave", "Air Slash", "Fly", "Protect"], "Blaze", "Charizardite Y")?,
    ];
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut pick = |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    let mut decisions = 0;
    for game in 0..20 {
        let mut original = Battle::new([&p1, &p2], [game, 10, 8, 1])?;
        let mut rebuilt = original;
        while !original.ended {
            // Through JSON and back, then both get the same choices.
            let written = original.to_state().to_json();
            rebuilt = Battle::from_state(&BattleState::from_json(&written)?)?;
            assert_eq!(original.position_diff(&rebuilt), None, "game {game}, turn {}", original.turn);
            let mut choices = [[Choice::Pass; 2]; 2];
            for (s, c) in choices.iter_mut().enumerate() {
                let legal = original.joint_choices(s);
                assert_eq!(legal, rebuilt.joint_choices(s));
                *c = legal[pick(legal.len())];
            }
            original.choose(choices)?;
            rebuilt.choose(choices)?;
            assert_eq!(original.position_diff(&rebuilt), None, "game {game}, after turn {}", original.turn);
            decisions += 1;
        }
        assert_eq!(original.winner, rebuilt.winner);
    }
    assert!(decisions > 200);
    Ok(())
}

#[test]
fn a_position_written_by_hand() -> Result<(), Error> {
    let mut ours = SideState::new(vec![
        mon("Incineroar", &["Fake Out", "Flare Blitz", "Parting Shot", "Protect"]).ability("Intimidate"),
        mon("Garchomp", &["Earthquake", "Rock Slide", "Protect"])
            .item("Life Orb")
            .nature("Jolly", [0, 32, 0, 0, 2, 32]),
        mon("Rillaboom", &["Grassy Glide", "Wood Hammer"]),
    ]);
    ours.pokemon[0].hp_percent = Some(42.0);
    ours.pokemon[0].boosts[0] = -1;
    ours.conditions.push(CondState::new("tailwind").turns(2));
    let theirs = SideState::new(vec![
        mon("Pelipper", &["Hurricane", "Protect"]).ability("Drizzle"),
        mon("Archaludon", &["Electro Shot", "Draco Meteor"]),
    ]);
    let state = BattleState {
        turn: 4,
        sides: [ours, theirs],
        weather: Some(CondState::new("raindance").turns(3)),
        ..Default::default()
    };
    let b = Battle::from_state(&state)?;
    assert_eq!((b.turn, b.request), (4, Request::Move));
    let incineroar = b.mon(b.active(0, 0));
    assert_eq!(incineroar.hp, (incineroar.max_hp() as f32 * 0.42).ceil() as u16);
    assert_eq!(incineroar.boosts[0], -1);
    assert_eq!(b.mon(b.active(0, 1)).stats[5], 169, "Jolly Garchomp with 32 points in Speed");
    assert_eq!(b.sides[0].conds.get(SideCond::Tailwind).map(|c| c.duration), Some(2));
    assert_eq!((b.field.weather.kind, b.field.weather.duration), (Weather::Raindance, 3));
    assert_eq!(b.sides[0].pokemon_left, 3);
    // In the middle of a battle a Pokémon is taken to have moved before: no Fake Out.
    assert_eq!(moves_offered(&b, 0, 0), vec![1, 2, 3]);
    // The same position survives being written out as JSON.
    let again = Battle::from_state(&BattleState::from_json(&state.to_json())?)?;
    assert_eq!(b.position_diff(&again), None);
    Ok(())
}

#[test]
fn json_by_hand() -> Result<(), Error> {
    let text = r#"{
        "turn": 7,
        "sides": [
            {"pokemon": [
                {"species": "Sylveon", "ability": "Pixilate", "item": "Leftovers", "hp_percent": 60,
                 "moves": [{"id": "Hyper Voice"}, {"id": "Protect", "pp": 3}],
                 "volatiles": [{"id": "taunt", "turns": 2}]},
                {"species": "Arcanine", "status": "par", "moves": [{"id": "Flare Blitz"}]},
                {"species": "Milotic", "fainted": true, "moves": [{"id": "Scald"}]}
            ], "conditions": [{"id": "stealthrock"}, {"id": "spikes", "value": 2}]},
            {"pokemon": [
                {"species": "Dragonite", "moves": [{"id": "Extreme Speed"}], "boosts": [1, 0, 0, 0, 1, 0, 0]},
                {"species": "Gardevoir", "moves": [{"id": "Psychic"}]}
            ]}
        ],
        "terrain": {"id": "psychicterrain", "turns": 4},
        "pseudo_weather": [{"id": "trickroom", "turns": 2}]
    }"#;
    let b = Battle::from_state(&BattleState::from_json(text)?)?;
    assert_eq!(b.turn, 7);
    assert_eq!(b.mon(b.active(0, 0)).moves[1].pp, 3);
    assert_eq!(moves_offered(&b, 0, 0), vec![0], "taunted: Protect is not offered");
    assert_eq!((b.sides[0].pokemon_left, b.sides[0].total_fainted), (2, 1));
    assert_eq!(b.sides[0].conds.get(SideCond::Spikes).map(|c| c.data), Some(2));
    assert!(b.field.pseudo.has(Pseudo::Trickroom));
    // A misspelt field is an error, not something silently ignored.
    assert!(BattleState::from_json(r#"{"turn": 2, "wether": null}"#).is_err());
    Ok(())
}

#[test]
fn fake_out_is_there_for_a_pokemon_that_has_just_come_in() -> Result<(), Error> {
    let mut incineroar = mon("Incineroar", &["Fake Out", "Protect"]);
    incineroar.move_actions = Some(0);
    incineroar.active_turns = Some(0);
    let b = Battle::from_state(&position(side(incineroar, bystander()), side(bystander(), bystander())))?;
    assert_eq!(moves_offered(&b, 0, 0), vec![0, 1]);
    Ok(())
}

#[test]
fn a_choice_lock_and_an_encore_hold() -> Result<(), Error> {
    let garchomp = mon("Garchomp", &["Dragon Claw", "Rock Slide", "Protect"])
        .item("Choice Scarf")
        .volatile(CondState::choice_lock("Rock Slide"));
    let mut sylveon =
        mon("Sylveon", &["Hyper Voice", "Calm Mind", "Protect"]).volatile(CondState::encore("Calm Mind", 2));
    sylveon.last_move = "calmmind".into();
    let b = Battle::from_state(&position(side(garchomp, sylveon), side(bystander(), bystander())))?;
    assert_eq!(moves_offered(&b, 0, 0), vec![1]);
    assert_eq!(moves_offered(&b, 0, 1), vec![1]);
    // A lock has to say which move.
    let nameless = mon("Garchomp", &["Dragon Claw"]).volatile(CondState::new("choicelock"));
    assert!(Battle::from_state(&position(side(nameless, bystander()), side(bystander(), bystander()))).is_err());
    Ok(())
}

/// Two Pokémon on 1 HP attack each other with moves that cannot miss:
/// whoever moves first is the one left standing.
fn duel(trick_room: bool) -> Result<Battle, Error> {
    let mut snorlax = mon("Snorlax", &["Body Slam"]);
    snorlax.hp = Some(1);
    let mut jolteon = mon("Jolteon", &["Thunderbolt"]);
    jolteon.hp = Some(1);
    let mut state = position(side(snorlax, bystander()), side(jolteon, bystander()));
    if trick_room {
        state.pseudo_weather.push(CondState::new("trickroom").turns(3));
    }
    let mut b = Battle::from_state(&state)?;
    let attack = Choice::Move { slot: 0, target: 1, mega: false };
    b.choose([[attack, PROTECT[0]], [attack, PROTECT[0]]])?;
    Ok(b)
}

#[test]
fn trick_room_decides_who_moves_first() -> Result<(), Error> {
    let b = duel(false)?;
    assert!(b.sides[0].team[0].fainted && !b.sides[1].team[0].fainted, "Jolteon is faster");
    let b = duel(true)?;
    assert!(!b.sides[0].team[0].fainted && b.sides[1].team[0].fainted, "under Trick Room Snorlax goes first");
    assert_eq!(b.field.pseudo.get(Pseudo::Trickroom).map(|c| c.duration), Some(2));
    Ok(())
}

#[test]
fn a_substitute_takes_the_hit() -> Result<(), Error> {
    let behind = mon("Sylveon", &["Protect", "Moonblast"]).volatile(CondState::new("substitute"));
    let attacker = mon("Jolteon", &["Thunderbolt"]);
    let mut b = Battle::from_state(&position(side(behind, bystander()), side(attacker, bystander())))?;
    let sylveon = b.active(0, 0);
    let max = b.mon(sylveon).max_hp();
    assert_eq!(b.vols(sylveon).get(VolKind::Substitute).map(|v| v.data), Some(2 * (max / 4)), "a fresh one");
    let wait = Choice::Move { slot: 1, target: 2, mega: false };
    b.choose([[wait, PROTECT[0]], [Choice::Move { slot: 0, target: 1, mega: false }, PROTECT[0]]])?;
    assert_eq!(b.mon(sylveon).hp, max);
    Ok(())
}

#[test]
fn weather_runs_out_when_its_turns_do() -> Result<(), Error> {
    let mut state = position(side(bystander(), bystander()), side(bystander(), bystander()));
    state.weather = Some(CondState::new("sandstorm").turns(1));
    let mut b = Battle::from_state(&state)?;
    assert_eq!(b.field.weather.kind, Weather::Sandstorm);
    b.choose([PROTECT, PROTECT])?;
    assert_eq!(b.field.weather.kind, Weather::None);
    assert_eq!(b.turn, 6);
    Ok(())
}

#[test]
fn replacing_the_fainted_at_the_end_of_a_turn() -> Result<(), Error> {
    let mut down = mon("Sylveon", &["Moonblast"]);
    down.fainted = true;
    let mut state = position(side(down, bystander()), side(bystander(), bystander()));
    state.request = Request::Switch;
    let mut b = Battle::from_state(&state)?;
    assert_eq!(b.legal_choices(0, 0), vec![Choice::Switch { to: 2 }, Choice::Switch { to: 3 }]);
    assert_eq!(b.legal_choices(0, 1), vec![Choice::Pass]);
    b.choose([[Choice::Switch { to: 3 }, Choice::Pass], [Choice::Pass; 2]])?;
    assert_eq!(b.mon(b.active(0, 0)).types[0], Type::Dragon, "Garchomp is in");
    assert_eq!((b.turn, b.request), (6, Request::Move), "the turn was already over");
    // The same position as a move request makes no sense.
    state.request = Request::Move;
    assert!(Battle::from_state(&state).is_err());
    Ok(())
}

/// Our left Pokémon has used U-turn and waits for its replacement; their
/// Jolteon has still to use Thunderbolt on that position.
fn after_u_turn(replacement: u8) -> Result<Battle, Error> {
    let mut leaving = mon("Scizor", &["U-turn", "Protect"]);
    leaving.switch_flag = true;
    leaving.switch_move = "uturn".into();
    let mut state = position(side(leaving, bystander()), side(mon("Jolteon", &["Thunderbolt"]), bystander()));
    state.request = Request::Switch;
    state.pending = vec![ActionState::mv(1, 0, "Thunderbolt", 1)];
    let mut b = Battle::from_state(&state)?;
    b.choose([[Choice::Switch { to: replacement }, Choice::Pass], [Choice::Pass; 2]])?;
    assert_eq!((b.turn, b.request), (6, Request::Move), "the rest of the turn was played");
    Ok(b)
}

#[test]
fn the_rest_of_a_turn_is_played_after_a_switch_in_the_middle() -> Result<(), Error> {
    let b = after_u_turn(2)?;
    let milotic = b.mon(b.active(0, 0));
    assert!(milotic.hp < milotic.max_hp(), "Milotic took the Thunderbolt meant for Scizor's place");
    let b = after_u_turn(3)?;
    let garchomp = b.mon(b.active(0, 0));
    assert_eq!(garchomp.hp, garchomp.max_hp(), "Garchomp is immune to it");
    Ok(())
}

#[test]
fn a_mega_evolution_is_once_per_side() -> Result<(), Error> {
    let garchomp = || mon("Garchomp", &["Earthquake", "Protect"]).item("Garchompite");
    let offers_mega = |b: &Battle| b.legal_choices(0, 1).iter().any(|c| matches!(c, Choice::Move { mega: true, .. }));
    let plain = mon("Charizard", &["Heat Wave", "Protect"]).item("Charizardite Y");
    let b = Battle::from_state(&position(side(plain, garchomp()), side(bystander(), bystander())))?;
    assert!(offers_mega(&b));
    let evolved = mon("Charizard-Mega-Y", &["Heat Wave", "Protect"]).item("Charizardite Y").ability("Drought");
    let b = Battle::from_state(&position(side(evolved, garchomp()), side(bystander(), bystander())))?;
    assert!(!offers_mega(&b), "Charizard has used the side's Mega Evolution");
    let charizard = b.mon(b.active(0, 0));
    assert_eq!(charizard.species, charizard.base_species, "and stays a Mega when it leaves the field");
    Ok(())
}

#[test]
fn the_first_turn_of_fly_as_the_engine_has_it() -> Result<(), Error> {
    // Play the first turn of Fly, and compare with the helper for writing that by hand.
    let mut state = position(side(mon("Charizard", &["Fly", "Protect"]), bystander()), side(bystander(), bystander()));
    let mut b = Battle::from_state(&state)?;
    b.choose([[Choice::Move { slot: 0, target: 2, mega: false }, PROTECT[0]], PROTECT])?;
    assert_eq!(b.legal_choices(0, 0).len(), 1, "it has to come down");
    let exported = b.to_state().without_bookkeeping();
    let mut volatiles = exported.sides[0].pokemon[0].volatiles.clone();
    volatiles.iter_mut().for_each(|v| {
        v.source = None;
        v.source_slot = None;
    });
    let by_hand = CondState::charging("Fly", 2);
    assert_eq!(volatiles, by_hand.to_vec());
    // And written by hand it gives the same request.
    state.turn = 6;
    state.sides[0].pokemon[0].volatiles = by_hand.to_vec();
    let hand = Battle::from_state(&state)?;
    assert_eq!(hand.legal_choices(0, 0), b.legal_choices(0, 0));
    Ok(())
}

#[test]
fn positions_that_make_no_sense_are_refused() {
    let ok = || position(side(bystander(), bystander()), side(bystander(), bystander()));
    let refused = |state: &BattleState| matches!(Battle::from_state(state), Err(Error::BadState(_)));
    assert!(Battle::from_state(&ok()).is_ok());

    let mut s = ok();
    s.sides[0].pokemon[0].species = "Missingno".into();
    assert!(refused(&s));

    let mut s = ok();
    s.sides[0].pokemon[0].hp = Some(9999);
    assert!(refused(&s));

    let mut s = ok();
    s.sides[0].pokemon[2].volatiles.push(CondState::new("taunt"));
    assert!(refused(&s), "a benched Pokémon has no volatile conditions");

    let mut s = ok();
    s.sides[0].pokemon[0].transformed = true;
    assert!(refused(&s), "transformed, but from what?");

    let mut s = ok();
    s.sides[1].pokemon.truncate(1);
    assert!(refused(&s));

    let mut s = ok();
    s.request = Request::Switch;
    assert!(refused(&s), "nobody to replace");

    let mut s = ok();
    s.sides[0].pokemon[0].ability = "Tinted Lens".into();
    assert!(matches!(Battle::from_state(&s), Err(Error::Unsupported(_))), "not in Champions");
}

/// Positions nobody would write on purpose: recorded positions with random
/// conditions thrown on top. `from_state` may refuse one, but a position it
/// accepts must be playable without the engine tripping over it.
#[test]
fn odd_positions_are_refused_or_playable() {
    let budget: usize = std::env::var("ODD_POSITIONS").ok().and_then(|n| n.parse().ok()).unwrap_or(400);
    let cases: Vec<vgc_engine::replay::Case> = include_str!("fixtures/showdown_cases.jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut pick = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n.max(1) as u64) as usize
    };
    let sides = [
        "tailwind",
        "reflect",
        "lightscreen",
        "auroraveil",
        "safeguard",
        "spikes",
        "toxicspikes",
        "stealthrock",
        "stickyweb",
    ];
    let weathers = ["raindance", "sunnyday", "sandstorm", "snowscape"];
    let terrains = ["electricterrain", "grassyterrain", "mistyterrain", "psychicterrain"];
    let rooms = ["trickroom", "gravity", "magicroom", "wonderroom", "fairylock"];
    let statuses = ["", "", "brn", "par", "psn", "tox", "slp", "frz"];
    let (mut accepted, mut refused) = (0, 0);
    for n in 0..budget {
        // A recorded battle played a few decisions in, as the canvas.
        let case = &cases[pick(cases.len())];
        let sets: Vec<Vec<PokemonSet>> =
            case.teams.iter().map(|t| t.iter().map(|s| s.to_set().unwrap()).collect()).collect();
        let mut b = Battle::new([&sets[0], &sets[1]], case.seed).unwrap();
        for step in case.steps.iter().take(pick(6)) {
            let c = [Choice::parse_side(&step.choices[0]).unwrap(), Choice::parse_side(&step.choices[1]).unwrap()];
            b.choose(c).unwrap();
        }
        if b.ended || b.request != Request::Move {
            continue;
        }
        let mut st = b.to_state().without_bookkeeping();
        for s in 0..2 {
            for p in 0..2 {
                let moves: Vec<String> = st.sides[s].pokemon[p].moves.iter().map(|m| m.id.clone()).collect();
                let m = &mut st.sides[s].pokemon[p];
                if m.fainted {
                    continue;
                }
                m.hp = None;
                m.hp_percent = Some(1.0 + pick(100) as f32);
                m.status = statuses[pick(statuses.len())].into();
                m.status_turns = pick(3) as u8;
                for k in 0..7 {
                    if pick(4) == 0 {
                        m.boosts[k] = pick(13) as i8 - 6;
                    }
                }
                for _ in 0..pick(4) {
                    let kind = VolKind::ALL[pick(VolKind::ALL.len())];
                    if m.volatiles.iter().any(|v| v.id == kind.id()) {
                        continue;
                    }
                    let mut v = CondState::new(kind.id());
                    if pick(2) == 0 {
                        v.move_id = moves[pick(moves.len())].clone();
                    }
                    if pick(2) == 0 {
                        v = v.source(pick(2) as u8, pick(2) as u8);
                    }
                    if pick(3) == 0 {
                        v = v.turns(1 + pick(4) as u8);
                    }
                    if pick(3) == 0 {
                        v.a = pick(5) as i16 - 2;
                    }
                    m.volatiles.push(v);
                }
            }
            for id in sides {
                if pick(5) == 0 && !st.sides[s].conditions.iter().any(|c| c.id == id) {
                    st.sides[s].conditions.push(CondState::new(id));
                }
            }
            for p in 0..2 {
                for id in ["wish", "futuremove", "healingwish"] {
                    if pick(8) == 0 && !st.sides[s].slot_conditions[p].iter().any(|c| c.id == id) {
                        st.sides[s].slot_conditions[p].push(CondState::new(id).source(pick(2) as u8, pick(2) as u8));
                    }
                }
            }
        }
        if pick(3) == 0 {
            st.weather = Some(CondState::new(weathers[pick(4)]).turns(1 + pick(5) as u8));
        }
        if pick(3) == 0 {
            st.terrain = Some(CondState::new(terrains[pick(4)]).turns(1 + pick(5) as u8));
        }
        for id in rooms {
            if pick(6) == 0 && !st.pseudo_weather.iter().any(|c| c.id == id) {
                st.pseudo_weather.push(CondState::new(id));
            }
        }
        let text = st.to_json();
        let outcome = std::panic::catch_unwind(move || {
            let Ok(mut b) = Battle::from_state(&st) else {
                return false;
            };
            let mut state = n as u64 * 0x9E37_79B9 + 1;
            for _ in 0..8 {
                if b.ended {
                    break;
                }
                let mut choices = [[Choice::Pass; 2]; 2];
                for (s, c) in choices.iter_mut().enumerate() {
                    let legal = b.joint_choices(s);
                    assert!(!legal.is_empty(), "nothing legal for side {s}");
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    *c = legal[(state >> 33) as usize % legal.len()];
                }
                b.choose(choices).unwrap();
            }
            true
        });
        match outcome {
            Ok(true) => accepted += 1,
            Ok(false) => refused += 1,
            Err(_) => panic!("the engine tripped over an accepted position:\n{text}"),
        }
    }
    println!("{accepted} positions accepted and played, {refused} refused");
    assert!(accepted > budget / 4, "{accepted} accepted, {refused} refused");
}
