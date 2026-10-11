# Fresh 50,000-battle validation after the observation fixes

**Follow-up:** the seven causes below are addressed in [the subsequent fixes](showdown-new-observation-fixes.md). This report preserves the original baseline findings and seeds.

Completed 50,000 newly generated Showdown battles: **2,489,957 compared observations, 81 differing observations (93 scalar features) in 12 battles, grouped into seven confirmed causes**. **71** differences occurred at actionable observations and **10** at wait/switch observations. **All 50,000 battles match Showdown’s mechanics at 1,202,841 decisions.** No engine changes were applied during this sweep.

| Issue | Cause | Battles | Differing observations | Actionable |
| --- | --- | ---: | ---: | ---: |
| OBS-NEW-001 | Confusion self-damage leaves a pending Instruct repeat attached to the later normal move | 1 | 1 | 1 |
| OBS-NEW-002 | Explicit Mimicry type change is overwritten by stale Reflect Type settlement | 2 | 4 | 4 |
| OBS-NEW-003 | Ghost Curse rerolls an allied target but the follower uses static Normal targeting | 4 | 4 | 0 |
| OBS-NEW-004 | A later Mental Herb cure erases evidence of an already replaced Encore action | 1 | 11 | 10 |
| OBS-NEW-005 | Own Speed inference trusts an Illusion alias’s public item over the private request | 2 | 56 | 52 |
| OBS-NEW-006 | A second Imposter transformation reuses PP from the previous copied moveset | 1 | 1 | 0 |
| OBS-NEW-007 | Own Grassy Glide priority is treated as unknown even when grounding is known | 1 | 4 | 4 |


Engine code tested: `a120f9b6e0a7a9c92e07ee338872c2aa9cc73d35`. The new findings below are documented for subsequent fixes; this sweep did not change the engine.

Reference: Pokémon Showdown commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8`, format `gen9championsvgc2026regmc`. No engine changes are made during this sweep.

## OBS-NEW-001 — Instruct interrupted by confusion self-damage

Confirmed in `general-09-battle-235`, decision 6, side 1 (closed sheets). Oranguru uses Instruct on Florges, asking it to repeat Protect. The repeat hurts Florges in confusion. Later in the same turn Florges uses its independently selected Moonblast.

Showdown emits an Instruct marker, confusion activation and self-damage, with no `move` or `cant` for that failed repeat. The follower consumes its pending Instruct marker only when a move or cant begins. Consequently it mistakes the later Moonblast for the immediate repeat. That drops valid Speed comparisons: Sinistcha's normalized minimum Speed is 0.2 in the client versus 0.405 in training, with related item-class and four move-order features also differing. This is one differing observation containing six differing scalar features; mechanics match at all 14 decisions. The frozen pre-fix engine/client pair also reproduces this mismatch, so it predates the pushed fixes.

Authoritative evidence: [Instruct queues a separate prioritized action](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L9662); [confusion produces damage and returns false without a cant](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/conditions.ts#L186). In the follower, the confusion damage handler clears conditions but does not consume the pending repeat; the later action is then classified as order 3 instead of ordinary queue order.

A diagnostic copy inserts a synthetic `cant` at the failed repeat and removes all differences in the complete recording. It is labeled `diagnostic-synthetic-cant.jsonl`, retained separately, and is not an actual Showdown log. The original recording is unchanged.

Potential fix: recognize confusion self-damage as the end of the pending Instruct action and record that attempt separately, without marking the subsequent normal action as instructed. Test both the failed repeat and the later normal move, plus ordinary confusion, sleeping Instruct and Sleep Talk/Snore behavior.

`failures/general-09-battle-235/case.jsonl` (Full recording) · `failures/general-09-battle-235/first-difference.jsonl` (First-difference prefix) · `failures/general-09-battle-235/follow.txt` (All features) · `failures/general-09-battle-235/mechanics.txt` (Mechanics) · `failures/general-09-battle-235/README.md` (Reproduction commands)

## OBS-NEW-002 — Explicit Mimicry change undone by Reflect Type bookkeeping

Confirmed in `general-14-battle-982`, decisions 8–10, side 0 (open sheets). Galarian Stunfisk uses Reflect Type on Eelektross, then its partner sets Psychic Terrain. Mimicry publicly changes Stunfisk to Psychic.

The follower records Psychic on that line but keeps the earlier Reflect Type `copied_foe` flag and pre-copy Ground/Steel types. At the next `turn` line it sees no silent end-of-turn revelation and restores Ground/Steel. Training correctly keeps Psychic. Three observations differ, with three type flags wrong in each; full mechanics match Showdown at all 19 decisions.

This extends the Mimicry fix in the pushed branch: restoration via `-end|typechange` was reconciled, but the ordinary explicit `-start|typechange|Psychic` path still retains the copy flags. Authoritative behavior: [Mimicry sets and announces terrain types](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L2606); [setType marks the new type known and apparent](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/pokemon.ts#L2124). No additional silent announcement is needed. The frozen pre-fix comparator reports no difference here: before the Mimicry correction, training and the follower shared the same stale type-knowledge restoration. Correcting training exposed the remaining client error. Equality between two implementations alone would not establish correct observable knowledge; the explicit Showdown Psychic announcement establishes the expected result.

Potential fix: terminate prior Reflect Type copy bookkeeping when an explicit non-copy type change arrives. Clear `copied`/`copied_foe`, preserving the distinct meaning of the silent revelation flag; prematurely setting that flag caused a different regression in the previous validation. Add order-sensitive regressions for Reflect Type → Mimicry, Mimicry → Reflect Type and Reflect Type → Soak.

`failures/general-14-battle-982/case.jsonl` (Full recording) · `failures/general-14-battle-982/first-difference.jsonl` (First-difference prefix) · `failures/general-14-battle-982/follow.txt` (All features) · `failures/general-14-battle-982/mechanics.txt` (Mechanics) · `failures/general-14-battle-982/README.md` (Reproduction commands)

## OBS-NEW-003 — Ghost Curse rerolls an allied target but the follower uses static Normal targeting

For a Ghost user, Curse aimed at an ally changes its executed target to randomNormal. A failed Curse hides that randomly chosen foe. Training uses the executed target; the follower uses static Normal metadata and does not infer an unaimed move for the chosen allied target. It deducts only base PP, missing either a known extra Pressure cost or the unknown-PP sentinel for mixed Pressure foes.

Evidence:

- Showdown data/moves.ts:3276-3281 changes Ghost Curse to randomNormal for missing/allied targets
- src/follow.rs:1556 uses static MOVES target for unaimed; src/moves.rs:345 uses the modified ActiveMove target
- case 68 has two Pressure foes: training reports 10/12 versus client 11/12; the other three cases have mixed Pressure and training reports unknown while client reports an exact count

Showdown reference: [Curse switches to randomNormal for allied/no target](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L3276).

Potential fix: Classify Curse using the executed target category and the user’s Ghost type, including allied/no-target rerolls. Preserve exact PP when all remaining foes have the same Pressure cost; otherwise hide PP on targetless failure until an exact request arrives. Test ally-targeted Ghost Curse, enemy-targeted Ghost Curse and non-Ghost self Curse.

Occurrences:

- **mental-herb-open-battle-183** — step 4 side 1: own mon: pp: the game has -1 (at 368), the follower 0.75. `failures/mental-herb-open-battle-183/case.jsonl` (Full recording), `failures/mental-herb-open-battle-183/first-difference.jsonl` (first-difference prefix), `failures/mental-herb-open-battle-183/follow.txt` (all features), `failures/mental-herb-open-battle-183/mechanics.txt` (mechanics), `failures/mental-herb-open-battle-183/README.md` (commands).
- **mental-herb-open-battle-829** — step 12 side 0: own mon: pp: the game has -1 (at 263), the follower 0.6666667. `failures/mental-herb-open-battle-829/case.jsonl` (Full recording), `failures/mental-herb-open-battle-829/first-difference.jsonl` (first-difference prefix), `failures/mental-herb-open-battle-829/follow.txt` (all features), `failures/mental-herb-open-battle-829/mechanics.txt` (mechanics), `failures/mental-herb-open-battle-829/README.md` (commands).
- **choice-swap-open-battle-112** — step 7 side 0: own mon: pp: the game has -1 (at 316), the follower 0.75. `failures/choice-swap-open-battle-112/case.jsonl` (Full recording), `failures/choice-swap-open-battle-112/first-difference.jsonl` (first-difference prefix), `failures/choice-swap-open-battle-112/follow.txt` (all features), `failures/choice-swap-open-battle-112/mechanics.txt` (mechanics), `failures/choice-swap-open-battle-112/README.md` (commands).
- **choice-swap-closed-battle-68** — step 9 side 1: own mon: pp: the game has 0.8333333 (at 315), the follower 0.9166667. `failures/choice-swap-closed-battle-68/case.jsonl` (Full recording), `failures/choice-swap-closed-battle-68/first-difference.jsonl` (first-difference prefix), `failures/choice-swap-closed-battle-68/follow.txt` (all features), `failures/choice-swap-closed-battle-68/mechanics.txt` (mechanics), `failures/choice-swap-closed-battle-68/README.md` (commands).


## OBS-NEW-004 — A later Mental Herb cure erases evidence of an already replaced Encore action

Encore replaces queued Thrash with Head Smash while Basculegion lacks Mental Herb. Rotom then transfers a Mental Herb through Trick, curing Encore, but Showdown keeps the already-replaced Head Smash action. The follower unconditionally clears overridden when Encore ends, losing that queue provenance and deriving different Speed bounds.

Evidence:

- Champions Encore data/mods/champions/moves.ts:329-336 changes the action only when the target does not already hold Mental Herb
- src/follow.rs:1808 clears overridden on Encore end; queued_since subsequently loses the replacement time
- Moving only the cure line after Head Smash in a diagnostic copy removes all 11 observation differences; the original recording is unchanged

Showdown reference: [Champions Encore action replacement](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/mods/champions/moves.ts#L329) · [Mental Herb clears the volatile conditions](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/items.ts#L3889).

Potential fix: Separate the active Encore condition from irreversible same-turn queued-action replacement evidence. A herb present before Encore prevents replacement; a herb gained afterward cures the condition without undoing the replacement. Preserve the replacement’s target/Speed inference metadata through the turn.

Occurrences:

- **choice-swap-closed-battle-711** — step 2 side 1: their mon: speed high: the game has 0.44 (at 527), the follower 0.45. `failures/choice-swap-closed-battle-711/case.jsonl` (Full recording), `failures/choice-swap-closed-battle-711/first-difference.jsonl` (first-difference prefix), `failures/choice-swap-closed-battle-711/follow.txt` (all features), `failures/choice-swap-closed-battle-711/mechanics.txt` (mechanics), `failures/choice-swap-closed-battle-711/README.md` (commands).


## OBS-NEW-005 — Own Speed inference trusts an Illusion alias’s public item over the private request

An Illusion Espeon disguised as Alcremie acquires Choice Scarf via Trick. Roar sends it out and brings the actual Alcremie in under the same apparent name. The private request correctly says Mental Herb, but own_now prefers the public apparent Choice Scarf record, computing Alcremie’s effective Speed as 126 instead of 84. A Protect tie-order comparison incorrectly narrows Gallade’s Speed minimum to 126 instead of 100; the bad bound persists.

Scope: One initially brought Illusion user; no multiple-Illusion issue. The saved stress roster injects Illusion on Espeon and is not tournament-legal. The stale apparent-item mechanism can also be tested with a legal Zoroark disguise.

Evidence:

- Saved requests at decisions 21 and 22 list actual Alcremie’s item mentalherb and raw Speed 84
- Follower debug before decision 22 shows OwnNow item 35 (choicescarf), stat 84, despite the exact request
- Showdown data/items.ts:998 Choice Scarf multiplies Speed by 1.5; the saved request establishes that the actual incoming Alcremie does not hold it

Showdown reference: [Choice Scarf Speed multiplier](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/items.ts#L998) · [Illusion’s apparent identity](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/abilities.ts#L2056).

Potential fix: Use the latest private request for the follower’s own held item, applying only subsequent item changes from the current stay. Do not carry an apparent predecessor’s item record across an Illusion alias reused by the actual Pokémon. Add a legal single-Zoroark item-transfer/forced-switch reproduction.

Occurrences:

- **choice-swap-open-battle-757** — step 22 side 0: their mon: speed low: the game has 0.5 (at 526), the follower 0.63. `failures/choice-swap-open-battle-757/case.jsonl` (Full recording), `failures/choice-swap-open-battle-757/first-difference.jsonl` (first-difference prefix), `failures/choice-swap-open-battle-757/follow.txt` (all features), `failures/choice-swap-open-battle-757/mechanics.txt` (mechanics), `failures/choice-swap-open-battle-757/README.md` (commands).


## OBS-NEW-006 — A second Imposter transformation reuses PP from the previous copied moveset

Imposter Orthworm transforms, spends a copied U-turn, leaves via U-turn, and is immediately forced back by Red Card. It transforms into a different target whose moveset also includes U-turn. Showdown creates fresh five-PP copied slots, but the follower reuses the previous copy’s U-turn at four PP because no intervening request cleared its copied cache. A wait request has move names but no exact PP, exposing 4/5 instead of 5/5.

Evidence:

- Showdown sim/pokemon.ts:1306-1325 replaces moveSlots and initializes each to min(5, move.pp)
- Saved decision 3 logs show departure, Red Card drag and a second -transform before a wait request; decision 4’s exact request confirms U-turn 5/5
- src/follow.rs -transform handler does not reset Own.copied; request clears it only if the Pokémon is currently untransformed

Showdown reference: [Transform replaces copied move slots with fresh PP](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/sim/pokemon.ts#L1306).

Potential fix: Treat each successful Transform as a new copied-PP generation. Initialize copied slots to min(5, base PP) and clear copied unknown-PP state, preserving the original moveset’s PP. Test immediate exit/reentry and overlapping move IDs across two targets.

Occurrences:

- **trapping-closed-battle-439** — step 3 side 0: own mon: pp: the game has 1 (at 158), the follower 0.8. `failures/trapping-closed-battle-439/case.jsonl` (Full recording), `failures/trapping-closed-battle-439/first-difference.jsonl` (first-difference prefix), `failures/trapping-closed-battle-439/follow.txt` (all features), `failures/trapping-closed-battle-439/mechanics.txt` (mechanics), `failures/trapping-closed-battle-439/README.md` (commands).


### Additional occurrences of established causes

- **OBS-NEW-002: `airborne-open-battle-350`**, decision 15, side 0: Reflect Type on Slowking is followed by Electric Terrain/Mimicry. The follower restores the pre-copy types instead of keeping the publicly announced Electric type. One differing observation, two type flags; all 141 mechanics decisions match.
- **OBS-NEW-005: `speed-order-open-battle-402`**, decisions 20–52, side 0: the Illusion Inteleon, disguised as Heliolisk, flings Iron Ball. The actual Heliolisk later enters holding Choice Scarf, confirmed by its private request. The public alias still records the earlier item loss. Debugging shows the follower’s `OwnNow` uses itemless Speed 123 rather than Scarf Speed 184; it fails to learn Passimian’s tighter upper bound (0.92 in training versus 0.99 in the client). The original stress roster has one injected Illusion user, not multiple Illusion users. All 53 mechanics decisions match.

Both full recordings, prefixes, feature reports and reproduction commands are retained under their named `failures/` directories.

### Competitive single-Illusion reproduction

`legal-illusion-reproduction/script.json` reproduces OBS-NEW-005 with natural **Zoroark + Illusion**, not an injected ability. Both complete six-Pokémon teams pass Pokémon Showdown’s TeamValidator for `gen9championsvgc2026regmc`, including Species Clause and Item Clause.

Zoroark disguises itself as Heliolisk and flings Iron Ball. The actual Heliolisk then enters holding Choice Scarf, which its private request states. When its Thunderbolt moves before Passimian’s Drain Punch, training learns a tighter Speed upper bound. The client still treats Heliolisk as itemless due to the apparent name’s earlier item-loss record. The normalized bound is 0.99 instead of 0.92. One of ten observations differs; all three mechanics decisions match. This extra scripted three-turn reproduction is **outside the 50,000 generated-battle totals**.

Reproduce from the repository:

```bash
node oracle/gen_cases.js --script oracle/scripts/illusion_private_item.json --log --requests --open-sheets --out /tmp/legal-illusion.jsonl
target/release/followcheck /tmp/legal-illusion.jsonl --list
target/release/difftest /tmp/legal-illusion.jsonl --by-hand
```

## OBS-NEW-007 — Own Grassy Glide loses valid priority information

`called-moves-open-battle-200`, decisions 23–26, side 1. Prankster Encore runs before the player’s grounded Grassy Glide on Grassy Terrain. Both have priority +1, so their order provides a valid Speed lower bound. The training observation narrows the opposing Whimsicott’s normalized minimum to 0.77; the client keeps 0.745. Four observations differ, all actionable; full mechanics match at all 27 decisions. The original stress recording duplicates Whimsicott and is not a tournament-valid team, but the priority interaction does not require duplicate species.

[Showdown’s Grassy Glide callback](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L7666) increases priority when the user is grounded on Grassy Terrain. The follower’s shared priority helper returns unknown for Grassy Glide under that terrain even for its own Pokémon, then removes that action from usable Speed comparisons.

Potential fix: use known own grounding to compute Grassy Glide’s priority, preserving uncertainty for an opponent with unobserved grounding conditions. Test grounded own Glide versus Prankster, airborne own Glide and unknown opposing grounding.

`failures/called-moves-open-battle-200/case.jsonl` (Recording) · `failures/called-moves-open-battle-200/first-difference.jsonl` (Prefix) · `failures/called-moves-open-battle-200/follow.txt` (Features) · `failures/called-moves-open-battle-200/mechanics.txt` (Mechanics) · `failures/called-moves-open-battle-200/README.md` (Commands)

## Replay any retained failure

The original recordings, prefixes, logs, requests, full states, frozen binaries, checksums and analysis are retained under `/workspace/showdown-validation/new-50000-post-fixes-20261011/`; `findings-bundle.tar.gz` contains all isolated failures and the additional validated Zoroark reproduction. These artifact paths refer to the testing workspace.

Each recording can also be regenerated from the pinned Showdown source using its fresh generator seed and original case ID. Run from this repository after installing the oracle dependencies and building `followcheck`/`difftest`:

```bash
cargo build --locked --release --bin followcheck --bin difftest
```

### airborne-open-battle-350

```bash
node oracle/gen_cases.js --seed 97011112 --only 350 --log --requests --out /tmp/showdown-failure.jsonl --moves gravity,smackdown,fly,bounce,magnetrise,skyattack,meteorbeam,phantomforce,dive --moves-per 3 --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 350
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### called-moves-open-battle-200

```bash
node oracle/gen_cases.js --seed 97011120 --only 200 --log --requests --out /tmp/showdown-failure.jsonl --moves instruct,copycat,sleeptalk,encore,transform,uproar --moves-per 3 --abilities pressure,innerfocus,prankster --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 200
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### choice-swap-closed-battle-68

```bash
node oracle/gen_cases.js --seed 97011103 --only 68 --log --requests --out /tmp/showdown-failure.jsonl --abilities klutz,innerfocus,pressure --items choicescarf,mentalherb,leftovers --moves skillswap,encore,magicroom,trick,switcheroo,instruct
target/release/followcheck /tmp/showdown-failure.jsonl --battle 68
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### choice-swap-closed-battle-711

```bash
node oracle/gen_cases.js --seed 97011103 --only 711 --log --requests --out /tmp/showdown-failure.jsonl --abilities klutz,innerfocus,pressure --items choicescarf,mentalherb,leftovers --moves skillswap,encore,magicroom,trick,switcheroo,instruct
target/release/followcheck /tmp/showdown-failure.jsonl --battle 711
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### choice-swap-open-battle-112

```bash
node oracle/gen_cases.js --seed 97011104 --only 112 --log --requests --out /tmp/showdown-failure.jsonl --abilities klutz,innerfocus,pressure --items choicescarf,mentalherb,leftovers --moves skillswap,encore,magicroom,trick,switcheroo,instruct --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 112
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### choice-swap-open-battle-757

```bash
node oracle/gen_cases.js --seed 97011104 --only 757 --log --requests --out /tmp/showdown-failure.jsonl --abilities klutz,innerfocus,pressure --items choicescarf,mentalherb,leftovers --moves skillswap,encore,magicroom,trick,switcheroo,instruct --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 757
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### general-09-battle-235

```bash
node oracle/gen_cases.js --seed 97011009 --only 235 --log --requests --out /tmp/showdown-failure.jsonl
target/release/followcheck /tmp/showdown-failure.jsonl --battle 235
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### general-14-battle-982

```bash
node oracle/gen_cases.js --seed 97011014 --only 982 --log --requests --out /tmp/showdown-failure.jsonl --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 982
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### mental-herb-open-battle-183

```bash
node oracle/gen_cases.js --seed 97011106 --only 183 --log --requests --out /tmp/showdown-failure.jsonl --abilities innerfocus,pressure,klutz --items mentalherb,choicescarf,leftovers --moves encore,taunt,disable,torment,throatchop,imprison --moves-per 3 --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 183
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### mental-herb-open-battle-829

```bash
node oracle/gen_cases.js --seed 97011106 --only 829 --log --requests --out /tmp/showdown-failure.jsonl --abilities innerfocus,pressure,klutz --items mentalherb,choicescarf,leftovers --moves encore,taunt,disable,torment,throatchop,imprison --moves-per 3 --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 829
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### speed-order-open-battle-402

```bash
node oracle/gen_cases.js --seed 97011114 --only 402 --log --requests --out /tmp/showdown-failure.jsonl --moves speedswap,trickroom,tailwind,quash,afteryou,round,allyswitch --moves-per 3 --items choicescarf,quickclaw,ironball,leftovers --open-sheets
target/release/followcheck /tmp/showdown-failure.jsonl --battle 402
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

### trapping-closed-battle-439

```bash
node oracle/gen_cases.js --seed 97011109 --only 439 --log --requests --out /tmp/showdown-failure.jsonl --moves octolock,bind,wrap,uturn,voltswitch,quash,afteryou,round --moves-per 3 --items ejectbutton,redcard,bindingband,leftovers
target/release/followcheck /tmp/showdown-failure.jsonl --battle 439
target/release/difftest /tmp/showdown-failure.jsonl --by-hand
```

## Corpus scope

Generated 52,688 candidates to retain exactly 50,000 complete battles: 30,000 general and 20,000 targeted, half open sheets and half closed sheets. Replaced 94 candidates with multiple initially brought Illusion users and 429 impossible closed-sheet injected Illusion rosters. There were no truncated candidates; 2,165 surplus candidates were unused. The existing hidden-information comparison policy passed over 15,725 observations. No whole accepted battle was skipped or failed to follow.

The generator includes stress rosters with unusual abilities or duplicate species; it is not exclusively a tournament-legal corpus. The natural single-Zoroark reproduction independently passes Showdown’s team validator. This suite checks observation fidelity for fast Rust training and does not benchmark throughput.
