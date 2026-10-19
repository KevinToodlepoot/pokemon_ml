//! The follower of Showdown's log against the simulator, on recorded battles.
//!
//! `tests/fixtures/followed_battles.jsonl` holds battles played in Pokémon
//! Showdown itself, with the log and the requests each player was sent
//! (`oracle/gen_cases.js --log --requests`, cut down by `followcheck
//! --slim`). Each is replayed in the simulator, and beside it a follower for
//! each side reads what Showdown sent that side. At every decision the two
//! must come to the same observation and the same legal actions, and the
//! follower must put the choice that was made back into Showdown's words.

use std::io::{BufRead, BufReader};

use vgc_engine::Choice;
use vgc_engine::env::OBS_M;
use vgc_engine::follow::Follower;
use vgc_engine::obs::{OBS_F, OBS_I, Phase};
use vgc_engine::replay::{FollowCase, FollowTally, Outcome, check_follow, walk_follow};

fn cases() -> Vec<FollowCase> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/followed_battles.jsonl");
    let file = std::fs::File::open(path).expect("the fixture is there");
    BufReader::new(file).lines().map(|line| serde_json::from_str(&line.unwrap()).expect("a readable battle")).collect()
}

/// A recording made by the oracle, with Showdown's state at each point (which it writes beside
/// the log) also nested under `state`, as a harness recorder writes it.
fn with_state(line: &str) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
    let nest = |told: &mut serde_json::Value| {
        let mut state = told.clone();
        for key in ["log", "requests", "draws"] {
            state.as_object_mut().unwrap().remove(key);
        }
        told["state"] = state;
    };
    nest(&mut v["initial"]);
    for step in v["steps"].as_array_mut().unwrap() {
        nest(&mut step["after"]);
    }
    v
}

/// D74: where a recording carries Showdown's state, the game's must equal it at the start and
/// after every step, the last included; one field edited anywhere fails the battle, naming it.
#[test]
fn the_state_is_held_to_showdowns_at_every_step() {
    let text = include_str!("fixtures/local_sweep_regressions.jsonl");
    let mut compared = 0;
    for line in text.lines() {
        let case: FollowCase = serde_json::from_value(with_state(line)).unwrap();
        assert!(case.has_every_state());
        match walk_follow(&case, &mut |_, _, _| Ok(())) {
            Outcome::Pass(_) => compared += case.states(),
            Outcome::Unsupported(why) => panic!("battle {}: {why}", case.id),
            Outcome::Fail(why) => panic!("battle {}: {}", case.id, why.join("; ")),
        }
        // The same battle without its states is followed as before.
        let plain: FollowCase = serde_json::from_str(line).unwrap();
        assert_eq!(plain.states(), 0);
        assert!(matches!(walk_follow(&plain, &mut |_, _, _| Ok(())), Outcome::Pass(_)));
    }
    assert!(compared > 50, "{compared} states compared");

    let edited = |path: &[&str], change: &dyn Fn(&serde_json::Value) -> serde_json::Value| -> Vec<String> {
        let mut v = with_state(text.lines().next().unwrap());
        let mut at = &mut v;
        for key in path {
            at = match key.parse::<usize>() {
                Ok(k) => &mut at[k],
                Err(_) => &mut at[*key],
            };
        }
        *at = change(at);
        let case: FollowCase = serde_json::from_value(v).unwrap();
        match walk_follow(&case, &mut |_, _, _| Ok(())) {
            Outcome::Fail(why) => why,
            Outcome::Pass(_) => panic!("an edited state was not caught"),
            Outcome::Unsupported(why) => panic!("{why}"),
        }
    };
    let plus_one = |x: &serde_json::Value| serde_json::json!(x.as_u64().unwrap() + 1);
    // A Pokémon's HP four steps in.
    let why = edited(&["steps", "3", "after", "state", "sides", "0", "mons", "0", "hp"], &plus_one);
    assert!(why[0].contains("step 4: the state differs") && why[0].contains(" hp: "), "{why:?}");
    // The random number generator's words at the start.
    let why = edited(&["initial", "state", "rng", "0"], &|x| serde_json::json!((x.as_u64().unwrap() + 1) % 65536));
    assert!(why[0].contains("at the start: the state differs") && why[0].contains("rng seed"), "{why:?}");
    // The last step's, where the battle ends.
    let last = text.lines().next().map(|l| serde_json::from_str::<FollowCase>(l).unwrap().steps.len()).unwrap();
    let why = edited(&["steps", &(last - 1).to_string(), "after", "state", "turn"], &plus_one);
    assert!(why[0].contains(&format!("step {last}: the state differs")) && why[0].contains("turn"), "{why:?}");
    // A point without its state: not every one carries it.
    let mut v = with_state(text.lines().next().unwrap());
    v["steps"][2]["after"].as_object_mut().unwrap().remove("state");
    let case: FollowCase = serde_json::from_value(v).unwrap();
    assert!(!case.has_every_state() && case.states() == case.steps.len());
}

/// Every field named on which a follower's export (`game_state`, what a search at play starts
/// from) and the true game disagree, at every decision of a recorded battle, from both sides, for
/// each Pokémon on the field (found by the species it was brought as). A field `a.b` compares `b`
/// of the list or object `a` (`volatiles.id`, `moves.pp`, `types_said.attacked_by`; a condition's
/// numbers only where they are public); `total_fainted` is the side's. Left out, as the gates
/// leave them out: what the player is not told (D89, the tracker's `Unsaid`), and what the turn's
/// end clears before anything reads it at a replacement request (last turn's move result, a Choice
/// lock whose item has gone).
fn export_differs(case: &FollowCase, fields: &[&str]) -> Vec<String> {
    use vgc_engine::env::Game;
    use vgc_engine::replay::{Unsaid, UnsaidTracker, hidden_disable};
    let value = |game: &Game| -> serde_json::Value { serde_json::from_str(&game.to_state().to_json()).unwrap() };
    let pick = |p: &serde_json::Value, field: &str| -> serde_json::Value {
        // A condition's numbers are public for a few kinds only (Protect's run, Ally Switch's counter,
        // Stockpile's layers and stages); the rest are hidden timers, which belief draws.
        let public = |e: &serde_json::Value, key: &str| {
            !matches!(key, "value" | "a" | "b")
                || matches!(e["id"].as_str(), Some("stall" | "allyswitch" | "stockpile"))
        };
        match field.split_once('.') {
            Some((list, key)) if p[list].is_array() => serde_json::Value::Array(
                p[list].as_array().into_iter().flatten().filter(|e| public(e, key)).map(|e| e[key].clone()).collect(),
            ),
            Some((object, key)) => p[object][key].clone(),
            None => p[field].clone(),
        }
    };
    let mut out = Vec::new();
    let mut unsaid = UnsaidTracker::default();
    let outcome = walk_follow(case, &mut |game, followers, step| {
        match step {
            0 => unsaid.start(game),
            k => unsaid.step(game, &case.steps[k - 1].after.log),
        }
        let truth = value(game);
        let switching = truth["battle"]["request"] == "switch";
        for (side, follower) in followers.iter().enumerate() {
            if hidden_disable(game, side) || follower.phase().is_none() {
                continue;
            }
            let Ok(st) = follower.game_state() else { continue };
            let export = value(&Game::from_state(&st).map_err(|e| e.to_string())?);
            let told = unsaid.for_viewer(side);
            let mut wrong = |what: String, a: &serde_json::Value, b: &serde_json::Value| {
                out.push(format!(
                    "battle {} step {step}, seen by p{}: {what}: the game's {a}, the export's {b}",
                    case.id,
                    side + 1
                ));
            };
            for s in 0..2 {
                if fields.contains(&"total_fainted") {
                    let (a, b) =
                        (&truth["battle"]["sides"][s]["total_fainted"], &export["battle"]["sides"][s]["total_fainted"]);
                    if a != b {
                        wrong(format!("p{} total_fainted", s + 1), a, b);
                    }
                }
                let theirs = export["battle"]["sides"][s]["pokemon"].as_array().cloned().unwrap_or_default();
                for p in truth["battle"]["sides"][s]["pokemon"].as_array().into_iter().flatten() {
                    if p["is_active"] != true {
                        continue;
                    }
                    let Some(q) = theirs.iter().find(|q| q["original_species"] == p["original_species"]) else {
                        continue;
                    };
                    let species = p["original_species"].as_str().unwrap_or("");
                    let choice = matches!(p["item"].as_str(), Some("choicescarf" | "choiceband" | "choicespecs"));
                    for &field in fields.iter().filter(|&&f| f != "total_fainted") {
                        let not_told = told.iter().any(|u| match u {
                            Unsaid::RampageEnded { side: us, species: sp } => {
                                *us == s && sp == species && field.starts_with("volatiles")
                            }
                            Unsaid::PressurePp { side: us, species: sp, .. } => {
                                *us == s && sp == species && field == "moves.pp"
                            }
                            Unsaid::EncoreTarget { side: us, species: sp } => {
                                *us == s && sp == species && field == "last_move_target"
                            }
                        });
                        if not_told || (switching && field == "move_last_turn") {
                            continue;
                        }
                        let (mut a, b) = (pick(p, field), pick(q, field));
                        if switching && !choice && field.starts_with("volatiles") {
                            // (A Choice lock whose item has gone: dropped at the next request, read by nothing before.)
                            let kept: Vec<bool> = p["volatiles"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|v| v["id"] != "choicelock")
                                .collect();
                            if let serde_json::Value::Array(list) = &mut a {
                                let mut k = kept.iter();
                                list.retain(|_| *k.next().unwrap_or(&true));
                            }
                        }
                        // (Berserk's `checkedBerserk`, 1 and unset, are read alike: rootcheck's rule.)
                        let unset = |v: &serde_json::Value| v.is_null() || *v == 0 || *v == 1;
                        let berserk = field == "ability_extra.a" && p["ability"] == "berserk" && unset(&a) && unset(&b);
                        if a != b && !berserk {
                            wrong(format!("p{} {species} {field}", s + 1), &a, &b);
                        }
                    }
                }
            }
        }
        Ok(())
    });
    match outcome {
        Outcome::Pass(_) => {}
        Outcome::Fail(why) => out.push(format!("battle {}: not followed: {}", case.id, why.join("; "))),
        Outcome::Unsupported(why) => out.push(format!("battle {}: unsupported: {why}", case.id)),
    }
    out
}

/// What the state gate's first run on skilled play (S3b.6, D74) found the follower's export getting
/// wrong, each in a battle scripted to reach it (Dex sets, `gen_cases.js --script`; the battle's
/// name says which): at every decision the export observes as its follower does, counts as the
/// game does, and has the game's value of every field it was found wrong in; the game is
/// Showdown's state throughout; and what no player is told (D89) is recognised, not failed.
#[test]
fn what_the_state_gates_first_run_found_the_export_getting_wrong_is_right() {
    use vgc_engine::replay::{ExportTally, check_export};
    let fields = [
        "last_item",
        "ate_berry",
        "volatiles.id",
        "volatiles.move_id",
        "volatiles.a",
        "volatiles.b",
        "volatiles.value",
        "moves.pp",
        "move_last_turn",
        "move_this_turn",
        "was_attacked",
        "times_attacked",
        "types_said.attacked_by",
        "types_said.known",
        "ability_extra.a",
        "total_fainted",
    ];
    let (mut tally, mut wrong) = (ExportTally::default(), Vec::new());
    let mut battles = 0;
    for line in include_str!("fixtures/export_regressions.jsonl").lines() {
        let case: FollowCase = serde_json::from_value(with_state(line)).unwrap();
        assert!(case.has_every_state());
        match check_export(&case, &mut tally) {
            Outcome::Pass(_) => battles += 1,
            Outcome::Unsupported(why) => panic!("battle {}: {why}", case.id),
            Outcome::Fail(why) => panic!("battle {}: {}", case.id, why.join("; ")),
        }
        wrong.extend(export_differs(&case, &fields));
    }
    assert!(battles >= 43 && tally.asked > 300, "{battles} battles, {} exports", tally.asked);
    let bad = (tally.broken, tally.observed_differently, tally.counts_differ, tally.hp_bar_differ);
    assert_eq!(bad, (0, 0, 0, 0), "{:#?}", tally.first);
    assert!(wrong.is_empty(), "{wrong:#?}");
    // The silent rampage ends in the fixture are recognised as such (D89), not missed.
    assert!(tally.unsaid.get("rampage_ended").is_some_and(|&n| n >= 3), "{:?}", tally.unsaid);
}

/// What no player is told is recognised from the game's history (D89), each kind in a battle that
/// reaches it, by the side that is not told; and a rampage ended with a word is not taken for one.
#[test]
fn what_no_player_is_told_is_recognised_exactly() {
    use vgc_engine::replay::{Unsaid, UnsaidTracker};
    let mut kinds = std::collections::BTreeMap::<&str, usize>::new();
    for line in include_str!("fixtures/export_regressions.jsonl").lines() {
        let case: FollowCase = serde_json::from_str(line).unwrap();
        let mut unsaid = UnsaidTracker::default();
        let mut seen: Vec<(usize, Unsaid)> = Vec::new();
        walk_follow(&case, &mut |game, _, step| {
            match step {
                0 => unsaid.start(game),
                k => unsaid.step(game, &case.steps[k - 1].after.log),
            }
            for side in 0..2 {
                for u in unsaid.for_viewer(side) {
                    if !seen.contains(&(side, u.clone())) {
                        seen.push((side, u));
                    }
                }
            }
            Ok(())
        });
        for (viewer, u) in seen {
            *kinds.entry(u.kind()).or_insert(0) += 1;
            match &u {
                // The other side is the one not told of a rampage's end or of PP paid to Pressure; a
                // player is not told where its own Encored move went.
                Unsaid::RampageEnded { side, .. } | Unsaid::PressurePp { side, .. } => assert_eq!(viewer, 1 - side),
                Unsaid::EncoreTarget { side, .. } => assert_eq!(viewer, *side),
            }
        }
    }
    // Three silent ends (the rampage battle and the two V5 ones), the Pressure case and the Encore
    // case; the renewed rampages (a self-hit, which the log shows) are not among them.
    assert_eq!(kinds.get("rampage_ended"), Some(&3), "{kinds:?}");
    assert!(kinds.get("pressure_pp").is_some_and(|&n| n >= 1), "{kinds:?}");
    assert!(kinds.get("encore_target").is_some_and(|&n| n >= 1), "{kinds:?}");
}

#[test]
fn solar_pressure_and_hidden_baton_suppression_regressions_match() {
    let cases: Vec<FollowCase> = include_str!("fixtures/local_300000_targeted_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 4);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        if case.id == 8 {
            for side in 0..2 {
                let mut me = follower_for(case, side);
                let opp = &case.rosters[1 - side];
                me.trust_item_clause(opp.iter().enumerate().all(|(j, a)| {
                    a.item.as_deref().is_none_or(str::is_empty) || opp[..j].iter().all(|b| b.item != a.item)
                }));
                me.lines(&Follower::own_lines(side, &case.initial.log)).unwrap();
                me.request(&case.initial.requests[side].to_string()).unwrap();
                for step in &case.steps[..8] {
                    me.lines(&Follower::own_lines(side, &step.after.log)).unwrap();
                    me.request(&step.after.requests[side].to_string()).unwrap();
                }
                let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
                me.observe(&mut f, &mut i, &mut mask).unwrap();
                if side == 0 {
                    assert_eq!(f[1231], 1.0, "the opponent cannot know suppression was silently removed");
                    me.line("|-activate|p2b: Alcremie0|ability: Battle Bond").unwrap();
                    me.request(&case.steps[7].after.requests[side].to_string()).unwrap();
                    me.observe(&mut f, &mut i, &mut mask).unwrap();
                    assert_eq!(f[1231], 0.0, "a public Battle Bond activation resolves the suppression uncertainty");
                } else {
                    assert_eq!(f[939], 0.0, "the holder knows its own unsuppressible ability");
                }
            }
        }
    }
    assert_eq!(tally.observations, 268);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn instructed_confusion_into_endure_has_matching_observations() {
    let case: FollowCase = serde_json::from_str(include_str!("fixtures/endure_confusion_regression.jsonl")).unwrap();
    let mut tally = FollowTally::default();
    assert!(matches!(check_follow(&case, &mut tally), Outcome::Pass(21)));
    assert_eq!(tally.observations, 44);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn local_300000_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/local_300000_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 10);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        let (steps, side) = match case.id {
            992 => (9, 0),
            733 => (2, 0),
            834 => (9, 0),
            970 => (11, 0),
            582 => (12, 0),
            236 => (15, 1),
            574 => (35, 1),
            328 => (6, 0),
            892 => (5, 0),
            111 => (10, 0),
            _ => unreachable!(),
        };
        let mut me = follower_for(case, side);
        let opp = &case.rosters[1 - side];
        me.trust_item_clause(
            opp.iter()
                .enumerate()
                .all(|(j, a)| a.item.as_deref().is_none_or(str::is_empty) || opp[..j].iter().all(|b| b.item != a.item)),
        );
        me.lines(&Follower::own_lines(side, &case.initial.log)).unwrap();
        me.request(&case.initial.requests[side].to_string()).unwrap();
        for step in &case.steps[..steps] {
            me.lines(&Follower::own_lines(side, &step.after.log)).unwrap();
            me.request(&step.after.requests[side].to_string()).unwrap();
        }
        let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
        me.observe(&mut f, &mut i, &mut mask).unwrap();
        match case.id {
            992 => assert_eq!(f[511], 0.0, "a pending SwitchIn cannot expose the previous Toxic counter"),
            733 => {
                assert_eq!(
                    f[542],
                    47.0 / 200.0,
                    "an instructed repeat does not reveal the unknown original queued move"
                )
            }
            834 => assert_eq!(f[268], 11.0 / 12.0, "Mega Levitate prevents spread Expanding Force Pressure costs"),
            970 => assert_eq!(f[271], 7.0 / 12.0, "the instant solar animation resolves the Pressure target"),
            582 => assert_eq!(
                i[72],
                vgc_engine::data::species_id("morpekohangry").unwrap() as i16 + vgc_engine::obs::ID_BASE
            ),
            236 => assert_eq!(
                f[435],
                101.0 / 200.0,
                "an instructed repeat cannot reveal the failed queued move's priority"
            ),
            574 => assert_eq!(f[467], 0.75, "a failed reflection does not establish ownership of Heal Pulse"),
            328 => assert_eq!(f[855], 0.0, "called Speed Swap makes the old first-mover inference unavailable"),
            892 => assert_eq!(f[434], 111.0 / 200.0, "Encore invalidates the initial Quick Claw roll"),
            111 => assert_eq!(f[1231], 0.0, "Battle Bond discards Baton Passed Gastro Acid"),
            _ => unreachable!(),
        }
    }
    assert_eq!(tally.observations, 662);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn future_sight_ripen_and_instruct_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/future_sight_ripen_instruct_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 3);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        let steps = match case.id {
            699 => 3,
            591 => 42,
            746 => 17,
            _ => unreachable!(),
        };
        let side = if case.id == 699 { 1 } else { 0 };
        let mut me = follower_for(case, side);
        let opp = &case.rosters[1 - side];
        me.trust_item_clause(
            opp.iter()
                .enumerate()
                .all(|(j, a)| a.item.as_deref().is_none_or(str::is_empty) || opp[..j].iter().all(|b| b.item != a.item)),
        );
        me.lines(&Follower::own_lines(side, &case.initial.log)).unwrap();
        me.request(&case.initial.requests[side].to_string()).unwrap();
        for step in &case.steps[..steps] {
            me.lines(&Follower::own_lines(side, &step.after.log)).unwrap();
            me.request(&step.after.requests[side].to_string()).unwrap();
        }
        let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
        me.observe(&mut f, &mut i, &mut mask).unwrap();
        match case.id {
            699 => assert_eq!(f[233], 1.0, "Future Sight's caster retains full HP when Red Card cancels recoil"),
            591 => assert_eq!(f[376], 1.0, "Ripen restores twenty PP, capped at Guard Swap's maximum"),
            746 => assert_eq!(f[489], 107.0 / 200.0, "instructed Snore does not consume the original queued action"),
            _ => unreachable!(),
        }
    }
    assert_eq!(tally.observations, 320);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn local_200000_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/local_200000_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 8);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        let steps = match case.id {
            830 => 4,
            843 => 14,
            392 => 5,
            605 => 9,
            539 => 17,
            25 => 8,
            9 => 2,
            11 => 14,
            _ => panic!("unexpected regression case"),
        };
        let side = if case.id == 25 { 1 } else { 0 };
        let mut me = follower_for(case, side);
        let opp = &case.rosters[1 - side];
        me.trust_item_clause(
            opp.iter()
                .enumerate()
                .all(|(j, a)| a.item.as_deref().is_none_or(str::is_empty) || opp[..j].iter().all(|b| b.item != a.item)),
        );
        me.lines(&Follower::own_lines(side, &case.initial.log)).unwrap();
        me.request(&case.initial.requests[side].to_string()).unwrap();
        for step in &case.steps[..steps] {
            me.lines(&Follower::own_lines(side, &step.after.log)).unwrap();
            me.request(&step.after.requests[side].to_string()).unwrap();
        }
        let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
        me.observe(&mut f, &mut i, &mut mask).unwrap();
        match case.id {
            830 => assert_eq!(f[651], 145.0 / 200.0, "Attract after waking preserves Quick Feet queue evidence"),
            843 => assert_eq!(f[40], 4.0 / 8.0, "Court Change preserves Reflect's elapsed age"),
            392 => {
                assert_eq!(f[949], 1.0, "a refused reflected heal does not cancel charging");
                assert_eq!(f[957], 1.0, "Solar Blade remains prepared");
            }
            605 => assert_eq!(f[543], 97.0 / 200.0, "an instructed Quick Claw roll cannot promote the queued move"),
            539 => {
                assert_eq!(f[683], 0.75, "lost-focus cant reveals the unmasked Pokémon's move");
                assert_eq!(i[94], vgc_engine::data::move_id("focuspunch").unwrap() as i16 + vgc_engine::obs::ID_BASE);
            }
            25 => assert_eq!(f[435], 118.0 / 200.0, "a Cheri Berry cure is not waking from sleep"),
            9 => assert_eq!(f[596], 141.0 / 200.0, "lost-focus order supplies Speed evidence"),
            11 => assert_eq!(f[650], 117.0 / 200.0, "lost focus after a pivot preserves queued priority"),
            _ => unreachable!(),
        }
    }
    assert_eq!(tally.observations, 468);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn illusion_residual_and_guard_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/illusion_residual_guard_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 4);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        let (steps, side) = match case.id {
            242 => (30, 1),
            735 => (33, 0),
            632 => (8, 0),
            178 => (2, 0),
            _ => panic!("unexpected regression case"),
        };
        let mut me = follower_for(case, side);
        // Stress rosters may break Item Clause; match the replay's assumptions.
        let opp = &case.rosters[1 - side];
        me.trust_item_clause(
            opp.iter()
                .enumerate()
                .all(|(j, a)| a.item.as_deref().is_none_or(str::is_empty) || opp[..j].iter().all(|b| b.item != a.item)),
        );
        me.lines(&Follower::own_lines(side, &case.initial.log)).unwrap();
        me.request(&case.initial.requests[side].to_string()).unwrap();
        for step in &case.steps[..steps] {
            me.lines(&Follower::own_lines(side, &step.after.log)).unwrap();
            me.request(&step.after.requests[side].to_string()).unwrap();
        }
        let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
        me.observe(&mut f, &mut i, &mut mask).unwrap();
        match case.id {
            242 => {
                assert_eq!(
                    i[56],
                    vgc_engine::data::species_id("aegislashblade").unwrap() as i16 + vgc_engine::obs::ID_BASE
                );
                assert_eq!(f[1004], 0.0, "the disguised ally's faint leaves this position empty");
            }
            735 => {
                assert_eq!(f[650], 36.0 / 200.0, "ambiguous item loss cannot exclude the real Aegislash's speed items");
                assert_eq!(f[651], 184.0 / 200.0);
            }
            632 => assert_eq!(f[10], 0.0, "rain inherited after Toxic has not aged"),
            178 => assert_eq!(f[37], 0.0, "Feint clears Quick Guard before the forced replacement"),
            _ => unreachable!(),
        }
    }
    assert_eq!(tally.observations, 368);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn locally_saved_sweep_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/local_sweep_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 5);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        // Check the reference behavior directly, as well as engine/client parity.
        let steps = match case.id {
            232 => 5,
            857 => 43,
            266 => 16,
            472 | 565 => 3,
            _ => panic!("unexpected local sweep case"),
        };
        let mut me = follower_for(case, 0);
        me.lines(&Follower::own_lines(0, &case.initial.log)).unwrap();
        me.request(&case.initial.requests[0].to_string()).unwrap();
        for step in &case.steps[..steps] {
            me.lines(&Follower::own_lines(0, &step.after.log)).unwrap();
            me.request(&step.after.requests[0].to_string()).unwrap();
        }
        let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
        me.observe(&mut f, &mut i, &mut mask).unwrap();
        match case.id {
            232 => assert_eq!(f[378], -1.0, "Armor Tail hides the rerolled target's Pressure cost"),
            857 => {
                assert_eq!(f[543], 141.0 / 200.0, "waking through confusion preserves the Speed bound");
                assert_eq!(f[544], 0.0, "the observed ordering excludes speed-modifying items");
            }
            266 => assert_eq!(f[705], 111.0 / 200.0, "a cured Encore permits Speed evidence"),
            472 => assert_eq!(f[217], 1.0, "Choice refusal does not spend copied Sleep Talk PP"),
            565 => assert_eq!(
                i[26],
                vgc_engine::data::item_id("sitrusberry").unwrap() as i16 + vgc_engine::obs::ID_BASE,
                "an empty Poltergeist hit keeps the lost berry"
            ),
            _ => unreachable!(),
        }
    }
    assert_eq!(tally.observations, 406);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn local_100000_and_targeted_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/local_100000_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 13);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        if matches!(case.id, 956 | 381) {
            let mut me = follower_for(case, 0);
            me.lines(&Follower::own_lines(0, &case.initial.log)).unwrap();
            me.request(&case.initial.requests[0].to_string()).unwrap();
            let steps = if case.id == 956 { 2 } else { 7 };
            for step in &case.steps[..steps] {
                me.lines(&Follower::own_lines(0, &step.after.log)).unwrap();
                me.request(&step.after.requests[0].to_string()).unwrap();
            }
            if case.id == 956 {
                let b = me.battle().unwrap();
                let m = b.mon(b.active(0, 1));
                assert_eq!(m.species, vgc_engine::data::species_id("castformsnowy").unwrap());
                assert_eq!(
                    m.types,
                    [vgc_engine::data::Type::Normal, vgc_engine::data::Type::None],
                    "Mimicry restores original types without changing the Forecast forme"
                );
            } else {
                let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
                me.observe(&mut f, &mut i, &mut mask).unwrap();
                assert_eq!(
                    i[73],
                    vgc_engine::data::item_id("whiteherb").unwrap() as i16 + vgc_engine::obs::ID_BASE,
                    "the replacement White Herb remains held after the original was consumed"
                );
            }
        }
    }
    assert_eq!(tally.observations, 773);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn a_follower_sees_what_the_simulator_sees() {
    let mut tally = FollowTally::default();
    let (mut battles, mut decisions) = (0, 0);
    for case in cases() {
        match check_follow(&case, &mut tally) {
            Outcome::Pass(n) => {
                battles += 1;
                decisions += n;
            }
            Outcome::Unsupported(why) => panic!("battle {} cannot be checked: {why}", case.id),
            Outcome::Fail(why) => panic!("battle {} was not followed: {}", case.id, why.join("; ")),
        }
    }
    assert!(battles >= 20 && decisions > 300, "{battles} battles, {decisions} decisions");
    assert!(tally.observations > 600, "{} observations", tally.observations);
    assert_eq!(tally.wrong, 0, "observations differ: {:#?}", tally.by_feature);
    // Which counts (obs-2) these battles reached: what the comparison above has checked.
    eprintln!("counts above 0: {:?}", tally.counted);
}

/// What gate 2 and the random fuzz found the follower getting wrong at Showdown `7b02eb3` (S3b.6, the
/// bump of vgc-bot's D91), each in the battle that showed it (Dex sets; the battle's name says which),
/// recorded again to just past it with Showdown's state at every point: a fainted Pokémon that keeps
/// its status, Octolock's user, a rampage's turn cap across a mid-turn switch, a rampage ended by
/// sleep, a disguise's sleep count, No Retreat without a word, and two rampages that run out
/// silently (D89), told to neither side for the confused holder at its replacement request.
#[test]
fn what_the_new_pin_and_random_play_found_the_follower_getting_wrong_is_right() {
    use vgc_engine::replay::{ExportTally, Unsaid, UnsaidTracker, check_export};
    let (mut tally, mut exports) = (FollowTally::default(), ExportTally::default());
    let mut silent = Vec::new();
    let text = include_str!("fixtures/new_pin_export_regressions.jsonl");
    for line in text.lines() {
        let case: FollowCase = serde_json::from_value(with_state(line)).unwrap();
        assert!(case.has_every_state());
        for outcome in [check_follow(&case, &mut tally), check_export(&case, &mut exports)] {
            match outcome {
                Outcome::Pass(_) => {}
                Outcome::Unsupported(why) => panic!("battle {} cannot be checked: {why}", case.id),
                Outcome::Fail(why) => panic!("battle {} was not followed: {}", case.id, why.join("; ")),
            }
        }
        let mut unsaid = UnsaidTracker::default();
        walk_follow(&case, &mut |game, _, step| {
            match step {
                0 => unsaid.start(game),
                k => unsaid.step(game, &case.steps[k - 1].after.log),
            }
            for viewer in 0..2 {
                for u in unsaid.for_viewer(viewer) {
                    if let Unsaid::RampageEnded { side, species } = u
                        && !silent.contains(&(case.id, viewer, side, species.clone()))
                    {
                        silent.push((case.id, viewer, side, species));
                    }
                }
            }
            Ok(())
        });
    }
    assert_eq!(text.lines().count(), 8);
    assert_eq!(tally.wrong, 0, "observations differ: {:#?}", tally.by_feature);
    let bad = (exports.broken, exports.observed_differently, exports.counts_differ, exports.hp_bar_differ);
    assert_eq!(bad, (0, 0, 0, 0), "{:#?}", exports.first);
    // The Own Tempo Emboar's and the confused Annihilape's: each is followed by a replacement request
    // (a wait for the side that does not replace), so neither side is told, its own included.
    for (id, species) in [(801, "emboar"), (1970, "annihilape")] {
        let mut own: Vec<bool> = silent.iter().filter(|s| s.0 == id && s.3 == species).map(|s| s.1 == s.2).collect();
        own.sort();
        assert_eq!(own, [false, true], "battle {id}: {silent:?}");
    }
}

/// The counts no other recording reaches (S3b.6): Lock-On's, Syrup Bomb's and Future Sight's, in
/// random battles over those moves with Showdown's state at every point (`gen_cases.js --n 120
/// --seed 20261018 --moves lockon,zapcannon,syrupbomb,futuresight --log --requests --open-sheets`,
/// the three in which a picked Pokémon knows Lock-On and one knows Syrup Bomb, no team holds a
/// species twice (an export needs the species clause), and none knows Endure, which Showdown's
/// Future Sight passes over at the oracle pin, D88): each is followed, observed and exported as
/// the game has it, and each count is above 0 somewhere.
#[test]
fn lock_on_syrup_bomb_and_future_sight_are_counted_as_the_game_counts_them() {
    use vgc_engine::replay::{ExportTally, check_export};
    let (mut tally, mut exports) = (FollowTally::default(), ExportTally::default());
    for line in include_str!("fixtures/follow_rare_counts.jsonl").lines() {
        let case: FollowCase = serde_json::from_value(with_state(line)).unwrap();
        assert!(case.has_every_state());
        for outcome in [check_follow(&case, &mut tally), check_export(&case, &mut exports)] {
            match outcome {
                Outcome::Pass(_) => {}
                Outcome::Unsupported(why) => panic!("battle {} cannot be checked: {why}", case.id),
                Outcome::Fail(why) => panic!("battle {} was not followed: {}", case.id, why.join("; ")),
            }
        }
    }
    assert_eq!(tally.wrong, 0, "observations differ: {:#?}", tally.by_feature);
    for count in ["position: lockon turns", "position: syrupbomb turns", "position: slot futuremove turns"] {
        assert!(tally.counted.get(count).is_some_and(|&n| n > 0), "{count} not reached: {:?}", tally.counted);
    }
    assert!(exports.asked > 100, "{} exports", exports.asked);
    let bad = (exports.broken, exports.observed_differently, exports.counts_differ, exports.hp_bar_differ);
    assert_eq!(bad, (0, 0, 0, 0), "{:#?}", exports.first);
}

/// The same from the outside, as a client uses it: lines and requests in,
/// an observation and a reply out.
#[test]
fn a_battle_is_followed_from_first_line_to_last() {
    let case = &cases()[0];
    let team = case.rosters[0].iter().map(|s| s.to_set().unwrap()).collect::<Vec<_>>();
    let mut me = Follower::new(0, team).unwrap();
    let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);

    // Team Preview: the other side's six are on show, and any of the 90 picks is open.
    let cut = case.initial.log.iter().position(|l| l.starts_with("|teamsize|")).unwrap();
    me.lines(&Follower::own_lines(0, &case.initial.log[..cut])).unwrap();
    assert_eq!(me.observe(&mut f, &mut i, &mut mask).unwrap(), 90);
    assert_eq!(me.choice([0, 0]).unwrap(), "team 1234");

    me.lines(&Follower::own_lines(0, &case.initial.log[cut..])).unwrap();
    me.request(&case.initial.requests[0].to_string()).unwrap();
    assert_eq!((me.phase(), me.turn()), (Some(Phase::Move), 1));
    let mut asked = 0;
    for step in &case.steps {
        // Whatever is legal can be put into Showdown's words.
        let legal = me.observe(&mut f, &mut i, &mut mask).unwrap();
        if legal > 0 {
            let first = mask[..47].iter().position(|&m| m == 1).unwrap();
            let second = mask[47 * (1 + first)..47 * (2 + first)].iter().position(|&m| m == 1).unwrap();
            let reply = me.choice([first, second]).unwrap();
            assert!(reply.split(", ").count() == 2, "{reply}");
            asked += 1;
        }
        me.lines(&Follower::own_lines(0, &step.after.log)).unwrap();
        me.request(&step.after.requests[0].to_string()).unwrap();
    }
    assert!(me.ended() && asked > 5);
    let last = case.steps.last().unwrap().after.log.iter().rev().find(|l| l.starts_with("|win|")).unwrap();
    assert_eq!(me.winner(), Some(if last == "|win|P1" { 0 } else { 1 }));
}

/// A follower for one side of a recorded battle. (The recorder names each
/// Pokémon, and its teams can have a species twice.)
fn follower_for(case: &FollowCase, side: usize) -> Follower {
    let team = case.rosters[side].iter().map(|s| s.to_set().unwrap()).collect::<Vec<_>>();
    let mut names = vec![String::new(); team.len()];
    let listed = case.initial.requests[side]["side"]["pokemon"].as_array().unwrap();
    for (k, mon) in listed.iter().enumerate() {
        names[case.picks[side][k]] = mon["ident"].as_str().unwrap().split_once(": ").unwrap().1.to_string();
    }
    Follower::new(side, team).unwrap().with_names(names)
}

fn hidden_move_cases() -> Vec<FollowCase> {
    include_str!("fixtures/hidden_move_information.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// All 41 failures retained from the fresh 50,000-battle sweep and two
/// regressions found while replaying the fixes. These include
/// interrupted Round chains, Encore retargeting, item theft, and Mimicry resets.
#[test]
fn saved_sweep_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/sweep_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 43);
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
    }
    assert_eq!(tally.observations, 2617);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn cured_encore_and_allied_skill_swap_preserve_speed_evidence() {
    let cases: Vec<FollowCase> = include_str!("fixtures/speed_knowledge.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut tally = FollowTally::default();
    for case in &cases {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
        let (steps, feature, expected) = match case.id {
            871 => (11, 488, 114.0 / 200.0),
            115 => (14, 435, 89.0 / 200.0),
            _ => panic!("unexpected regression case"),
        };
        let mut me = follower_for(case, 1);
        me.lines(&Follower::own_lines(1, &case.initial.log)).unwrap();
        me.request(&case.initial.requests[1].to_string()).unwrap();
        for step in &case.steps[..steps] {
            me.lines(&Follower::own_lines(1, &step.after.log)).unwrap();
            me.request(&step.after.requests[1].to_string()).unwrap();
        }
        let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
        me.observe(&mut f, &mut i, &mut mask).unwrap();
        assert_eq!(f[feature], expected, "case {} retains the observed Speed bound", case.id);
    }
    assert_eq!(cases.len(), 2);
    assert_eq!(tally.observations, 124);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn hidden_choice_refusals_and_pressure_targets_have_matching_observations() {
    let mut tally = FollowTally::default();
    for case in hidden_move_cases() {
        assert!(matches!(check_follow(&case, &mut tally), Outcome::Pass(_)), "case {}", case.id);
    }
    assert!(tally.observations >= 300);
    assert_eq!(tally.wrong, 0, "{:?}", tally.by_feature);
}

#[test]
fn a_wait_request_keeps_pressure_pp_unknown_until_an_exact_request() {
    let case = hidden_move_cases().into_iter().find(|c| c.id == 586).unwrap();
    let mut me = follower_for(&case, 1);
    me.lines(&Follower::own_lines(1, &case.initial.log)).unwrap();
    me.request(&case.initial.requests[1].to_string()).unwrap();
    for step in &case.steps[..3] {
        me.lines(&Follower::own_lines(1, &step.after.log)).unwrap();
        me.request(&step.after.requests[1].to_string()).unwrap();
    }
    assert_eq!(case.steps[2].after.requests[1]["wait"], true);
    let b = me.battle().unwrap();
    assert_eq!(b.mon(b.active(1, 0)).unknown_pp, 1, "Comeuppance's count is uncertain");
    let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
    me.observe(&mut f, &mut i, &mut mask).unwrap();
    assert_eq!(f[376], -1.0, "do not expose an inferred PP count as exact");
    let step = &case.steps[3];
    me.lines(&Follower::own_lines(1, &step.after.log)).unwrap();
    me.request(&step.after.requests[1].to_string()).unwrap();
    let b = me.battle().unwrap();
    let mon = b.mon(b.active(1, 0));
    assert_eq!((mon.unknown_pp, mon.moves[0].pp, mon.moves[0].maxpp), (0, 8, 12));
    me.observe(&mut f, &mut i, &mut mask).unwrap();
    assert_eq!(f[376], 8.0 / 12.0);
}

/// The last of a side to choose is told only that it *may* be trapped, when
/// a foe could be holding it in by an ability not yet shown. It may try to
/// switch; if it cannot, Showdown sends the request again saying so.
#[test]
fn a_request_put_right_is_followed() {
    let switches = |me: &Follower, pos: usize| {
        let b = me.battle().unwrap();
        b.legal_choices(me.side(), pos).iter().any(|c| matches!(c, Choice::Switch { .. }))
    };
    let observed = |me: &Follower| {
        let (mut f, mut i, mut mask) = (vec![0.0; OBS_F], vec![0; OBS_I], vec![0; OBS_M]);
        me.observe(&mut f, &mut i, &mut mask).unwrap();
        (f, i, mask)
    };
    let mut met = 0;
    for case in cases() {
        for side in 0..2 {
            let mut me = follower_for(&case, side);
            me.lines(&Follower::own_lines(side, &case.initial.log)).unwrap();
            me.request(&case.initial.requests[side].to_string()).unwrap();
            for step in &case.steps {
                me.lines(&Follower::own_lines(side, &step.after.log)).unwrap();
                let request = &step.after.requests[side];
                me.request(&request.to_string()).unwrap();
                let unsure =
                    request["active"].as_array().and_then(|a| a.iter().position(|m| m["maybeTrapped"] == true));
                let bench = request["side"]["pokemon"].as_array().is_some_and(|mons| {
                    mons.iter().skip(2).any(|m| !m["condition"].as_str().unwrap_or("fnt").ends_with("fnt"))
                });
                let (Some(pos), true) = (unsure, bench) else { continue };
                // As far as it has been told, it can switch.
                assert!(switches(&me, pos), "battle {}", case.id);
                // The same request a second time changes nothing.
                let before = observed(&me);
                me.request(&request.to_string()).unwrap();
                assert!(before == observed(&me), "battle {}", case.id);
                // Put right: it is trapped, and the switch is gone.
                let mut fixed = request.clone();
                let active = fixed["active"][pos].as_object_mut().unwrap();
                active.remove("maybeTrapped");
                active.insert("trapped".into(), true.into());
                let mut told = me.clone();
                told.request(&fixed.to_string()).unwrap();
                assert!(!switches(&told, pos), "battle {}", case.id);
                assert!(before != observed(&told), "battle {}", case.id);
                met += 1;
            }
        }
    }
    assert!(met > 0, "no battle of the fixture has a Pokémon that may be trapped");
}

/// All twelve fresh sweep failures plus the tournament-valid single-Zoroark
/// reproduction. Keep entire recordings to catch persistent inference errors.
#[test]
fn fresh_sweep_failures_have_matching_observations() {
    let cases: Vec<FollowCase> = include_str!("fixtures/fresh_sweep_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 13);
    let mut tally = FollowTally::default();
    for case in &cases {
        let mut individual = FollowTally::default();
        assert!(matches!(check_follow(case, &mut individual), Outcome::Pass(_)), "case {}", case.id);
        assert_eq!(individual.wrong, 0, "case {}: {:?}", case.id, individual.first_wrong);
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)));
    }
    assert_eq!(tally.observations, 911);
    assert_eq!(tally.wrong, 0);
}

/// Equality alone missed this before: both implementations restored stale types.
/// The public Soak line establishes Water after Reflect Type, with no silent reset.
#[test]
fn explicit_soak_supersedes_reflect_type_knowledge() {
    let case: FollowCase = include_str!("fixtures/sweep_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<FollowCase>(line).unwrap())
        .find(|case| case.id == 716)
        .unwrap();
    let mut me = follower_for(&case, 1);
    me.lines(&Follower::own_lines(1, &case.initial.log)).unwrap();
    me.request(&case.initial.requests[1].to_string()).unwrap();
    for step in &case.steps[..16] {
        me.lines(&Follower::own_lines(1, &step.after.log)).unwrap();
        me.request(&step.after.requests[1].to_string()).unwrap();
    }
    let b = me.battle().unwrap();
    assert_eq!(b.mon(b.active(1, 1)).types, [vgc_engine::data::Type::Water, vgc_engine::data::Type::None]);
}

#[test]
fn private_items_track_symbiosis_transfers_between_requests() {
    let cases: Vec<FollowCase> = include_str!("fixtures/item_transfer_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 21);
    let mut tally = FollowTally::default();
    for case in &cases {
        let mut individual = FollowTally::default();
        assert!(matches!(check_follow(case, &mut individual), Outcome::Pass(_)), "case {}", case.id);
        assert_eq!(individual.wrong, 0, "case {}: {:?}", case.id, individual.first_wrong);
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)));
    }
    assert_eq!(tally.observations, 1061);
    assert_eq!(tally.wrong, 0);
}

/// What a follower exports for a search from its side (`Follower::game_state`), on every
/// recorded battle these tests follow: at every decision it loads, both sides' choices can
/// be listed, it observes for its side as the follower does, and its counts of
/// hidden-length effects are the game's. What it cannot export is refused with a reason:
/// a Pokémon that may be in disguise, a switch in the middle of a turn.
#[test]
fn a_followers_game_state_loads_observes_as_it_does_and_counts_as_the_game_does() {
    use vgc_engine::replay::{ExportTally, check_export};
    let files = [
        "followed_battles", "endure_confusion_regression", "fresh_sweep_regressions",
        "future_sight_ripen_instruct_regressions", "hidden_move_information", "illusion_residual_guard_regressions",
        "item_transfer_regressions", "local_100000_regressions", "local_200000_regressions", "local_300000_regressions",
        "local_300000_targeted_regressions", "local_sweep_regressions", "speed_knowledge", "sweep_regressions",
    ];
    let mut tally = ExportTally::default();
    let mut battles = 0;
    for name in files {
        let path = format!("{}/tests/fixtures/{name}.jsonl", env!("CARGO_MANIFEST_DIR"));
        let file = std::fs::File::open(&path).expect("the fixture is there");
        for line in BufReader::new(file).lines() {
            let Ok(case) = serde_json::from_str::<FollowCase>(&line.unwrap()) else { continue };
            match check_export(&case, &mut tally) {
                Outcome::Pass(_) => battles += 1,
                Outcome::Unsupported(_) => {}
                Outcome::Fail(why) => panic!("{name}: battle {} was not followed: {}", case.id, why.join("; ")),
            }
        }
    }
    assert!(battles >= 40 && tally.asked > 1000, "{battles} battles, {} exports", tally.asked);
    assert_eq!(tally.broken, 0, "{:#?}", tally.first);
    // These fixtures are random play kept for the follower's hardest cases: what a search exports
    // there observes as its follower does and counts as the game does, every time (D74; before
    // S3b.6 up to 1% were let through).
    let exported = tally.asked - tally.refused.values().sum::<usize>();
    let (seen, counted) = (tally.observed_differently, tally.counts_differ);
    assert_eq!((seen, counted), (0, 0), "of {exported}: {:#?}", tally.first);
    // The other side's HP: every export shows the percentage and bar the follower was told, HP
    // points included where a bar at exactly 20% or 50% needs them (S3.4).
    assert_eq!(tally.hp_bar_differ, 0, "{:#?}", tally.first);
    println!("observed differently {seen} and counts differ {counted} of {exported} exports");
    let known =
        ["Illusion", "Transform", "a switch in the middle of a turn", "closed team sheets", "waiting for the other side"];
    assert!(tally.refused.keys().all(|why| known.contains(&why.as_str())), "{:?}", tally.refused);
    println!("{battles} battles, {} exports, refused {:?}", tally.asked, tally.refused);
}

#[test]
fn the_other_sides_pp_follows_spite_eerie_spell_and_leppa_berry() {
    // D84: PP given back by a Leppa Berry, or taken by Spite or Eerie Spell, is counted for the
    // other side's Pokémon too, so the export has it as the game does.
    let (mut lines, mut checked) = (0, 0);
    // (Battles with open sheets, where the export can be asked for, that have such a line.)
    let recorded: Vec<FollowCase> = [
        include_str!("fixtures/sweep_regressions.jsonl"),
        include_str!("fixtures/illusion_residual_guard_regressions.jsonl"),
        include_str!("fixtures/item_transfer_regressions.jsonl"),
        include_str!("fixtures/fresh_sweep_regressions.jsonl"),
    ]
    .iter()
    .flat_map(|text| text.lines())
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
    for case in recorded.iter().filter(|c| c.open_sheets) {
        // Each such line: the step it came in, and who (side, position) and which move it was about.
        let marks: Vec<(usize, usize, usize, u16)> = case
            .steps
            .iter()
            .enumerate()
            .flat_map(|(k, step)| step.after.log.iter().map(move |l| (k, l)))
            .filter_map(|(k, l)| {
                let p: Vec<&str> = l.split('|').collect();
                let what = p.get(3).copied()?;
                if p.get(1) != Some(&"-activate")
                    || !matches!(what, "item: Leppa Berry" | "move: Spite" | "move: Eerie Spell")
                {
                    return None;
                }
                let who = p.get(2)?;
                let side = if who.starts_with("p1") { 0 } else { 1 };
                let pos = (who.as_bytes()[2] - b'a') as usize;
                let mv = vgc_engine::data::move_id(&p.get(4)?.to_lowercase().replace([' ', '-'], ""))?;
                Some((k + 1, side, pos, mv))
            })
            .collect();
        lines += marks.len();
        let outcome = walk_follow(case, &mut |game, followers, step| {
            for &(at, side, pos, mv) in marks.iter().filter(|m| m.0 == step) {
                // The other side's follower, whose export has this Pokémon as the log showed it.
                let Ok(st) = followers[1 - side].game_state() else { continue };
                let export = vgc_engine::env::Game::from_state(&st).map_err(|e| e.to_string())?;
                let (truth, theirs) = (game.battle().unwrap(), export.battle().unwrap());
                let pp = |b: &vgc_engine::Battle| {
                    let m = b.mon(b.active(side, pos));
                    m.moves[..m.n_moves as usize].iter().find(|s| s.id == mv).map(|s| s.pp)
                };
                if let (Some(a), Some(b)) = (pp(truth), pp(theirs)) {
                    assert_eq!(b, a, "battle {} step {at}: the export's PP for move {mv} on p{}", case.id, side + 1);
                    checked += 1;
                }
            }
            Ok(())
        });
        assert!(matches!(outcome, Outcome::Pass(_)), "battle {}", case.id);
    }
    eprintln!("PP changes on the other side: {lines} lines, {checked} checked against the game");
    assert!(lines >= 6 && checked >= 4, "{lines} lines, {checked} checked");
}

#[test]
fn a_line_of_a_kind_the_follower_does_not_know_is_counted() {
    // D84: nothing is read past unseen. Over the recorded battles every kind is one the follower
    // reads or passes over on purpose; a kind it has never met is counted, by kind.
    let mut tally = FollowTally::default();
    for case in &cases() {
        assert!(matches!(check_follow(case, &mut tally), Outcome::Pass(_)), "battle {}", case.id);
    }
    assert!(tally.unknown.is_empty(), "lines of a kind the follower does not know: {:?}", tally.unknown);
    let case = &cases()[0];
    let mut me = follower_for(case, 0);
    me.lines(&Follower::own_lines(0, &case.initial.log)).unwrap();
    me.line("|-somethingnew|p1a: Whoever|what").unwrap();
    me.line("|-somethingnew|p2a: Whoever|what").unwrap();
    assert_eq!(me.unknown().get("-somethingnew"), Some(&2));
}
