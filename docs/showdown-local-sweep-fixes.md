# Fixes from the local 50,000-battle comparison

The subsequent local 100,000-battle run is documented in [the next observation-fix report](showdown-100000-observation-fixes.md).

The uploaded run used engine commit `370cdffef90b83569f6ad4acd5f5ef62a5623276`, pinned Showdown commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8`, and generator seed `20261013`. Its 50,000 battles produced 2,456,660 observation comparisons, 101 differences in five battles (85 at actionable decisions), and zero mechanics divergences. These are remaining paths found in the new run, rather than a replay against the old pre-fix engine.

## Causes and changes

| Saved case | Differences before | Cause and fix |
| --- | ---: | --- |
| `0009-pressure-battle-232` | 1 | Encore replaces Samurott’s choice with Upper Hand and rerolls its target. Armor Tail blocks the move; Showdown removes the target from its move line. The possible opponents have different Pressure costs. The client correctly reported unknown PP, but the training engine exposed its internal exact count. Apply the existing PP uncertainty handling to failed TryMove checks, including Armor Tail, rather than only charging moves. The wait observation now reports unknown PP until an exact move request arrives. |
| `0011-general-battle-857` | 52 | Ceruledge wakes from sleep, passes a confusion check, and uses Reflect. Its action was sorted while sleep activated Quick Feet, but the intervening confusion line cleared the client’s pre-cure status snapshot. Preserve that snapshot through confusion activation/end messages. The Speed upper bound becomes 141 rather than 169, and the same ordering excludes speed-modifying items. |
| `0014-choice-swap-battle-266` | 3 | Oranguru receives Encore, then Mental Herb through Trick before acting. Its queued action remains replaced, but Encore itself has ended. The client used one flag for both facts and wrongly discarded Speed evidence. Snapshot active Encore separately from the replacement timestamp. Retain the replacement for targeting/order, while learning the Speed upper bound of 111 rather than 112 after the cure. |
| `0019-mental-herb-battle-472` | 1 | Despite its profile name, the failure is Transform plus Choice Scarf, Magic Room and Torment. Ditto’s old Transform Choice lock should lapse when Transform is absent from its copied moveset. The client checked its original moveset, kept the stale lock, then refunded PP for a failed Sleep Talk as though it were a Choice refusal. Check copied moves when transformed. The later genuine Choice refusal leaves all five copied Sleep Talk PP intact. |
| `0035-airborne-battle-565` | 44 | Despite its profile name, the failure is Parental Bond Poltergeist. The first hit consumes Sitrus Berry; the second hit announces an empty item slot. The public-log reader replaced the recorded berry loss with an empty held-item entry. An empty Poltergeist announcement now preserves the existing loss history. |

The stress generator can inject abilities and duplicate species. The exact uploaded teams need not be tournament legal; the records reproduce the supported interactions in Showdown. None of these five cases depends on multiple Illusion users.

The training engine change only marks uncertainty on an already failed move with a hidden, potentially mixed-cost target. It does not change damage, actual PP spending, RNG draws or action execution, and introduces no allocation. The remaining changes affect observation reconstruction.

## Showdown reference

The pinned source and the saved public logs are the reference, including the sequence of private requests:

- [Armor Tail removes the target](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L215); [Pressure is deducted before TryMove](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/battle-actions.ts#L467).
- [Sleep cures before confusion is checked](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L47); [Quick Feet applies while statused](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L3749).
- [Champions Encore replaces the queued action](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/mods/champions/moves.ts#L309); [Mental Herb removes Encore](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/items.ts#L3889).
- [Choice lock fails before PP spending and ends when the locked move is absent](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L324).
- [Poltergeist checks the item once, then announces it for each hit](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L13595).

## Reproduction

All five full recordings, including generation metadata, are retained in `tests/fixtures/local_sweep_regressions.jsonl`. Run:

```bash
cargo test --locked --test follow locally_saved_sweep_failures_have_matching_observations
cargo run --locked --release --bin followcheck -- tests/fixtures/local_sweep_regressions.jsonl --list
cargo run --locked --release --bin difftest -- tests/fixtures/local_sweep_regressions.jsonl --by-hand
```

The regression checks both engine/client agreement and the specific expected PP, Speed and consumed-item observations. The captured seeds and before/after results are in `showdown-local-sweep-fixes-results.json`.

## Validation

The five uploaded battles now have zero differences across all 406 observations (359 actionable), and all 198 mechanics decisions match Showdown. The broad archived replay was stopped at the user’s request. Validation uses 1,000 newly generated targeted battles: 200 per interaction group, split equally between open and closed team sheets. All 61,931 checked observations match, and all 30,113 mechanics decisions match. There are no skipped or unfollowed battles; 295 observations use the existing hidden-action pass-over.

The same targeted recordings compared against the pre-fix `370cdff` binary produce 656 observation differences in 21 battles (602 at actionable decisions). These random cases exercise the Quick Feet/confusion and Poltergeist variants; the five exact uploaded recordings directly exercise all five root causes. Log coverage includes 199 Armor Tail blocks, 183 wakes followed by a confusion check, 165 Mental Herb consumptions, 312 successful transformations, and 15 empty Poltergeist hit announcements. These counters establish that the relevant events ran, rather than asserting that every event constitutes the exact original failure.

All 11 follower integration tests pass, including the five direct reference assertions and the existing retained regressions. Formatting and whitespace checks pass. The unrelated full-suite bot benchmark was stopped; no full-suite completion is claimed for this change. The targeted manifest, negative-control cases, seeds, source/binary hashes, and final results are retained in the accompanying results file.

For a fresh run, use a different base seed and output directory:

```bash
python3 scripts/compare-showdown.py --battles 50000 --seed 847291603 --workers 4 --out runs/showdown-847291603
```

See [the local workflow](running-showdown-comparisons.md) for setup and sharing the resulting `differences.tar.gz`.
