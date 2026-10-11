# Fixes from the local 100,000-battle run

The uploaded run used commit `8db777ed2df0f8b17f14a38670f2c99f64f64089`, a clean working tree, generator seed `902681437`, and Pokémon Showdown commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8`. It completed all 100,000 battles: 4,918,229 observation comparisons, 59 differences across ten battles (51 at actionable decisions), and zero mechanics divergences across 2,374,937 decisions. These recordings come from the latest fixed version, rather than the earlier engine.

## Six causes

| Cause | Saved cases | What happened and what changed |
| --- | --- | --- |
| Pressure after public redirection | `0009-pressure-battle-338`, `0025-item-control-battle-694`, `0069-mental-herb-battle-822` | A charging or failed move hides its target, but Rage Powder redirects it before Pressure is charged. The client charged against the selected slot rather than the redirected slot. Resolve public Follow Me/Rage Powder using the known user's types, abilities and current private item; honor powder immunity and moves that track their original target. The targeted tests also found three cases where the move was originally aimed at the user's ally; that selection can be redirected too. |
| Blocked Sleep Talk is one queued action | `0070-mental-herb-battle-543`, `0075-item-control-battle-356`, `0099-called-moves-battle-864` | Showdown emits `cant ... slp`, then `cant ... Taunt/Imprison ... Sleep Talk` for the same attempt. The client counted two actions and lost ordinary move-order evidence. Merge the named second refusal into the pending sleep action, just as for a Sleep Talk move line. Preserve its original Speed snapshot and Round bookkeeping. |
| Waking into Struggle | `0096-general-battle-505` | Rotom with Quick Feet wakes, then Showdown announces `-activate ... move: Struggle` before its move line. That intervening announcement cleared the client's pre-cure status snapshot. Keep the snapshot through the Struggle announcement, so Speed evidence uses the status active when the action was sorted. This is another path through the waking/Quick Feet family addressed previously. |
| Mimicry after Forecast | `0087-general-battle-956` | Castform becomes Snowy through Forecast, then receives Mimicry through Skill Swap with no terrain active. Showdown restores its original Normal type while keeping its Snowy forme. The client treated silent `-end typechange` as restoring the current forme's Ice type. Restore original base types for a Mimicry reset, preserving the forme. |
| A replacement White Herb is a new item | `0097-general-battle-381` | Toxtricity consumes White Herb to clear negative boosts, then receives another White Herb through Symbiosis. The old same-item consumption was mistaken for consumption of the incoming herb. Distinguish the loss that triggered the transfer from a subsequent arrival consumption; clear processed consumption evidence for the recipient. Record Fling as item use too, so its subsequent Symbiosis transfers remain distinguishable. |
| Instruct remembers the earlier target | `0099-called-moves-battle-129` | Audino's original Encore targeted a Pressure opponent. Instruct repeats it using that earlier selected target, while its independently chosen action is Uproar. The client inferred the repeat's PP from the current turn's choice. Retain the last selected target location, use it for Instruct, and restore it if a Choice refusal means the attempted move never began. |

The original ten differences by cause are 15 redirection observations, 13 blocked-Sleep-Talk observations, 22 waking-Struggle observations, four type observations, two item observations and three Instruct observations. One missed inference can persist into later observations; these counts do not represent 59 independent mechanics bugs.

All code changes are in `src/follow.rs` and `src/observer.rs`. The Rust training simulation and its action loop are unchanged. Rebuilding and redirecting a temporary battle is restricted to the client’s PP inference when a hidden target and public redirection need interpretation.

## Showdown reference

The pinned official implementation and the recorded log/request order are the reference:

- [Pressure is deducted after targets are resolved](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/battle-actions.ts#L467), [Follow Me](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L6039), and [Rage Powder](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L14598).
- [Sleep's BeforeMove callback emits `cant slp` but permits sleep-usable moves to continue](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L47). Later Taunt/Imprison checks can stop the same attempt.
- [Quick Feet modifies Speed while statused](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L3749).
- [Mimicry restores `pokemon.baseSpecies.types` without terrain](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L2582).
- [Symbiosis transfers after item use and announces after `setItem`](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L4839).
- [Instruct uses `lastMoveTargetLoc`](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L9641).

## Retained tests and validation

`tests/fixtures/local_100000_regressions.jsonl` contains the ten uploaded battles plus the three allied-target redirection failures found by targeted testing. All 13 retain their complete records and generator metadata. Replay them with:

```bash
cargo test --locked --test follow local_100000
cargo run --locked --release --bin followcheck -- tests/fixtures/local_100000_regressions.jsonl --list
cargo run --locked --release --bin difftest -- tests/fixtures/local_100000_regressions.jsonl --by-hand
```

The tests also directly assert that Castform remains Snowy with Normal type, and that the replacement White Herb remains held. The full follower integration suite includes the previous retained failures, rather than only testing these new records.

Validation uses **1,000 targeted battles**, not another large archived replay. The groups are 200 redirected-Pressure battles, 200 blocked-Sleep-Talk battles, 200 waking-Struggle battles, 100 Forecast/Mimicry battles, 100 replacement-herb battles and 200 Instruct battles. Each group is split equally between open and closed team sheets; the generator base seed is `1739628401`. These are stress teams and can include injected abilities and repeated species, using the same single-Illusion constraints as the local runner.

The first candidate fixed all ten uploads, but the targeted set exposed three allied-target redirection variants. Those are corrected and retained too. The same 1,000 recordings compared against the pre-fix `8db777e` client produce **67 differing observations across 12 battles**, including 56 at actionable decisions. **Final replay: all 62,846 checked observations match, and all 31,793 mechanics decisions match. All 13 retained cases pass across 773 observations, and all 12 follower integration tests pass.** Formatting and whitespace checks pass. Final counts and source/binary hashes are in [the results file](showdown-100000-observation-fixes-results.json). The 2,740 existing hidden-action pass-overs remain explicit in the totals.

Full artifacts and passing targeted recordings are retained under `/workspace/showdown-validation/local-100000-902681437/`; final replay reports are under `final/`. See [the local comparison workflow](running-showdown-comparisons.md) to run a new seed and share its failure bundle.
