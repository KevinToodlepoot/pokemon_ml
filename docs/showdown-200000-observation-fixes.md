# Observation fixes from the seed-2732425018 run

The uploaded 200,000-battle run used merged commit `6d84c36` and the pinned Showdown oracle. It found five battles with 24 observation differences, 19 actionable, and no mechanics divergences. The differences reproduce on that code. The final fixes retain what Showdown publicly reveals; they do not change battle damage or move execution.

| Recording | Cause and correction |
| --- | --- |
| 0011-general-battle-830 | Quick Feet's status bonus applied when the queue was sorted, before waking. An intervening Attract activation erased the follower's remembered sleeping status. Preserve it through that activation until the action begins. |
| 0017-general-battle-843 | Court Change moves a screen with its remaining duration and elapsed age. The engine's observation timer treated it as newly started on the receiving side. Follow the condition's unique ID across both sides when updating screen ages. |
| 0106-general-battle-392 | Magic Bounce attempts to reflect Heal Pulse while its holder is charging Solar Blade. Heal Block refuses the reflected move before its move line. Its cant was mistaken for refusal of the holder's queued Solar Blade, removing charging and attributing Heal Pulse to its moveset. Track the in-progress user/target/move to distinguish this borrowed refusal. |
| 0143-general-battle-605 | Instruct resolves an additional move and can roll Quick Claw again. That activation does not promote the original queued action. Keep the follower's queued fractional-priority evidence separate from an instructed activation. |
| 0187-general-battle-539 | Damage unmasks Illusion after Focus Punch's preparation. The lost-focus cant names Focus Punch under the revealed identity. Record that public move in the engine and use its named priority in follower Speed evidence. |

## Additional findings from targeted testing

The first candidate fixed all five uploads. Its 1,000-battle targeted set exposed three recordings, retained with their generation seeds and options:

- `0001-waking-attract-battle-25`: a Cheri Berry cures paralysis with `[msg]`; the parser treated every messaged cure as waking/thawing and fabricated a sleeping Quick Feet bonus. Restrict that BeforeMove status memory to sleep and freeze cures.
- `0020-illusion-focus-punch-battle-9` and `battle-11`: once the engine correctly names the failed Focus Punch, its Speed inference can use that action's known priority. The follower still suppressed that name. Use the explicitly named attempted move, including Focus Punch after a pivot interruption.

No additional mechanics differences were found. Both corrections are included in the final replay of the same 1,000 recordings.

## Showdown reference

The reference was inspected locally at commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8`:

- [Court Change](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L3032) transfers the existing side-condition state.
- [Focus Punch](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L6001) explicitly emits `cant ... Focus Punch ... Focus Punch` when focus is lost.
- [Quick Claw](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/items.ts#L4989) runs on FractionalPriority; the extra activation during Instruct is preserved in the uploaded log.
- [Heal Block](https://github.com/smogon/pokemon-showdown/blob/ad7ca5d51c03865e6829dbec3a7875df71480fb8/data/moves.ts#L8273) can refuse a move in ModifyMove, before its move line. The recorded Heal Pulse/Magic Bounce sequence demonstrates that distinction.

## Verification

`tests/fixtures/local_200000_regressions.jsonl` retains all eight recordings and their seeds. The follower test checks whole-battle parity plus explicit expected feature values for each cause.

```bash
cargo test --locked --test follow --test shown --test speed
```

All **31 relevant integration tests pass**, including previous uploaded failures and the Speed soundness test. An additional environment suite passed ten checks; its unrelated long-running baseline-strength benchmark was cancelled rather than counted as passed.

Fresh generation used base seed **1546319087**, 40 batches of 25, and 15 themes. Every theme has both open and closed sheet coverage. The first ten themes have 75 battles each and the remaining five have 50. Besides the five new themes, coverage includes Illusion forms/items, residual weather, broken guards, type changes, Pressure, Choice/ability swaps, Mental Herb, trapping/Octolock and called moves.

**Final replay: all 1,000 battles match across 62,396 checked observations and 30,374 mechanics decisions. Zero observation differences, follower failures, unsupported cases or mechanics divergences.** The 352 existing hidden-action pass-overs remain separately reported. The eight retained recordings match across 482 observations and 233 decisions. Initial and final results, reproduction seeds and hashes are in [the results JSON](showdown-200000-observation-fixes-results.json).

Full targeted recordings and initial findings are under `/workspace/showdown-validation/local-200000-targeted-1546319087`; final replay reports are in its `final/` directory. The original upload is under `/workspace/showdown-validation/new-200000`.

## Run another 200,000 battles

Use the tested branch before merging, or merged `main` afterwards:

```bash
git fetch origin
git switch fix/showdown-200000-observations
git pull --ff-only origin fix/showdown-200000-observations
python3 scripts/run-showdown-200000.py --seed 3489102761
```

This builds the checked-out fixes and saves the complete run to `~/showdown-runs/showdown-3489102761`. Upload its `differences.tar.gz`. To save directly on the Desktop, add `--out ~/Desktop/showdown-3489102761`. No 200,000-battle run was launched during this task.
