//! Checks the log follower against recorded Showdown battles.
//!
//!     followcheck CASES.jsonl... [--limit N] [--battle ID] [--list] [--slim OUT.jsonl] [--report OUT.json] [--export]
//!                                [--require-state]
//!
//! The battles must have been recorded with their logs and requests
//! (`oracle/gen_cases.js --log --requests`). Each is played in the
//! simulator, and beside it a follower for each side reads what Showdown
//! sent that side. Printed: how many observations were compared, how many
//! differed, and which features differed how often. Where a recording
//! carries Showdown's state at each point (`oracle/snapshot.js`), the
//! game's state must equal it at the start and after every step (D74);
//! `--require-state` fails a battle recorded without it at every step.
//!
//! `--export` also takes each follower's `game_state` at every decision
//! (`replay::check_export`): it must load, list both sides' choices and
//! observe as its follower does, show the other side's HP and bar as the
//! follower was shown them, and count the hidden-length effects as the game
//! does; refusals are counted by reason. Any export that does otherwise fails.

use std::io::{BufRead, BufReader};

use vgc_engine::replay::{ExportTally, FollowCase, FollowTally, Outcome, check_export, check_follow};

fn main() {
    let mut files = Vec::new();
    let mut limit = usize::MAX;
    let mut only = None;
    let mut slim = None;
    let mut report = None;
    let mut records = Vec::new();
    let mut list = false;
    let mut export = None;
    let mut require_state = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--limit" {
            limit = args.next().and_then(|n| n.parse().ok()).expect("--limit takes a number");
        } else if arg == "--report" {
            report = Some(args.next().expect("--report takes a JSON path"));
        } else if arg == "--slim" {
            // Write the battles followed back out with only what this check reads.
            let path = args.next().expect("--slim takes a file to write");
            slim = Some(std::fs::File::create(&path).unwrap_or_else(|e| panic!("cannot write {path}: {e}")));
        } else if arg == "--export" {
            export = Some(ExportTally::default());
        } else if arg == "--require-state" {
            // Every point of every battle must carry Showdown's state, so that all of it is compared.
            require_state = true;
        } else if arg == "--list" {
            // The first difference of every battle that has one.
            list = true;
        } else if arg == "--battle" {
            // Every difference found in this one battle.
            only = Some(args.next().and_then(|n| n.parse::<u32>().ok()).expect("--battle takes a number"));
        } else {
            files.push(arg);
        }
    }
    if files.is_empty() {
        eprintln!(
            "usage: followcheck CASES.jsonl... [--limit N] [--battle ID] [--list] [--slim OUT.jsonl] \
             [--report OUT.json] [--export] [--require-state]"
        );
        std::process::exit(2);
    }
    let mut tally = FollowTally { verbose: only.is_some(), ..FollowTally::default() };
    let (mut battles, mut decisions, mut skipped, mut failed) = (0, 0, 0, Vec::new());
    // Points (the start and each step) whose state was compared with Showdown's, in battles followed.
    let mut states = 0;
    let mut reasons = std::collections::BTreeMap::new();
    'files: for path in &files {
        let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
        for line in BufReader::new(file).lines() {
            if battles + skipped + failed.len() >= limit {
                break 'files;
            }
            let line = line.expect("a readable line");
            let case: FollowCase = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{path}: {e}"));
            if only.is_some_and(|id| id != case.id) {
                continue;
            }
            let before = (
                tally.observations,
                tally.wrong,
                tally.choices,
                tally.wrong_choices,
                tally.hidden_disables,
                tally.first_wrong.len(),
            );
            let outcome = if require_state && !case.has_every_state() {
                Outcome::Fail(vec![format!(
                    "recorded with Showdown's state at {} of its {} points (--require-state)",
                    case.states(),
                    case.steps.len() + 1
                )])
            } else {
                check_follow(&case, &mut tally)
            };
            if let Some(ex) = export.as_mut()
                && matches!(outcome, Outcome::Pass(_))
                && let Outcome::Fail(why) = check_export(&case, ex)
            {
                ex.broken += 1;
                if ex.first.len() < 20 {
                    ex.first.push(format!("battle {}: {}", case.id, why.join("; ")));
                }
            }
            if report.is_some() {
                let (status, decisions, errors) = match &outcome {
                    Outcome::Pass(n) => ("followed", *n, Vec::new()),
                    Outcome::Unsupported(why) => ("unsupported", 0, vec![why.clone()]),
                    Outcome::Fail(why) => ("failed", 0, why.clone()),
                };
                records.push(serde_json::json!({
                    "id": case.id, "status": status, "decisions": decisions, "errors": errors, "states": case.states(),
                    "observations": tally.observations - before.0,
                    "differences": tally.wrong - before.1,
                    "actionable_observations": tally.choices - before.2,
                    "actionable_differences": tally.wrong_choices - before.3,
                    "passed_over": tally.hidden_disables - before.4,
                    "first_differences": &tally.first_wrong[before.5..],
                }));
            }
            match outcome {
                Outcome::Pass(n) => {
                    battles += 1;
                    decisions += n;
                    states += case.states();
                    if let Some(out) = slim.as_mut() {
                        use std::io::Write;
                        writeln!(out, "{}", serde_json::to_string(&case).expect("a case is serialisable"))
                            .expect("a writable file");
                    }
                }
                Outcome::Unsupported(why) => {
                    skipped += 1;
                    *reasons.entry(why).or_insert(0usize) += 1;
                }
                Outcome::Fail(why) => failed.push(format!("battle {}: {}", case.id, why.join("; "))),
            }
        }
    }
    println!(
        "{battles} battles followed to the end ({decisions} decisions), {skipped} skipped, {} not followed",
        failed.len()
    );
    for (why, n) in &reasons {
        println!("  skipped {n}: {why}");
    }
    for why in failed.iter().take(20) {
        println!("  {why}");
    }
    println!(
        "{} observations compared, {} differ ({:.2}%); {} passed over (a move or a switch barred in a way the player is not shown)",
        tally.observations,
        tally.wrong,
        100.0 * tally.wrong as f64 / tally.observations.max(1) as f64,
        tally.hidden_disables
    );
    // (The state is the whole battle's: the points passed over above are compared like every other.)
    println!("states compared with Showdown's: {states} (every point of every battle followed that carries one)");
    println!(
        "of those, {} are ones the player has a choice to make at: {} differ ({:.3}%)",
        tally.choices,
        tally.wrong_choices,
        100.0 * tally.wrong_choices as f64 / tally.choices.max(1) as f64
    );
    let mut rows: Vec<_> = tally.by_feature.iter().collect();
    rows.sort_by_key(|(_, (n, _))| std::cmp::Reverse(*n));
    for (name, (n, example)) in rows {
        println!("{n:>8}  {name}\n          {example}");
    }
    // Which of the observation's counts (obs-2) the recording reached at all, and how often.
    let counted: Vec<String> = tally.counted.iter().map(|(name, n)| format!("{name} {n}")).collect();
    let counted = if counted.is_empty() { "none".into() } else { counted.join(", ") };
    println!("counts above 0 in the game's observations: {counted}");
    // Lines of a kind the followers do not know (D84): reported, so that nothing is read past unseen.
    let unknown: Vec<String> = tally.unknown.iter().map(|(kind, n)| format!("{kind:?} {n}")).collect();
    println!("unknown lines: {}", if unknown.is_empty() { "none".into() } else { unknown.join(", ") });
    if list {
        for first in &tally.first_wrong {
            println!("first: {first}");
        }
    }
    let mut bad_export = false;
    if let Some(ex) = &export {
        println!(
            "export: {} decisions asked, {} refused {:?}, {} broken, {} observe differently from their follower, \
             {} with counts unlike the game's, {} showing the other side's HP or bar otherwise; \
             not told (D89, counted) {:?}",
            ex.asked,
            ex.refused.values().sum::<usize>(),
            ex.refused,
            ex.broken,
            ex.observed_differently,
            ex.counts_differ,
            ex.hp_bar_differ,
            ex.unsaid
        );
        for first in ex.first.iter().take(if list { usize::MAX } else { 10 }) {
            println!("  export: {first}");
        }
        // What a search at play starts from must be what its player was told (D74): an export that
        // observes otherwise than its follower, counts otherwise than the game or shows the other
        // side's HP otherwise fails, as one that does not load does.
        bad_export = ex.broken > 0 || ex.observed_differently > 0 || ex.counts_differ > 0 || ex.hp_bar_differ > 0;
    }
    if let Some(path) = report {
        let export = export.as_ref().map(|ex| {
            serde_json::json!({"asked": ex.asked, "refused": ex.refused, "broken": ex.broken,
                "observed_differently": ex.observed_differently, "counts_differ": ex.counts_differ,
                "hp_bar_differ": ex.hp_bar_differ, "unsaid": ex.unsaid, "first": ex.first})
        });
        let output = serde_json::json!({
            "schema_version": 2, "cases": records,
            "summary": {"followed": battles, "decisions": decisions, "unsupported": skipped,
                "failed": failed.len(), "observations": tally.observations,
                "differences": tally.wrong, "actionable_differences": tally.wrong_choices,
                "passed_over": tally.hidden_disables, "states": states, "counted": tally.counted,
                "unknown_lines": tally.unknown, "export": export},
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&output).unwrap())
            .unwrap_or_else(|e| panic!("cannot write report {path}: {e}"));
    }
    if !failed.is_empty() || tally.wrong > 0 || bad_export {
        std::process::exit(1);
    }
}
