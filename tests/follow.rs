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
        // The oracle's own form, the snapshot written flat on each point, carries the same states.
        let flat: FollowCase = serde_json::from_str(line).unwrap();
        assert!(flat.has_every_state() && flat.states() == case.states());
        assert!(matches!(walk_follow(&flat, &mut |_, _, _| Ok(())), Outcome::Pass(_)));
        // Slimmed to what the follower reads, it has none, and is followed as before.
        let slim: FollowCase = serde_json::from_str(&serde_json::to_string(&flat).unwrap()).unwrap();
        assert_eq!(slim.states(), 0);
        assert!(matches!(walk_follow(&slim, &mut |_, _, _| Ok(())), Outcome::Pass(_)));
    }
    assert!(compared > 50, "{compared} states compared");
    // An edit to the flat form is caught as one to the nested form is.
    let mut v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let hp = &mut v["steps"][3]["after"]["sides"][0]["mons"][0]["hp"];
    *hp = serde_json::json!(hp.as_u64().unwrap() + 1);
    let case: FollowCase = serde_json::from_value(v).unwrap();
    match walk_follow(&case, &mut |_, _, _| Ok(())) {
        Outcome::Fail(why) => {
            assert!(why[0].contains("step 4: the state differs") && why[0].contains(" hp: "), "{why:?}")
        }
        _ => panic!("an edited flat state was not caught"),
    }
    // A point without its log is refused by name.
    let mut v: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    v["steps"][1]["after"].as_object_mut().unwrap().remove("log");
    let err = serde_json::from_value::<FollowCase>(v).unwrap_err().to_string();
    assert!(err.contains("missing field `log`"), "{err}");

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
    // A point without its state (nested or flat, only what a follower reads): not every one carries it.
    let mut v = with_state(text.lines().next().unwrap());
    let after = &mut v["steps"][2]["after"];
    *after = serde_json::json!({"ended": after["ended"], "log": after["log"], "requests": after["requests"]});
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
            k => unsaid.step(game, &case.steps[k - 1].after.log, &case.steps[k - 1].choices),
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
                            Unsaid::Rampage { side: us, species: sp } | Unsaid::ChargeTarget { side: us, species: sp } => {
                                *us == s && sp == species && field.starts_with("volatiles")
                            }
                            Unsaid::PressurePp { side: us, species: sp, .. } => {
                                *us == s && sp == species && field == "moves.pp"
                            }
                            Unsaid::EncoreTarget { side: us, species: sp, .. } => {
                                *us == s && sp == species && field == "last_move_target"
                            }
                            Unsaid::StatsShared { side: us, species: sp } => *us == s && sp == species && field == "stats",
                            Unsaid::MoveResult { side: us, species: sp, this_turn } => {
                                *us == s
                                    && sp == species
                                    && field == if *this_turn { "move_this_turn" } else { "move_last_turn" }
                            }
                            Unsaid::ItemTaken { side: us, species: sp, other_side, other_species, .. } => {
                                field == "last_item"
                                    && ((*us == s && sp == species) || (*other_side == s && other_species == species))
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

/// Every field named that a search from an export reads, export against game, at every decision of a recorded
/// battle, from both sides (S3b.6c, what rootcheck found): for each Pokémon on the field (found by the species it
/// was brought as), and each side's `conditions` and `slot_conditions` where named. A Pokémon is named by its
/// species wherever a field points at one (an attacker, a condition's maker), as the export numbers the other side
/// by appearance and the game by pick order. As rootcheck has them: a condition's numbers only where they are
/// public (`pick` in `export_differs`), its maker only for the kinds a handler reads it of (rootcheck's
/// `SOURCE_READ`, and a substitute's, which belief sizes it from); the other side's stats (its hidden spread's) and
/// its aim (`last_move_target`, a choice it is not shown) left out. What lasts the battle (`last_item`, `ate_berry`,
/// `syrup_triggered`, `moves.pp`) is compared on the bench too. Left out too: the field each entry of the export's
/// list of what its player cannot tell is about (`untold`, D89, D94; read as JSON), and what the turn's end clears
/// before anything reads it at a replacement request (last turn's move result, a Choice lock whose item has gone).
fn root_state_differs(case: &FollowCase, fields: &[&str]) -> Vec<String> {
    use vgc_engine::env::Game;
    use vgc_engine::replay::hidden_disable;
    const SOURCE_READ: [&str; 11] = [
        "attract",
        "octolock",
        "syrupbomb",
        "yawn",
        "partiallytrapped",
        "imprison",
        "lockon",
        "leechseed",
        "trapped",
        "trapper",
        "substitute",
    ];
    let value = |game: &Game| -> serde_json::Value { serde_json::from_str(&game.to_state().to_json()).unwrap() };
    // A Pokémon a field points at, by its species in the same state (`side`, `pokemon`: its place in its side's list).
    fn named(state: &serde_json::Value, side: usize, v: &serde_json::Value) -> serde_json::Value {
        let k = v.as_u64().unwrap_or(u64::MAX) as usize;
        state["battle"]["sides"][side]["pokemon"].get(k).map_or(v.clone(), |p| p["original_species"].clone())
    }
    fn pointers(state: &serde_json::Value, v: &mut serde_json::Value, foe: usize) {
        match v {
            serde_json::Value::Object(o) => {
                if let (Some(s), Some(k)) = (o.get("side").and_then(|s| s.as_u64()), o.get("pokemon").cloned()) {
                    o.insert("pokemon".into(), named(state, s as usize, &k));
                }
                for (_, x) in o.iter_mut() {
                    pointers(state, x, foe);
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| pointers(state, x, foe)),
            _ => {}
        }
    }
    let pick = |state: &serde_json::Value, p: &serde_json::Value, field: &str, s: usize| -> serde_json::Value {
        let public = |e: &serde_json::Value, key: &str| {
            (!matches!(key, "value" | "a" | "b")
                || matches!(e["id"].as_str(), Some("stall" | "allyswitch" | "stockpile" | "metronome" | "dragoncheer"))
                || (key == "value" && e["id"] == "partiallytrapped"))
                && (!matches!(key, "source" | "source_slot") || SOURCE_READ.contains(&e["id"].as_str().unwrap_or("")))
        };
        let mut v = match field.split_once('.') {
            Some(("damaged_by", key)) => serde_json::Value::Array(
                p["damaged_by"].as_array().into_iter().flatten().map(|e| named(state, 1 - s, &e[key])).collect(),
            ),
            Some((list, key)) if p[list].is_array() => serde_json::Value::Array(
                p[list].as_array().into_iter().flatten().filter(|e| public(e, key)).map(|e| e[key].clone()).collect(),
            ),
            Some((object, key)) => p[object][key].clone(),
            None => p[field].clone(),
        };
        pointers(state, &mut v, 1 - s);
        v
    };
    let mut out = Vec::new();
    let outcome = walk_follow(case, &mut |game, followers, step| {
        let truth = value(game);
        let switching = truth["battle"]["request"] == "switch";
        for (viewer, follower) in followers.iter().enumerate() {
            if hidden_disable(game, viewer) || follower.phase().is_none() {
                continue;
            }
            let Ok(st) = follower.game_state() else { continue };
            let export = value(&Game::from_state(&st).map_err(|e| e.to_string())?);
            // (What its player cannot tell, by side and the species at each team index named.)
            let untold = serde_json::to_value(&st.untold).unwrap();
            let species_at = |side: u64, ti: &serde_json::Value| {
                export["battle"]["sides"][side as usize]["pokemon"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|q| q["team_index"] == *ti || (q.get("team_index").is_none() && *ti == 0))
                    .map(|q| q["original_species"].clone())
            };
            // (Side, species, the fields its entry is about, and the move for PP.)
            let mut doubted: Vec<(usize, serde_json::Value, &[&str], serde_json::Value)> = Vec::new();
            for u in untold.as_array().into_iter().flatten() {
                let about: &[&str] = match u["kind"].as_str().unwrap_or("") {
                    "rampage" => &["volatiles.id", "volatiles.move_id", "volatiles.a", "volatiles.value", "locked_move", "trapped"],
                    "pressure_pp" => &["moves.pp"],
                    "encore_target" | "charge_target" => &["last_move_target", "volatiles.a"],
                    "move_result" => &["move_this_turn", "move_last_turn"],
                    "item_taken" => &["last_item"],
                    _ => &["stats"],
                };
                for (s, ti) in [("side", "team_index"), ("other_side", "other_index")] {
                    if let (Some(side), Some(sp)) = (u[s].as_u64(), species_at(u[s].as_u64().unwrap_or(0), &u[ti])) {
                        doubted.push((side as usize, sp, about, u["move_id"].clone()));
                    }
                }
            }
            let mut wrong = |what: String, a: &serde_json::Value, b: &serde_json::Value| {
                out.push(format!("battle {} step {step}, seen by p{}: {what}: the game's {a}, the export's {b}", case.id, viewer + 1));
            };
            for s in 0..2 {
                // (Each side's conditions, where named.)
                if fields.iter().any(|f| f.ends_with("conditions")) {
                    // Its slot conditions, with their makers and numbers (the other side's Wish heals half its maker's
                    // max HP, which is its hidden spread's).
                    // (The effect order's numbering is a count of all effects, compared by rootcheck after renumbering.)
                    let slots = |state: &serde_json::Value| {
                        let mut v = state["battle"]["sides"][s]["slot_conditions"].clone();
                        pointers(state, &mut v, 1 - s);
                        for c in v.as_array_mut().into_iter().flatten().flat_map(|slot| slot.as_array_mut().into_iter().flatten()) {
                            if let Some(o) = c.as_object_mut() {
                                o.remove("order");
                                if s != viewer {
                                    o.remove("value");
                                }
                            }
                        }
                        v
                    };
                    let (a, b) = (slots(&truth), slots(&export));
                    if a != b {
                        wrong(format!("p{} slot_conditions", s + 1), &a, &b);
                    }
                    // Its side conditions and how long each has left (whoever put one up is read only as it goes up).
                    let conds = |state: &serde_json::Value| {
                        let mut v = state["battle"]["sides"][s]["conditions"].clone();
                        for c in v.as_array_mut().into_iter().flatten() {
                            if let Some(o) = c.as_object_mut() {
                                o.remove("source");
                                o.remove("source_slot");
                                o.remove("order");
                            }
                        }
                        v
                    };
                    let (a, b) = (conds(&truth), conds(&export));
                    if a != b {
                        wrong(format!("p{} conditions", s + 1), &a, &b);
                    }
                }
                let theirs = export["battle"]["sides"][s]["pokemon"].as_array().cloned().unwrap_or_default();
                for p in truth["battle"]["sides"][s]["pokemon"].as_array().into_iter().flatten() {
                    let lasting = |f: &str| matches!(f, "last_item" | "ate_berry" | "syrup_triggered" | "moves.pp");
                    let doubts: Vec<&(usize, serde_json::Value, &[&str], serde_json::Value)> =
                        doubted.iter().filter(|d| d.0 == s && d.1 == p["original_species"]).collect();
                    let Some(q) = theirs.iter().find(|q| q["original_species"] == p["original_species"]) else { continue };
                    let species = p["original_species"].as_str().unwrap_or("");
                    let choice = matches!(p["item"].as_str(), Some("choicescarf" | "choiceband" | "choicespecs"));
                    // (Where a move was aimed matters only for one whose target a player picks, used this stay:
                    // rootcheck's rule, as Encore's and Instruct's repeat read it.)
                    let chosen = p["last_move"].as_str().and_then(vgc_engine::data::move_id).is_some_and(|m| {
                        use vgc_engine::data::Target;
                        matches!(
                            vgc_engine::data::MOVES[m as usize].target,
                            Target::Normal | Target::Any | Target::AdjacentFoe | Target::AdjacentAlly | Target::AdjacentAllyOrSelf
                        )
                    });
                    for &field in fields.iter().filter(|f| !f.ends_with("conditions")) {
                        let theirs = s != viewer && matches!(field, "stats" | "last_move_target");
                        let benched = p["is_active"] != true && !lasting(field);
                        let unsaid = doubts.iter().any(|d| d.2.contains(&field) && (field != "moves.pp" || d.3.is_null()));
                        if (switching && field == "move_last_turn") || theirs || benched || unsaid || (field == "last_move_target" && !chosen) {
                            continue;
                        }
                        let (mut a, mut b) = (pick(&truth, p, field, s), pick(&export, q, field, s));
                        // (Who has attacked it is a set: the two sides number the other's Pokémon each their own way.)
                        if field == "types_said.attacked_by" {
                            for v in [&mut a, &mut b] {
                                if let serde_json::Value::Array(l) = v {
                                    l.sort_by_key(|x| x.to_string());
                                }
                            }
                        }
                        // (PP paid to Pressure behind a blanked target: that move's PP is not told.)
                        if field == "moves.pp" {
                            for d in doubts.iter().filter(|d| d.2.contains(&"moves.pp")) {
                                for (state, list) in [(p, &mut a), (q, &mut b)] {
                                    let ids: Vec<serde_json::Value> =
                                        state["moves"].as_array().into_iter().flatten().map(|m| m["id"].clone()).collect();
                                    if let serde_json::Value::Array(l) = list {
                                        for (k, v) in l.iter_mut().enumerate() {
                                            if ids.get(k) == Some(&d.3) {
                                                *v = serde_json::Value::Null;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if switching && !choice && field.starts_with("volatiles") {
                            let kept: Vec<bool> =
                                p["volatiles"].as_array().into_iter().flatten().map(|v| v["id"] != "choicelock").collect();
                            if let serde_json::Value::Array(list) = &mut a {
                                let mut k = kept.iter();
                                list.retain(|_| *k.next().unwrap_or(&true));
                            }
                        }
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

/// What the belief roots found the follower and its export getting wrong on random and themed play (S3b.6c), each
/// in the battle that showed it (Dex sets, random or `gen_cases`; cut from gate 1's recordings at `fefdfcf` to just
/// past it, with Showdown's state at every point; ids from 1, the source in the battle's name): at every decision
/// the follower observes as the game does (the disguise sleep counts, D96, by the name each Pokémon goes by), its
/// export loads, observes as it does, counts as the game does, lists every case of what its player is not told and
/// nothing else, and has the game's value of every field of its family a search reads (`root_state_differs`). At
/// `fefdfcf` each battle failed one of these (or did not list its D89 case, 39-41), but six that guard S3b.6c's own
/// changes, found by followcheck or rootcheck on its way: 48, the told sleep count of two on the field that look the
/// same (D96); 58, 80 and 90, a Pokémon's own `cant` after a move Magic Bounce sent back or an Instruct repeated, which
/// is an action of its own; 76, Trace judged on who stood beside it as it came in; and 94, Flower Veil keeping Parting
/// Shot's stages off at a target it still reached. 49-73 are the
/// families the second full measurement found (Dragon Darts that strike nothing, a move Magic Bounce sends back,
/// Synchronize's and Heal Block's lines, a Substitute's share of a status move, Curse's aim, and more); 74-89
/// the third's (a revived Pokémon passed in, Ripen, Trace at entry, Counter's and Metal Burst's PP, a disguise's item);
/// 90-97 the fourth's (an Instruct while asleep, a Substitute's share, Trace at a Mega Evolution, Flower Veil, a redirect);
/// 98 the real-team re-measure's (S3b.8: a Pokémon revived where it fainted, before anyone replaced it, scripted);
/// 99 its gate 1's rotating seed (a doubt over Pressure's PP behind a charge made in disguise, the Zoroark's once the
/// disguise drops).
#[test]
fn what_the_belief_roots_found_the_export_getting_wrong_is_right() {
    use vgc_engine::replay::{ExportTally, check_export};
    // The fields each battle shows (by its id in the fixture; the disguise battles, 42-48, are the observation's).
    const VOLS: &[&str] = &["volatiles.id", "volatiles.move_id", "volatiles.a", "volatiles.value"];
    const RESULT: &[&str] = &["move_last_turn", "move_this_turn", "types_said.attacked_by", "times_attacked"];
    let fields = |id: u32| -> &'static [&'static str] {
        match id {
            1 | 8 | 9 | 10 => &["volatiles.id", "volatiles.source", "volatiles.source_slot"],
            2..=4 | 11..=14 | 37 | 52 | 53 | 55 | 57 | 84 => VOLS,
            5 => &["syrup_triggered"],
            6 | 7 | 38 => &["conditions", "slot_conditions"],
            15..=19 | 40 | 49..=51 | 59 | 61 | 64..=69 | 73 | 77..=79 | 83 | 85 | 86 | 89 | 91 | 93 | 94 | 96 | 97 => RESULT,
            20 | 21 | 56 | 62 | 74 | 98 => &["damaged_by.pokemon", "times_attacked", "types_said.attacked_by"],
            22 => &["ate_berry"],
            23 | 41 => &["last_item"],
            24..=28 | 60 | 75 | 76 | 92 => &["ability_extra.a", "ability_extra.b", "ability_extra.item"],
            29 => &["shown_now.copied_gastro"],
            30 => &["skip_before_switch_out", "switch_flag"],
            31 | 32 | 58 | 80 | 87 | 90 | 95 => &["move_actions"],
            33 | 54 | 81 | 82 | 88 => &["moves.pp"],
            34 | 39 | 70..=72 => &["last_move_target", "volatiles.a"],
            63 => &["types_said.apparent", "types_said.copied", "types_said.copied_foe", "types_said.told"],
            35 | 36 => &["stats"],
            _ => &[],
        }
    };
    let (mut tally, mut exports) = (FollowTally::default(), ExportTally::default());
    let text = include_str!("fixtures/root_regressions.jsonl");
    let mut wrong = Vec::new();
    for line in text.lines() {
        let case: FollowCase = serde_json::from_value(with_state(line)).unwrap();
        assert!(case.has_every_state(), "battle {}", case.id);
        for outcome in [check_follow(&case, &mut tally), check_export(&case, &mut exports)] {
            match outcome {
                Outcome::Pass(_) => {}
                Outcome::Unsupported(why) => panic!("battle {} cannot be checked: {why}", case.id),
                Outcome::Fail(why) => panic!("battle {} was not followed: {}", case.id, why.join("; ")),
            }
        }
        wrong.extend(root_state_differs(&case, fields(case.id)));
    }
    assert!(text.lines().count() >= 99, "{} battles", text.lines().count());
    assert_eq!(tally.wrong, 0, "observations differ: {:#?}", tally.by_feature);
    let bad = (exports.broken, exports.observed_differently, exports.counts_differ, exports.hp_bar_differ);
    assert_eq!(bad, (0, 0, 0, 0), "{:#?}", exports.first);
    assert_eq!((exports.untold_missing, exports.untold_unrecognised), (0, 0), "{:#?}", exports.first);
    assert!(wrong.is_empty(), "{wrong:#?}");
    // (What its player cannot tell, D89's D1-D3 among it: listed where the battles reach it.)
    for kind in ["charge_target", "move_result", "item_taken", "stats_shared"] {
        assert!(exports.untold.get(kind).is_some_and(|&n| n > 0), "{kind}: {:?}", exports.untold);
    }
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
    assert!(battles >= 49 && tally.asked > 300, "{battles} battles, {} exports", tally.asked);
    let bad = (tally.broken, tally.observed_differently, tally.counts_differ, tally.hp_bar_differ);
    assert_eq!(bad, (0, 0, 0, 0), "{:#?}", tally.first);
    assert!(wrong.is_empty(), "{wrong:#?}");
    // The silent rampage ends in the fixture are recognised as such (D89), not missed.
    assert!(tally.unsaid.get("rampage").is_some_and(|&n| n >= 3), "{:?}", tally.unsaid);
    // And each export lists what its player cannot tell, every case of it (D94), and nothing else.
    assert_eq!(tally.untold_missing, 0, "{:#?}", tally.first);
    assert_eq!(tally.untold_unrecognised, 0, "{:#?}", tally.first);
    for kind in ["rampage", "pressure_pp", "encore_target"] {
        assert!(tally.untold.get(kind).is_some_and(|&n| n > 0), "{kind}: {:?}", tally.untold);
    }
}

/// Champions' Encore puts a move in again with no target, and Showdown draws one (D89, S3b.6b2): the
/// Encored Pokémon's own side is told where, unless both of the other side stood at the draw and
/// the move's line names nobody (`[still]`) with no later line of it naming the place. Battles
/// 43-48 of the D89 fixture: a Trick (after one of the other side fainted), a Dig and a Fake Out whose
/// lines are blanked (not told: recognised as `encore_blanked` and listed, the Dig's charge aimed at
/// the same hidden place); a Disable with
/// one of the other side standing (the draw had one place); a Solar Beam let off in the sun, whose
/// `-anim` names the place; and the same Solar Beam after the Pokémon drawn has fainted (battle 48),
/// whose `-anim` names Showdown's second draw, not the place the game keeps: listed as the fainted
/// case is. Wherever the export does not list it, it has the game's place.
#[test]
fn where_an_encored_move_went_is_listed_only_where_it_is_not_told() {
    use vgc_engine::env::{Game, Untold};
    use vgc_engine::replay::{Unsaid, UnsaidTracker, hidden_disable};
    let value = |game: &Game| -> serde_json::Value { serde_json::from_str(&game.to_state().to_json()).unwrap() };
    let mut seen = std::collections::BTreeMap::<u32, (usize, usize, usize, usize)>::new();
    for line in include_str!("fixtures/export_regressions.jsonl").lines() {
        let case: FollowCase = serde_json::from_str(line).unwrap();
        if !(43..=48).contains(&case.id) {
            continue;
        }
        let (mut recognised, mut listed, mut compared, mut charged) = (0, 0, 0, 0);
        let mut unsaid = UnsaidTracker::default();
        let outcome = walk_follow(&case, &mut |game, followers, step| {
            match step {
                0 => unsaid.start(game),
                k => unsaid.step(game, &case.steps[k - 1].after.log, &case.steps[k - 1].choices),
            }
            let truth = value(game);
            for (side, follower) in followers.iter().enumerate() {
                if hidden_disable(game, side) || follower.phase().is_none() {
                    continue;
                }
                // (Either case: the line blanked, or the one drawn fallen first.)
                let blanked: Vec<String> = unsaid
                    .for_viewer(side)
                    .into_iter()
                    .filter_map(|u| match u {
                        Unsaid::EncoreTarget { species, .. } => Some(species),
                        _ => None,
                    })
                    .collect();
                recognised += blanked.len();
                // (A wait, or a position the export refuses: nothing to compare.)
                let Ok(st) = follower.game_state() else { continue };
                let export = value(&Game::from_state(&st).map_err(|e| e.to_string())?);
                let mine = |state: &serde_json::Value| state["battle"]["sides"][side]["pokemon"].as_array().cloned().unwrap();
                // (The Pokémon under an Encore: what any other's last aim is, is another fixture's.)
                let encored = |p: &&serde_json::Value| {
                    p["volatiles"].as_array().into_iter().flatten().any(|v| v["id"] == "encore")
                };
                for p in mine(&truth).iter().filter(|p| p["is_active"] == true).filter(encored) {
                    let q = mine(&export).into_iter().find(|q| q["original_species"] == p["original_species"]).unwrap();
                    let entry = st.untold.iter().find_map(|u| match u {
                        Untold::EncoreTarget { side: s, team_index, locs }
                            if *s as usize == side && q["team_index"] == *team_index => Some(locs.clone()),
                        _ => None,
                    });
                    let species = p["original_species"].as_str().unwrap();
                    let game_loc = p["last_move_target"].as_i64().unwrap_or(0);
                    match entry {
                        Some(locs) => {
                            listed += 1;
                            assert!(locs.contains(&(game_loc as i8)), "battle {} step {step}: {game_loc} not in {locs:?}", case.id);
                            // (A charge it is making is aimed where the draw went: the same hidden place.)
                            let charging = ["dig", "fly", "dive", "bounce", "phantomforce", "solarbeam", "solarblade", "skyattack", "meteorbeam", "electroshot"];
                            for v in p["volatiles"].as_array().into_iter().flatten() {
                                if charging.contains(&v["id"].as_str().unwrap_or("")) {
                                    assert_eq!(v["a"].as_i64().unwrap_or(0), game_loc, "battle {} step {step}: {v}", case.id);
                                    charged += 1;
                                }
                            }
                        }
                        None => {
                            assert!(!blanked.iter().any(|b| b == species), "battle {} step {step}: {species} not listed", case.id);
                            compared += 1;
                            assert_eq!(q["last_move_target"].as_i64().unwrap_or(0), game_loc, "battle {} step {step}: {species}", case.id);
                        }
                    }
                }
            }
            Ok(())
        });
        match outcome {
            Outcome::Pass(_) => {}
            Outcome::Fail(why) => panic!("battle {}: {}", case.id, why.join("; ")),
            Outcome::Unsupported(why) => panic!("battle {}: {why}", case.id),
        }
        seen.insert(case.id, (recognised, listed, compared, charged));
    }
    assert_eq!(seen.len(), 6, "{seen:?}");
    for id in [43, 44, 47] {
        assert!(seen[&id].0 > 0 && seen[&id].1 > 0, "battle {id}: {seen:?}");
    }
    assert!(seen[&44].3 > 0, "the Dig's charge: {seen:?}");
    for id in [45, 46] {
        assert!(seen[&id].0 == 0 && seen[&id].1 == 0 && seen[&id].2 > 0, "battle {id}: {seen:?}");
    }
    // (After the one drawn fell, a later line names the second draw: the fainted case, recognised and listed.)
    assert!(seen[&48].0 > 0 && seen[&48].1 > 0, "battle 48: {seen:?}");
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
                k => unsaid.step(game, &case.steps[k - 1].after.log, &case.steps[k - 1].choices),
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
                // The other side is the one not told of PP paid to Pressure or of a blanked move's result; a player
                // is not told where its own Encored or called charging move went. Neither is told of a rampage
                // that may be over (its own side until a move request), nor of Pickup's source, nor of a split
                // between one of each side; of a split between two of one side, the other side.
                Unsaid::PressurePp { side, .. } | Unsaid::MoveResult { side, .. } => assert_eq!(viewer, 1 - side),
                Unsaid::EncoreTarget { side, .. } | Unsaid::ChargeTarget { side, .. } => assert_eq!(viewer, *side),
                Unsaid::Rampage { .. } | Unsaid::ItemTaken { .. } | Unsaid::StatsShared { .. } => {}
            }
        }
    }
    // Four rampages whose second turn could have been their last without a word (battles 1 and 4, which each
    // end there, and the two V5 ones, 26 and 27; the walk now recognises the case whichever reading the game
    // took, S3b.6c), the Pressure case and the Encore case; the renewed rampages (a self-hit, which the log
    // shows) are not among them.
    assert_eq!(kinds.get("rampage"), Some(&4), "{kinds:?}");
    assert!(kinds.get("pressure_pp").is_some_and(|&n| n >= 1), "{kinds:?}");
    assert!(kinds.get("encore_target").is_some_and(|&n| n >= 1), "{kinds:?}");
    // Two blanked lines after an Encore (S3b.6b2): a Fake Out that fails, and a Dig's charge. (The Trick that
    // fails after one of the other side fainted is the fainted case, S3b.6c.)
    assert_eq!(kinds.get("encore_blanked"), Some(&2), "{kinds:?}");
}

/// No over-listing (S3b.6c, Kevin 2026-10-20): every entry of an export's list of what its player cannot tell is
/// a case the game's history knows of, at every decision of the D89 fixture; an entry added by hand, or one that
/// names another move than the one that paid, names no case and is caught.
#[test]
fn an_entry_naming_no_case_its_player_cannot_tell_is_caught() {
    use vgc_engine::env::{Game, Untold};
    use vgc_engine::replay::{UnsaidTracker, hidden_disable, unrecognised};
    let (mut entries, mut added, mut renamed) = (0, 0, 0);
    for line in include_str!("fixtures/export_regressions.jsonl").lines() {
        let case: FollowCase = serde_json::from_str(line).unwrap();
        let mut unsaid = UnsaidTracker::default();
        walk_follow(&case, &mut |game, followers, step| {
            match step {
                0 => unsaid.start(game),
                k => unsaid.step(game, &case.steps[k - 1].after.log, &case.steps[k - 1].choices),
            }
            for (side, follower) in followers.iter().enumerate() {
                if hidden_disable(game, side) || follower.phase().is_none() {
                    continue;
                }
                let Ok(mut st) = follower.game_state() else { continue };
                let export = Game::from_state(&st).map_err(|e| e.to_string())?;
                let b = export.battle().unwrap();
                let cases = unsaid.for_viewer(side);
                assert!(unrecognised(&st.untold, &cases, b).is_empty(), "battle {} step {step}: {:?}", case.id, st.untold);
                entries += st.untold.len();
                // A Pokémon of the other side listed as having split stats, which none in the fixture did.
                let extra = Untold::StatsShared { side: (1 - side) as u8, team_index: 0 };
                st.untold.push(extra.clone());
                assert_eq!(unrecognised(&st.untold, &cases, b), [&extra], "battle {} step {step}", case.id);
                st.untold.pop();
                added += 1;
                // PP paid to Pressure by another move than the one that paid.
                if let Some(Untold::PressurePp { move_id, .. }) = st.untold.iter_mut().find(|e| matches!(e, Untold::PressurePp { .. })) {
                    *move_id = "struggle".into();
                    assert_eq!(unrecognised(&st.untold, &cases, b).len(), 1, "battle {} step {step}", case.id);
                    renamed += 1;
                }
            }
            Ok(())
        });
    }
    assert!(entries > 0 && added > 300 && renamed > 0, "{entries} entries, {added} added, {renamed} renamed");
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
                k => unsaid.step(game, &case.steps[k - 1].after.log, &case.steps[k - 1].choices),
            }
            for viewer in 0..2 {
                for u in unsaid.for_viewer(viewer) {
                    if let Unsaid::Rampage { side, species } = u
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
    // Each silent end is in the export's own list of what its player cannot tell (D94).
    assert_eq!(exports.untold_missing, 0, "{:#?}", exports.first);
    assert!(exports.untold.get("rampage").is_some_and(|&n| n >= 5), "{:?}", exports.untold);
    // The Own Tempo Emboar's and the confused Annihilape's: each is followed by a replacement request
    // (a wait for the side that does not replace), so neither side is told, its own included.
    for (id, species) in [(801, "emboar"), (1970, "annihilape")] {
        let mut own: Vec<bool> = silent.iter().filter(|s| s.0 == id && s.3 == species).map(|s| s.1 == s.2).collect();
        own.sort();
        assert_eq!(own, [false, true], "battle {id}: {silent:?}");
    }
}

/// What the themed batches and random play found the follower getting wrong (S3b.6b), each in the
/// battle that showed it (Dex sets from `gen_cases`; the battle's name says which), recorded again to
/// just past it with Showdown's state at every point: a binding Baton Pass hands on, the other side's
/// Skill Swap between partners (which names no ability), a Quick Claw rolled again for the move
/// Champions' Encore put in, Gastro Acid on an Early Bird, a rampage Copycat began; and what the export
/// adds: an exact HP a Pain Split tells (with what it says of the max HP), stats two of the other side's
/// split, which no spread this side can draw gives, and an Endeavor's exact HP, which goes with a
/// disguise when it drops and is told by the Endeavor's own hit only.
#[test]
fn what_the_themed_batches_and_random_play_found_the_follower_getting_wrong_is_right() {
    use vgc_engine::replay::{ExportTally, check_export};
    let (mut tally, mut exports) = (FollowTally::default(), ExportTally::default());
    let text = include_str!("fixtures/themed_export_regressions.jsonl");
    for line in text.lines() {
        let case: FollowCase = serde_json::from_str(line).unwrap();
        assert!(case.has_every_state(), "battle {}", case.id);
        for outcome in [check_follow(&case, &mut tally), check_export(&case, &mut exports)] {
            match outcome {
                Outcome::Pass(_) => {}
                Outcome::Unsupported(why) => panic!("battle {} cannot be checked: {why}", case.id),
                Outcome::Fail(why) => panic!("battle {} was not followed: {}", case.id, why.join("; ")),
            }
        }
    }
    assert_eq!(text.lines().count(), 9);
    assert_eq!(tally.wrong, 0, "observations differ: {:#?}", tally.by_feature);
    let bad = (exports.broken, exports.observed_differently, exports.counts_differ, exports.hp_bar_differ);
    assert_eq!(bad, (0, 0, 0, 0), "{:#?}", exports.first);
    assert_eq!((exports.untold_missing, exports.hp_seen_wrong), (0, 0), "{:#?}", exports.first);
    assert!(exports.hp_seen > 0, "the Pain Split's HP is worked out");
    assert!(exports.untold.get("stats_shared").is_some_and(|&n| n > 0), "{:?}", exports.untold);
}

/// What an export adds beside the battle (D94, D77) is kept by a game taken up from it, written back
/// out, and gone after a step, when what was drawn from it is the game's own.
#[test]
fn what_an_export_adds_is_kept_until_a_step() {
    use vgc_engine::env::{Game, GameState, HpSeen, Untold};
    let case: FollowCase =
        serde_json::from_str(include_str!("fixtures/themed_export_regressions.jsonl").lines().nth(5).unwrap()).unwrap();
    let mut exported = None;
    walk_follow(&case, &mut |game, followers, _| {
        if exported.is_none() && game.phase() == vgc_engine::obs::Phase::Move {
            exported = followers[0].game_state().ok();
        }
        Ok(())
    });
    let mut st: GameState = exported.expect("an export at a move request");
    st.untold.push(Untold::Rampage { side: 1, team_index: 0 });
    st.hp_seen.push(HpSeen { side: 1, team_index: 0, hp: 50, pct: 40, bar: "yellow".into(), now: false });
    let mut g = Game::from_state(&st).unwrap();
    let back = g.to_state();
    assert_eq!((back.untold.clone(), back.hp_seen.clone()), (st.untold.clone(), st.hp_seen.clone()));
    let (a, b) = (g.legal(0)[0], g.legal(1)[0]);
    g.act([a, b], [0; 4]).unwrap();
    let after = g.to_state();
    assert!(after.untold.is_empty() && after.hp_seen.is_empty());
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

/// What gate 1's rotating seed of 2026-10-22 found on the DGX Spark (vgc-bot S4.5a), both on the
/// checking side, the follower right in each: a Thrash's second turn under Gravity in Misty Terrain,
/// where the end's confusion would have been taken without a word at the move (Misty Terrain asks
/// `isGrounded` then) though Gravity ended with the turn, so the walk, judging grounding at the step's
/// end, saw no case where the follower listed one (battle 1451); and a Pokémon in disguise, asleep,
/// knocked out as its disguise dropped, where the game's count for the name it passed for went to 0,
/// as if the name had woken, while the follower kept the name's own turn asleep (battle 492).
/// And what gate 3 found there on the first run's gen1.pt (2 of 10,000 skilled games), on the follower's
/// side: two of the other side's on the field that look the same (an Illusion beside the Pokémon it
/// copies), the one asleep under the name the Pokémon itself (a Rested Malamar beside a Zoroark-Hisui
/// passing for it, battle 2794) or the copy carrying a count told under the name on an earlier stay
/// (a Zoroark-Hisui as Dragapult beside the Dragapult, battle 6352), where the follower put the awake
/// look-alike's 0 on the name's count and the game kept the runs the log had shown under the name (D96).
#[test]
fn what_the_spark_rotating_seed_found_is_right() {
    use vgc_engine::replay::{ExportTally, check_export};
    let (mut tally, mut exports) = (FollowTally::default(), ExportTally::default());
    let text = include_str!("fixtures/s45a_spark_regressions.jsonl");
    for line in text.lines() {
        // (The oracle's fuzz recordings carry the state flat beside the log; the harness's, gate 3's, nested.)
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let nested = value["initial"].get("state").is_some();
        let case: FollowCase = serde_json::from_value(if nested { value } else { with_state(line) }).unwrap();
        assert!(case.has_every_state());
        for outcome in [check_follow(&case, &mut tally), check_export(&case, &mut exports)] {
            match outcome {
                Outcome::Pass(_) => {}
                Outcome::Unsupported(why) => panic!("battle {} cannot be checked: {why}", case.id),
                Outcome::Fail(why) => panic!("battle {} was not followed: {}", case.id, why.join("; ")),
            }
        }
    }
    assert_eq!(text.lines().count(), 12);
    assert_eq!(tally.wrong, 0, "observations differ: {:#?}", tally.by_feature);
    // R2's gate 3 (game 4268): a Ceruledge Encored into a Protect the foe's Imprison then sealed, its side's last
    // active, is shown its real moves and says `move 4`; Showdown takes it and plays Struggle, and so must the
    // engine's `choose` (its own listing names `move 1 2` for the same Struggle). The decision is one the player
    // is not shown the whole of, so its observation is passed over, as the tally counts.
    assert!(tally.hidden_disables >= 1, "{:?}", tally.hidden_disables);
    // The rotating seed 20261026 (battle 1939): a Speed Swap on a Sneasler in Avalugg's disguise, the disguise then
    // dropping (`|replace|`) while it stands. Its Speed is the Raichu's, which its own player cannot know, and the
    // export goes on listing it (D77); the walk used to drop the case at the `replace` as if a forme had changed.
    assert!(exports.untold.get("stats_shared").is_some_and(|&n| n >= 2), "{:?}", exports.untold);
    // The Leafeon's sleep count is compared at every decision it stands asleep (the one its disguise fell under),
    // and the Malamar's and the Dragapult's beside their look-alikes (26 decisions over the two gate 3 battles).
    assert!(tally.counted.get("mon: sleep count").is_some_and(|&n| n > 26), "{:?}", tally.counted);
    // R2's gate 3 at 63752e0 (game 3974): Perish Song felled both of p2's Pokémon after `upkeep`, one of them asleep,
    // with one left to send in; `checkFainted` cleared both statuses before the replacement came in, and the follower,
    // judging "someone to send in" once it stood, kept the sleeper's sleep and its count (`counts_differ`).
    // R2's gate 3 at 8828f9b (game 2721): a Golisopod's Emergency Exit at the turn's end beside a fallen Sneasler,
    // the side's one reserve going to the Sneasler's slot and the Golisopod passed over; the follower kept it as
    // switching, and its export at the next move request did not load (`broken`: a switch flag at a move request).
    // R2's gate 3 at 972e82f (game 7012): a Pawmot's Revival Blessing passed over by its player; Showdown's slot
    // condition lasts one turn and ended at the residual, the follower's stood to the next `turn`, and the other
    // side's replacement request between them saw it (2 observations, `slot revivalblessing`).
    // The rotating seed 20261026 (battle 1129, taken by name in gate 1 until this fix): a Rested Orthworm roared
    // in beside its Zoroark look-alike, asleep for a run there, roared out and back in where the look-alike had
    // stood; the look-alike, left alone with the name, took the name's number with its own count of 0 over the
    // run the log had shown under the name, so the Orthworm came back to a count of 0 (2 observations, side 0).
    // The same seed (battle 1873): a Raging Fury begun by an Infernape in Goodra's disguise, the disguise dropping
    // on its second turn. The lock and its count went by the number the Infernape passed for, and the export had
    // it free (`counts_differ`); they go with it to its own number now, as its sleep and freeze do.
    let bad = (exports.broken, exports.observed_differently, exports.counts_differ, exports.hp_bar_differ);
    assert_eq!(bad, (0, 0, 0, 0), "{:#?}", exports.first);
    // The Thrash's silent end is a case the walk knows, and the export lists it, and nothing more (D94).
    // And the same seed (battle 1337): a Mabosstiff's Pickup from one of two that ate the same berry, then a
    // Corviknight's, the one of the two that ate, picking up from the one left: the first took the Corviknight's.
    // The export listed no doubt, rightly; the walk kept one (`untold_missing`) until a Trick ended it.
    assert_eq!((exports.untold_missing, exports.untold_unrecognised), (0, 0), "{:#?}", exports.first);
    assert!(exports.unsaid.get("rampage").is_some_and(|&n| n >= 1), "{:?}", exports.unsaid);
    assert!(exports.untold.get("rampage").is_some_and(|&n| n >= 1), "{:?}", exports.untold);
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
