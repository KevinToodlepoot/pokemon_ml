//! Checks the engine against battles recorded from Pokémon Showdown. The
//! fixture is a small sample; `scripts/fuzz.sh` runs the same check on
//! thousands of freshly generated battles.

use vgc_engine::replay::{Case, Outcome, Rebuild, check_case, check_case_with};
use vgc_engine::{ACTIVE, Battle, Choice, PokemonSet};

fn fixture() -> Vec<Case> {
    let text = include_str!("fixtures/showdown_cases.jsonl");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("malformed fixture line"))
        .collect()
}

#[test]
fn endure_preserves_one_hp_against_instructed_confusion() {
    let case: Case = serde_json::from_str(include_str!("fixtures/endure_confusion_regression.jsonl")).unwrap();
    for mode in [Rebuild::No, Rebuild::ByHand] {
        match check_case_with(&case, mode) {
            Outcome::Pass(n) => assert_eq!(n, 21),
            Outcome::Fail(why) => panic!("{}", why.join("; ")),
            Outcome::Unsupported(why) => panic!("unsupported: {why}"),
        }
    }
}

#[test]
fn future_sight_red_card_and_life_orb_match_showdown() {
    let cases: Vec<Case> = include_str!("fixtures/future_sight_ripen_instruct_regressions.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cases.len(), 3);
    let mut decisions = 0;
    for case in &cases {
        match check_case_with(case, Rebuild::ByHand) {
            Outcome::Pass(n) => decisions += n,
            Outcome::Fail(why) => panic!("case {}: {}", case.id, why.join("; ")),
            Outcome::Unsupported(why) => panic!("case {} unsupported: {}", case.id, why),
        }
    }
    assert_eq!(decisions, 157);
}

#[test]
fn recorded_battles_match_showdown() {
    let cases = fixture();
    assert!(cases.len() >= 10);
    let mut decisions = 0;
    for c in &cases {
        match check_case(c) {
            Outcome::Pass(n) => decisions += n,
            Outcome::Unsupported(what) => panic!("case {} uses unsupported {what}", c.id),
            Outcome::Fail(report) => panic!("case {} diverged from Showdown:\n{}", c.id, report.join("\n")),
        }
    }
    assert!(decisions > 100);
}

#[test]
fn expiring_terrain_clears_a_replacement_created_during_residuals() {
    let case: Case = serde_json::from_str(include_str!("fixtures/terrain_replacement.jsonl")).unwrap();
    for mode in [Rebuild::No, Rebuild::ByHand] {
        match check_case_with(&case, mode) {
            Outcome::Pass(n) => assert_eq!(n, 8),
            Outcome::Unsupported(why) => panic!("unsupported terrain regression: {why}"),
            Outcome::Fail(report) => panic!("terrain replacement differs from Showdown: {}", report.join("\n")),
        }
    }
}

/// The same battles with the position written down and the battle built back
/// from it at every decision (`Battle::to_state`, `Battle::from_state`): the
/// rebuilt battle must be in the same position and go on matching Showdown,
/// and a battle built from the position without the simulator's bookkeeping
/// must come to the same.
#[test]
fn recorded_battles_survive_being_rebuilt_at_every_decision() {
    for c in &fixture() {
        match check_case_with(c, Rebuild::ByHand) {
            Outcome::Pass(_) => {}
            Outcome::Unsupported(what) => panic!("case {} uses unsupported {what}", c.id),
            Outcome::Fail(report) => panic!(
                "case {}:
{}",
                c.id,
                report.join(
                    "
"
                )
            ),
        }
    }
}

/// `joint_ok` (used to validate submitted choices) and `joint_choices` (used
/// to enumerate them) are written separately; they must describe the same set.
#[test]
fn choice_validation_agrees_with_enumeration() {
    let mut universe = vec![Choice::Pass];
    for slot in 0..5 {
        for target in -3..=3 {
            for mega in [false, true] {
                universe.push(Choice::Move { slot, target, mega });
            }
        }
    }
    for to in 0..7 {
        universe.push(Choice::Switch { to });
    }
    for c in fixture().iter().take(6) {
        let sets: Vec<Vec<PokemonSet>> =
            c.teams.iter().map(|t| t.iter().map(|s| s.to_set().unwrap()).collect()).collect();
        let mut b = Battle::new([&sets[0], &sets[1]], c.seed).unwrap();
        for step in &c.steps {
            for side in 0..2 {
                let listed = b.joint_choices(side);
                assert!(!listed.is_empty(), "a side always has at least one legal joint choice");
                for &x in &universe {
                    for &y in &universe {
                        let pair: [Choice; ACTIVE] = [x, y];
                        assert_eq!(
                            b.joint_ok(side, &pair),
                            listed.contains(&pair),
                            "case {} turn {} side {side} {pair:?}",
                            c.id,
                            b.turn
                        );
                    }
                }
            }
            let p1 = Choice::parse_side(&step.choices[0]).unwrap();
            let p2 = Choice::parse_side(&step.choices[1]).unwrap();
            b.choose([p1, p2]).unwrap();
        }
    }
}

#[test]
fn illegal_choices_are_rejected_without_changing_state() {
    let c = &fixture()[0];
    let sets: Vec<Vec<PokemonSet>> = c.teams.iter().map(|t| t.iter().map(|s| s.to_set().unwrap()).collect()).collect();
    let mut b = Battle::new([&sets[0], &sets[1]], c.seed).unwrap();
    let before = b.rng;
    // Both slots switching to the same benched Pokémon.
    let bad = [Choice::Switch { to: 2 }, Choice::Switch { to: 2 }];
    let ok = b.joint_choices(1)[0];
    assert!(b.choose([bad, ok]).is_err());
    assert_eq!(b.rng, before);
    assert_eq!(b.turn, 1);
}
