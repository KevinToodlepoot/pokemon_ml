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
use vgc_engine::replay::{FollowCase, FollowTally, Outcome, check_follow};

fn cases() -> Vec<FollowCase> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/followed_battles.jsonl");
    let file = std::fs::File::open(path).expect("the fixture is there");
    BufReader::new(file).lines().map(|line| serde_json::from_str(&line.unwrap()).expect("a readable battle")).collect()
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
                    assert_eq!(f[1144], 1.0, "the opponent cannot know suppression was silently removed");
                    me.line("|-activate|p2b: Alcremie0|ability: Battle Bond").unwrap();
                    me.request(&case.steps[7].after.requests[side].to_string()).unwrap();
                    me.observe(&mut f, &mut i, &mut mask).unwrap();
                    assert_eq!(f[1144], 0.0, "a public Battle Bond activation resolves the suppression uncertainty");
                } else {
                    assert_eq!(f[894], 0.0, "the holder knows its own unsuppressible ability");
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
            992 => assert_eq!(f[495], 0.0, "a pending SwitchIn cannot expose the previous Toxic counter"),
            733 => {
                assert_eq!(
                    f[526],
                    47.0 / 200.0,
                    "an instructed repeat does not reveal the unknown original queued move"
                )
            }
            834 => assert_eq!(f[262], 11.0 / 12.0, "Mega Levitate prevents spread Expanding Force Pressure costs"),
            970 => assert_eq!(f[265], 7.0 / 12.0, "the instant solar animation resolves the Pressure target"),
            582 => assert_eq!(
                i[72],
                vgc_engine::data::species_id("morpekohangry").unwrap() as i16 + vgc_engine::obs::ID_BASE
            ),
            236 => assert_eq!(
                f[423],
                101.0 / 200.0,
                "an instructed repeat cannot reveal the failed queued move's priority"
            ),
            574 => assert_eq!(f[453], 0.75, "a failed reflection does not establish ownership of Heal Pulse"),
            328 => assert_eq!(f[810], 0.0, "called Speed Swap makes the old first-mover inference unavailable"),
            892 => assert_eq!(f[422], 111.0 / 200.0, "Encore invalidates the initial Quick Claw roll"),
            111 => assert_eq!(f[1144], 0.0, "Battle Bond discards Baton Passed Gastro Acid"),
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
            699 => assert_eq!(f[227], 1.0, "Future Sight's caster retains full HP when Red Card cancels recoil"),
            591 => assert_eq!(f[366], 1.0, "Ripen restores twenty PP, capped at Guard Swap's maximum"),
            746 => assert_eq!(f[475], 107.0 / 200.0, "instructed Snore does not consume the original queued action"),
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
            830 => assert_eq!(f[631], 145.0 / 200.0, "Attract after waking preserves Quick Feet queue evidence"),
            843 => assert_eq!(f[40], 4.0 / 8.0, "Court Change preserves Reflect's elapsed age"),
            392 => {
                assert_eq!(f[904], 1.0, "a refused reflected heal does not cancel charging");
                assert_eq!(f[912], 1.0, "Solar Blade remains prepared");
            }
            605 => assert_eq!(f[527], 97.0 / 200.0, "an instructed Quick Claw roll cannot promote the queued move"),
            539 => {
                assert_eq!(f[661], 0.75, "lost-focus cant reveals the unmasked Pokémon's move");
                assert_eq!(i[94], vgc_engine::data::move_id("focuspunch").unwrap() as i16 + vgc_engine::obs::ID_BASE);
            }
            25 => assert_eq!(f[423], 118.0 / 200.0, "a Cheri Berry cure is not waking from sleep"),
            9 => assert_eq!(f[578], 141.0 / 200.0, "lost-focus order supplies Speed evidence"),
            11 => assert_eq!(f[630], 145.0 / 200.0, "lost focus after a pivot preserves queued priority"),
            _ => unreachable!(),
        }
    }
    assert_eq!(tally.observations, 482);
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
                assert_eq!(f[938], 0.0, "the disguised ally's faint leaves this position empty");
            }
            735 => {
                assert_eq!(f[630], 36.0 / 200.0, "ambiguous item loss cannot exclude the real Aegislash's speed items");
                assert_eq!(f[631], 184.0 / 200.0);
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
            232 => assert_eq!(f[368], -1.0, "Armor Tail hides the rerolled target's Pressure cost"),
            857 => {
                assert_eq!(f[527], 141.0 / 200.0, "waking through confusion preserves the Speed bound");
                assert_eq!(f[528], 0.0, "the observed ordering excludes speed-modifying items");
            }
            266 => assert_eq!(f[683], 111.0 / 200.0, "a cured Encore permits Speed evidence"),
            472 => assert_eq!(f[213], 1.0, "Choice refusal does not spend copied Sleep Talk PP"),
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
            871 => (11, 474, 114.0 / 200.0),
            115 => (14, 423, 89.0 / 200.0),
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
    assert_eq!(f[366], -1.0, "do not expose an inferred PP count as exact");
    let step = &case.steps[3];
    me.lines(&Follower::own_lines(1, &step.after.log)).unwrap();
    me.request(&step.after.requests[1].to_string()).unwrap();
    let b = me.battle().unwrap();
    let mon = b.mon(b.active(1, 0));
    assert_eq!((mon.unknown_pp, mon.moves[0].pp, mon.moves[0].maxpp), (0, 8, 12));
    me.observe(&mut f, &mut i, &mut mask).unwrap();
    assert_eq!(f[366], 8.0 / 12.0);
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
    assert_eq!(tally.observations, 951);
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
    assert_eq!(tally.observations, 1103);
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
    // These fixtures are random play kept for the follower's hardest cases; what a search exports
    // there may still miss a rare lock or count (none do on skilled-play recordings, `followcheck
    // --export`). Bounded here, and printed.
    let exported = tally.asked - tally.refused.values().sum::<usize>();
    let (seen, counted) = (tally.observed_differently, tally.counts_differ);
    assert!(seen * 100 <= exported, "{seen} of {exported}: {:#?}", tally.first);
    assert!(counted * 100 <= exported, "{counted} of {exported}: {:#?}", tally.first);
    println!("observed differently {seen} and counts differ {counted} of {exported} exports");
    let known = ["Illusion", "Transform", "a switch in the middle of a turn", "closed team sheets"];
    assert!(tally.refused.keys().all(|why| known.contains(&why.as_str())), "{:?}", tally.refused);
    println!("{battles} battles, {} exports, refused {:?}", tally.asked, tally.refused);
}
