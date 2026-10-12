# Observation fixes from the 300,000-battle run

The uploaded run tested merged commit `ee1e2a7`, including the prior Future Sight/Ripen/Instruct fixes. All 300,000 mechanics replays matched. Ten battles had 60 differing observations, 52 actionable. These are new combinations in familiar observation bug families, rather than the previous saved failures recurring.

| Recording | Relationship to earlier work and final correction |
| --- | --- |
| 992 | Related to residual/forced-switch timing. A poisoned replacement was visible before its queued SwitchIn callback reset the previous Toxic counter. The engine observation now exposes zero for that pending new stay. |
| 733, 236 | Related to Instruct and sleep/confusion ordering. Register confusion self-hits before HP changes; preserve private knowledge of the chosen move. Instructed/promoted actions cannot reveal the priority of an earlier failed queued move merely because their names happen to match. The engine's former tighter Speed bounds used that hidden coincidence, so the corrected bounds can be wider. |
| 834 | Related to Pressure and changing abilities. Mega Delphox acquires Levitate before the next private request. Use the publicly known current ability when rebuilding its grounding; airborne Expanding Force stays single-target and does not pay spread Pressure costs. |
| 970 | Related to Encore/hidden-target PP. An instant Solar Beam's move line omits the target, but its animation names it. Reconcile the extra Pressure deduction from that public animation without inventing PP when the initial payment was capped. |
| 582 | Related to the earlier single-Illusion/Aegislash case. A visible form change identifies the genuine Morpeko among two aliases. Keep the disguise's frozen form separate from the genuine Pokémon's temporary form and from its bench record. |
| 574 | Related to the reflected Heal Pulse/Heal Block fix. The engine must not treat a blocked reflected move as evidence that the reflector owns it. |
| 328 | Related to called moves and Speed Swap. Record successful exchanges explicitly, including Copycat/Instruct, and mark just the two affected Pokémon. A failed attempted swap does not change the inference state. |
| 892 | Related to Quick Claw and Encore. Encore replaces the queued move and invalidates its original Quick Claw fraction; recompute priority evidence from the replacement. |
| 111 | Related to ability replacement/suppression. Baton Passed Gastro Acid is removed for an unsuppressible ability such as Battle Bond when that ability is known from private information or open sheets. |

The ten uploaded recordings now match across 662 observations and 321 mechanics decisions. Existing prior regressions also pass. The observation layout and action space remain unchanged.

## Findings from the requested targeted tests

The 2,000-case set found additional variants that are fixed and retained:

- **Endure and confusion:** confusion's special effect has Showdown's Move effect type. The engine's Endure handler only recognized ordinary move objects, so an instructed self-hit could incorrectly faint a Pokémon that should retain one HP. Use the existing move-effect predicate, which includes confusion. This is a mechanics correction and is checked with continuous replay and rebuild at each decision.
- **Three solar animation cases:** Mega Evolution had replaced a target's Pressure ability. Extra PP must be based on its current revealed ability, not its stale entry-time announcement.
- **Closed-sheet Baton Pass:** an unrevealed Battle Bond silently discards copied suppression. The opponent cannot know that removal; retain the publicly copied suppression state until ability knowledge resolves it. The holder, who knows its own ability, observes the actual removal. An optional, backward-compatible `copied_gastro` field preserves this knowledge through exported/rebuilt positions; it does not add a policy feature or affect suppression mechanics.

All fifteen retained recordings have complete seeds and metadata: the ten uploads in `local_300000_regressions.jsonl`, four targeted variants in `local_300000_targeted_regressions.jsonl`, and the Endure case in `endure_confusion_regression.jsonl`, under `tests/fixtures/`.

## Official reference

The pinned Showdown commit remains `ad7ca5d51c03865e6829dbec3a7875df71480fb8`:

- [Confusion](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L193) constructs a Move effect; [Endure](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L4805) protects against lethal damage from that effect type.
- Gastro Acid's Copy callback removes it for an ability whose flags prohibit suppression. The uploaded open-sheet case and retained closed-sheet case establish the corresponding public event boundaries.
- The exact move, animation, Mega Evolution, Instruct, form-change and replacement event sequences are preserved in the recordings. The reference was inspected locally; fixes do not replace public evidence with hidden team details.

## Verification

```bash
cargo test --locked --test follow --test parity --test shown --test speed --test position
```

**All 59 integration tests pass**, including previous recorded failures, Speed soundness, and position serialization/rebuild checks. Formatting, whitespace and Python syntax checks pass.

Fresh targeted generation used seed **2615837409**, 80 batches of 25 and 27 themes. Every theme has both open and closed team sheets. The first 26 themes have 75 battles each and the final theme has 50. New coverage includes Toxic replacements, confusion/Instruct, Mega/Pressure, solar animations, Hunger Switch/Illusion, reflected-heal knowledge, called Speed Swap, Baton suppression and Encore/Quick Claw. All earlier regression themes remain included.

**Final replay of all 2,000 battles: 122,243 checked observations and 59,496 mechanics decisions, zero differences, follower failures or unsupported cases.** Mechanics also match continuously without rebuilding between decisions. The 749 existing hidden-action pass-overs are reported separately from checked observations.

The first validation attempt exhausted workspace storage. Recordings were preserved, disposable build files and redundant concatenations were removed, and the same seed was regenerated to complete the corpus. Candidate reports retain their findings; final reports are separate. This did not reduce the requested battle count or discard failing cases.

[Results, source/binary hashes and all reproduction seeds](showdown-300000-observation-fixes-results.json). Full completed artifacts are under `/workspace/showdown-validation/local-300000-targeted-2615837409-retry`, with final observation and both mechanics-mode reports in `final/`. No additional large sweep was run.
