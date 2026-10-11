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
