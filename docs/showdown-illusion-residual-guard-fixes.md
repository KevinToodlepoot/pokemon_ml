# Illusion, residual weather and guard observation fixes

The merged code tested in the uploaded seed-2847160953 run had four failing battles and 20 observation differences, 18 actionable. All four recordings now match across 368 observations and 180 mechanics decisions. These changes affect public observations and inferred Speed, not battle damage or move execution.

| Recording | Cause and correction |
| --- | --- |
| 0026-general-battle-242 | A disguised Pokémon can faint from residual burn without revealing Illusion. Preserve the same identity assignment on both sides of the faint instead of assuming the fainted alias must be the real Pokémon. This retains the surviving Aegislash's Blade form and keeps the fainted position empty. |
| 0047-general-battle-735 | The engine reapplied the copied Pokémon's stale bench item after a doubtful Trick trade. A live alias now supersedes its filed record for item-based Speed inference. The correct public-information range remains 36–184, rather than narrowing to 72–123 by using an unobservable distinction between the real and disguised Pokémon. |
| 0078-general-battle-632 | Toxic damage happens after weather's residual tick. Recognize poison/burn residual damage as the ending phase, so Receiver acquiring Drizzle on the subsequent faint starts rain at age zero until the next turn. |
| 0092-general-battle-178 | Feint clears side protection as well as individual protection. Clear Quick Guard and Wide Guard immediately on Feint or another announced protection-breaking hit, before any forced-switch decision. |

The Illusion recordings have a single native Illusion user per side. Species Clause does not prevent an alias shared by the real Pokémon and its disguise.

## Showdown reference

The pinned Showdown implementation is the authority for public event ordering:

- [Protection breaking](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/battle-actions.ts#L752) removes both individual and side protections before announcing the break.
- [Burn](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L2) and [Toxic](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L138) have residual orders 10 and 9, after weather.
- [Trick](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L19865) reports item arrival and loss under the current visible identity. It does not identify an unrevealed Illusion user. Speed ranges are our inference features, not values supplied by Showdown.

## Validation and reproduction

The four exact recordings, including their battle and generation seeds, are retained in `tests/fixtures/illusion_residual_guard_regressions.jsonl`. The new test checks engine/client parity and directly checks the surviving form, empty position, conservative Speed range, rain age and Quick Guard flag:

```bash
cargo test --locked --test follow --test shown --test speed
```

All 30 integration tests pass, including the earlier saved failures and the randomized Speed soundness test.

The fresh targeted run used base seed **1648207359**, 20 batches of 50 battles, ten themes, each in both open and closed sheet modes. Themes cover Illusion/form/faint, Illusion/item transfers, residual weather/Receiver, Feint/guards/forced switches, type changes, Pressure, item/ability swaps, Mental Herb/Encore, trapping/Octolock and called moves. The archived exact cases guarantee the original triggers are exercised; random themed battles add coverage without guaranteeing every combination occurs.

**1,000 battles: 58,991 observations and 28,716 mechanics decisions, zero differences, failures or unsupported battles.** The 441 pre-existing hidden-action observation pass-overs are reported separately and are not counted as checked observations. Full counts, seeds, source/binary hashes and move-event coverage are in [the results file](showdown-illusion-residual-guard-fixes-results.json). Passing recordings and raw reports are retained at `/workspace/showdown-validation/illusion-residual-guards-1648207359`.

The comparison runner disables Git's filesystem monitor when recording provenance, avoiding the macOS startup hang previously reported. It adds a reusable `--profile regressions` option. The new `scripts/run-showdown-200000.py` wrapper selects a random base seed or accepts an explicit one, builds current sources and writes to `~/showdown-runs/showdown-SEED`. It retains failures and exit codes through the existing runner. Its argument forwarding and exit status were smoke checked without launching 200,000 battles. See [local commands](running-showdown-comparisons.md#fresh-200000-battle-runs-after-merging-fixes).
