# Fixes from the 50,000-battle Showdown sweep

The sweep retained 41 differing battles: 241 observation differences and one full-state mechanics divergence. The fixes below use Pokémon Showdown commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8`, format `gen9championsvgc2026regmc`, as the reference. The original recordings and baseline reports remain under `/workspace/showdown-validation/fresh-50000-20261011`; new validation reports are under `/workspace/showdown-validation/fixes-20261011`.

## Changes and reference behavior

| Issue | Saved battles | Change |
| --- | ---: | --- |
| OBS-001: Transform critical boosts | 1 | Clear the user's Dragon Cheer/Focus Energy and copy the target's corresponding volatiles. Transform does this without separate end lines. |
| ENG-001: detached field expiration | 1 | Retain the queued terrain/weather duration. A detached last-turn timer clears the current field condition before the stale-handler identity check, matching Showdown's residual ordering. Other stale handlers retain their identity safeguards. |
| OBS-002: Illusion and Speed Swap | 1 | Record actual switches in the Speed event stream and reset the incoming stay's swapped marker, even when it reuses the apparent roster number of a disguise. Merely revealing Illusion does not reset it. |
| OBS-003: flung Mental Herb ownership | 1 | A Mental Herb cure's `-end` tag does not establish the recipient's held item. The explicit item-loss line establishes who lost it. |
| OBS-004: retaliation after attacker returns | 3 | Keep attacker identity and its original attacking slot across an exit/return, pruning at turn cleanup according to whether that attacker remains active. Metal Burst/Comeuppance use the old slot, not the attacker's new position. |
| OBS-005: Encore retargeting | 22 | When Encore actually replaces the queued action, discard its original target evidence. Resolve known Pressure costs, or mark PP unknown when the hidden random target could cost different amounts. Exact requests restore PP knowledge. Training and the client apply the same rule; actual PP deduction is unchanged. |
| OBS-006: Mimicry type resets | 2 | Clear added types and copy bookkeeping on restoration. Training also prevents a same-turn Reflect Type from undoing Mimicry's later reset during observation settlement. |
| OBS-007: interrupted Round chains | 6 | Track one pending promotion, consume it on the next action, and only continue a chain when an executed Round reaches its handler. Use Showdown's explicit `from: Round` evidence for promoted move lines. |
| OBS-008: theft and item-class inference | 2 | Deduplicate the victim's loss: `-enditem` and the recipient's later `-item ... [of] victim` describe one transfer. |
| OBS-009: Expanding Force Pressure | 1 | Compute its executed spread targeting under Psychic Terrain for a grounded user, rather than relying on static Normal-target metadata. Pay Pressure to both opposing targets. |
| OBS-010: sleeping Instruct | 1 | Keep a sleeping failed Instruct repeat distinct from the Pokémon's subsequent queued move. Preserve the legitimate sleep/called-move merge used by Sleep Talk. |

Encore's replacement marker survives queued-action position serialization. Substitute's target-stripping failure path also hides ambiguous PP, including its null return, so training does not expose a count that the client cannot know.

### Two details established during implementation

**Item theft:** The initially uncertain cause was duplicate loss evidence. The first loss can contradict historical Speed-item hypotheses, after which the inference code resets current item classes. Applying the same loss again can then incorrectly accept it as proof of the original registered item owner. Item-clause propagation incorrectly excludes Iron Ball or Choice Scarf from teammates. Deduplicating the transfer fixes both saved occurrences without changing Klutz mechanics.

**Round:** In three saved cases, the first Round promotes another queued Round user that then faints. The promotion is consumed by that canceled action, so the next surviving Round is ordinary. The absence of Showdown's `from: Round` annotation matters here. Other occurrences involve sleep, Imprison, or confusion interrupting the chain. A nameless sleep `cant` can tentatively suggest a promoted Round, but the subsequent Snore/Sleep Talk line disproves that inference; rollback preserves any independent After You ordering. This refines the original report's provisional explanation of the targetless cases.

### Regressions caught during the full replay

The first replay after fixing the original 41 cases found three observation differences in two previously passing recordings, with no mechanics divergences. Both are retained under `/workspace/showdown-validation/fixes-20261011/new-regressions` and included in the regression fixture:

- `types-open-battle-716`, decision 16, side 1: Mimicry runs before Reflect Type and Soak. Its reset must clear copy bookkeeping without claiming that an end-of-turn type revelation has occurred. Setting `told` prematurely prevents the normal restoration of the client's prior type knowledge. The corrected reset clears that flag.
- `trapping-open-battle-469`, decisions 19–20, side 1: a Round is followed by a nameless sleeping `cant` and Snore. Inferring Round promotion from the `cant` alone incorrectly relaxes Speed bounds. The following move disproves the inference, so the follower restores the ordering evidence present before the tentative Round promotion.

## Authoritative source

These are links to the pinned source actually inspected, including the Champions override rather than assuming base-generation Encore behavior:

- [Transform clears/copies critical-hit volatiles](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/pokemon.ts#L1331).
- [Residual expiration before identity checks](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/battle.ts#L519).
- [Champions Encore replaces the action without its original target](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/mods/champions/moves.ts#L309).
- [Metal Burst targets the prior damage slot](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L11670) and [active-attacker cleanup](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/battle.ts#L1707).
- [Mimicry](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L2582) and [setType clearing addedType](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/pokemon.ts#L2109).
- [Round promotes one queued action in onTry](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L15498).
- [Thief's paired transfer messages](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L19302) and [Pickpocket](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L3240).
- [Expanding Force's target modification](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L4943) and [Instruct's separate immediate action](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L9641).

## Regressions and validation

`tests/fixtures/sweep_regressions.jsonl` retains all 41 original failures and two additional regressions as 43 complete client recordings, including their seeds, rosters, choices, both players' requests and logs. `saved_sweep_failures_have_matching_observations` checks all 2,617 observations with an explicit zero-differences assertion. `tests/fixtures/terrain_replacement.jsonl` retains the original mechanics reproduction through the divergent decision; the parity test checks continuous replay and rebuilding at every decision.

Run from the repository:

```bash
cargo test --locked --release
cargo build --locked --release --bin followcheck --bin difftest
cargo fmt --check
maturin develop --release --locked
pytest -q python/tests
```

The sweep uses mechanics stress teams, sometimes with unusual abilities or duplicate species. Multiple initially brought Illusion users were excluded under the agreed scope. Existing hidden-information comparison exclusions remain in place; no exclusions or observations were removed to make these regressions pass.

### Final results

- 43 saved regression battles: 2,617 observations, zero differences; the original 41 failures and both additional cases also pass rebuilding mechanics replay.
- Full replay of the same 50,000 recordings: 2,480,709 observations and 1,198,401 decisions, zero observation differences and zero mechanics divergences. Zero skipped or unfollowed battles; the original 16,093 hidden-information comparisons passed over remain unchanged.
- All 103 Rust tests pass, including documentation tests; formatting and whitespace checks pass.
- Rebuilt Python extension: 10 integration/training tests pass. One live-server test is skipped because `SHOWDOWN_SERVER` is not configured.

Detailed machine-readable results and per-batch logs: `/workspace/showdown-validation/fixes-20261011/sweep-final/summary.json`.
