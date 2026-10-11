# Fixes for the seven fresh observation failures

The later local run found five additional cases on this fixed version. See [their causes and fixes](showdown-local-sweep-fixes.md).

The 12 battles from the fresh 50,000-battle sweep had 81 differing observations (93 scalar features). The changes below address their seven confirmed causes using pinned Pokémon Showdown commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8` as the reference.

| Cause | Fix |
| --- | --- |
| Instruct interrupted by confusion | Record and consume the failed immediate repeat on confusion self-damage, even without a `move` or `cant` line. Keep the later independently queued move in normal order. |
| Reflect Type followed by Mimicry | Explicit type changes supersede pending Reflect Type bookkeeping. Keep silent turn-end revelation bookkeeping distinct. |
| Ghost Curse rerolls an allied target | Use the executed target category, accounting for Ghost type (including an added Ghost type). Infer random-target Pressure PP only when all possible targets agree; otherwise retain unknown PP until an exact request. Non-Ghost Curse remains self-targeted. |
| Mental Herb received after Encore | Distinguish an immediate cure from a later cure after another action. Preserve evidence that Encore already replaced the queued move, even after its volatile condition ends. |
| Illusion alias item information | Infer own Speed from the exact private held item plus subsequent item changes during that stay, rather than another Pokémon's cached public item under the same apparent name. Include Symbiosis transfers, which use `-activate`, and exclude duplicate consumption messages. |
| Second Transform shares move IDs | Reset copied PP and its uncertainty on every successful transformation, preserving original PP. Fresh copied slots start at `min(5, base PP)`, even when no intervening untransformed request arrives. |
| Own Grassy Glide priority | Calculate grounding from known own items, abilities, types and field conditions at the relevant action snapshot. Learn Speed bounds from its terrain priority. Keep uncertain grounding and opponents' hidden grounding unknown. |

The underlying log-follower changes do not add work to the training engine's action loop. The training-side type fix clears a few existing knowledge flags when types change.

## An additional shared type-knowledge error

Broad regression checking exposed an older case (`716`, decision 16) where Reflect Type is followed by Soak. Showdown explicitly announces Water and `Pokemon.setType` sets `knownType`/`apparentType`. Previously both training and the follower could restore the pre-copy types at turn cleanup, so their equality concealed the incorrect knowledge.

Type replacement now clears stale copy settlement consistently in `Battle::set_type`, covering Soak and other explicit changes as well as Mimicry. Reflect Type establishes its own copy bookkeeping after calling that helper. A regression asserts the public Water result directly, in addition to observation equality.

## Transfer regressions found during validation

Preferring the private item also requires preserving every later transfer. The first implementation missed Symbiosis, which announces its donor and recipient using `-activate` without an accompanying `-item`. Nine older and twelve fresh recordings exposed incorrect Speed bounds as Iron Ball or Choice Scarf moved between partners. Both sides of that transfer now update the current stay's item override. Arrival lines respect items consumed immediately on arrival; empty Fling, Cud Chew and duplicate `[weaken]` lines do not remove a newly received item.

Those 21 complete recordings are retained in `tests/fixtures/item_transfer_regressions.jsonl`; all 1,103 observations pass after the correction. This was a regression caught and corrected while validating the new changes, rather than a new baseline failure added to the original 81 count.

## Reference behavior

- [Confusion can return false after self-damage without emitting cant](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L186).
- [setType makes the new types known and apparent](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/pokemon.ts#L2124).
- [Ghost Curse changes an allied/missing target to randomNormal](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L3276).
- [Champions Encore replaces an existing queued action unless Mental Herb prevents it](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/mods/champions/moves.ts#L329), while [Mental Herb only removes volatile conditions](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/items.ts#L3907).
- [Symbiosis transfers the item and emits only its activate line](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L4839).
- [Transform creates fresh copied PP slots](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/pokemon.ts#L1306).
- [Grassy Glide adds priority only when grounded on Grassy Terrain](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L7666).

The original observations, occurrences, causes and reproduction seeds remain in [the fresh sweep report](showdown-fresh-50000.md). The twelve complete client recordings and legal single-Zoroark reproduction are retained in `tests/fixtures/fresh_sweep_regressions.jsonl`; full state recordings remain in the original validation artifacts. No comparison exclusions were added to make these regressions pass.

## Validation

- All 12 original failing battles: **941 observations, zero differences**. The legal single-Zoroark reproduction adds **10 observations, zero differences**.
- All 43 earlier sweep regressions: **2,617 observations, zero differences**.
- All 21 transfer regressions: **1,103 observations, zero differences**. In total, **77 retained recordings and 4,671 observations pass**.
- Final replay of **both archived 50,000-battle corpora**: **4,970,666 observations, zero differences**, with no skipped or unfollowed battles. The existing **31,818 hidden-information pass-overs** remain unchanged.
- Full-state/rebuilding mechanics comparison: **100,000 battles and 2,401,242 decisions, zero divergences**. The mechanics replay uses the same engine sources as the final observation replay; the later transfer correction changes only `src/follow.rs`.
- All **106 Rust tests pass**; the complete follower test suite was rerun after expanding its transfer fixture to all 21 cases. Rebuilt Python extension: **10 tests pass**, one live-server test skips because `SHOWDOWN_SERVER` is not configured. Formatting, whitespace and Python syntax checks pass.
- Local runner: 100 battles exercise all ten targeted profiles plus general/open/closed modes. A separate 20-battle run verifies its default build and generation workflow. A negative control using the pre-fix engine with the report-enabled CLI rediscovers original battle **235**, seed **97011009**, saves its full recording and both seed types, and produces a readable archive with an expected exit status of 1.

Machine-readable [final results](showdown-new-observation-fixes-results.json) and [transfer regression seeds](showdown-item-transfer-regressions.json) are committed with the changes. The initial candidate's transfer regressions remain documented separately from the final zero-difference result. Full artifacts are retained under `/workspace/showdown-validation/new-observation-fixes-20261011/`, with the corrected follower's reports under `final-observations/`.

See [local comparison instructions](running-showdown-comparisons.md) to generate new battles and send back complete failure bundles.
