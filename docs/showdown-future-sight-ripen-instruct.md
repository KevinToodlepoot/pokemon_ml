# Future Sight, Ripen and instructed sleep fixes

The uploaded seed-3726198057 run checked merged commit `94bdae1` across 200,000 battles. Three battles differed: one mechanics failure and two client observation failures, producing 50 differing observations (46 actionable). The 48 differences in case 699 stem from one incorrect HP loss and its downstream effects, rather than 48 independent bugs.

| Recording | Cause and fix |
| --- | --- |
| 0029-trapping-battle-699 | Future Sight strikes a Red Card holder. The caster is scheduled to be dragged out, so Showdown's Life Orb callback skips recoil. The engine's special delayed-hit path bypassed that guard and removed 19 HP. Check the pending forced-switch flag before applying delayed-hit recoil. |
| 0127-general-battle-591 | Ripen doubles Leppa Berry's restoration to 20 PP. The client restored 10, then a forced switch exposed the wrong PP before a correcting private request. Use the publicly known active Ripen ability and cap restoration at maximum PP. |
| 0160-pressure-battle-746 | Instruct repeats a sleeping Pokémon's Snore, producing both `cant slp` and a move line. The client treated them as separate actions, consuming the original queued action and losing Speed evidence. Merge the matching instructed sleep-usable continuation; retain a subsequent wake as the start of the original action. |

The Future Sight fix changes battle mechanics; the other changes correct the client reconstruction. Normal Life Orb recoil remains in place. No observation layout or model architecture changes are needed.

## Official reference

Showdown was inspected locally at the pinned commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8`:

- [Futuremove resolution](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L393) invokes the Life Orb callback after the delayed hit.
- [Life Orb](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/items.ts#L3404) suppresses recoil when `source.forceSwitchFlag` is set.
- [Leppa Berry](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/items.ts#L3352) restores 20 PP with Ripen, otherwise 10, capped at maximum.
- The saved Instruct/Snore log records `-singleturn ... Instruct`, `cant ... slp`, then `move ... Snore`. The later natural action can wake in that same turn. Both sequences are covered by saved regressions.

## Verification

All three complete recordings and generation seeds are retained in `tests/fixtures/future_sight_ripen_instruct_regressions.jsonl`. Tests check all observations, all mechanics after rebuilding each decision, and explicit HP, PP and Speed feature values.

```bash
cargo test --locked --test follow --test parity --test shown --test speed
```

All **38 integration tests pass**. This includes the older case 150: an instructed sleeping action fails, then its original action wakes and attempts Yawn. An initial overly broad merge failed that regression; the final matching-continuation guard preserves it.

The fresh targeted set used seed **2075841639**, 40 batches of 25, and 18 themes. All themes have open and closed sheet coverage. The three new themes plus waking/Attract have 75 battles each; the other fourteen have 50 each. They include the previous Court Change, reflected healing, Instruct/Quick Claw, Illusion, weather, guard, type, Pressure, item/ability-swap, Mental Herb, trapping/Octolock and called-move issues.

**Final replay: 1,000 battles, 63,867 checked observations and 31,144 mechanics decisions; zero differences, failures or unsupported cases.** The 421 existing hidden-action pass-overs remain reported separately. Uploaded cases match across 320 observations and 157 mechanics decisions.

The recorded logs include 208 Future Sight hits, three steps where a Future Sight hit triggers Red Card, thirteen steps with Life Orb recoil after a Future Sight hit, 21 Leppa activations, five Ripen announcements and 242 Instruct announcements. These are event coverage counts, not independent bug counts or guarantees of every possible interaction.

[Machine-readable results](showdown-future-sight-ripen-instruct-results.json) retain seeds, initial/final results and hashes. Full artifacts are under `/workspace/showdown-validation/future-ripen-instruct-2075841639`, with final reports in `final/`. No 300,000-battle run was launched here.

## Run 300,000 fresh comparisons after merging

```bash
git fetch origin
git switch main
git pull --ff-only origin main
python3 scripts/compare-showdown.py --battles 300000 --seed 3184072951 --workers 4 --out ~/showdown-runs/showdown-3184072951
```

The command builds the merged code and retains all findings and their seeds. Upload `~/showdown-runs/showdown-3184072951/differences.tar.gz` when complete. Use a new seed and directory for a subsequent run.
