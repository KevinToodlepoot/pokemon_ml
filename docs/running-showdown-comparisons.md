# Run Showdown comparisons locally and retain differences

The comparison runner generates battles in the pinned Pokémon Showdown simulator, then compares both observations (`followcheck`) and mechanics (`difftest --by-hand`) with the Rust engine. It saves every differing, unfollowed or unsupported battle separately, including its full recording and seeds.

## Setup

Use Git, current stable Rust/Cargo, Node.js **22 or newer**, npm, and Python **3.10 or newer**. Run these commands from the repository on Linux or macOS; on Windows, use WSL.

```bash
git fetch origin
git checkout fix/showdown-observation-parity
git pull --ff-only
bash scripts/setup-oracle.sh
```

The setup script fetches and builds Showdown commit `ad7ca5d51c03865e6829dbec3a7875df71480fb8`. The oracle currently uses `gen9championsvgc2026regmc`. Comparisons run locally; no Showdown server, account or Python extension installation is needed.

Start with a small run:

```bash
python3 scripts/compare-showdown.py \
  --battles 100 --seed 20261012 \
  --batch-size 50 --workers 2 \
  --out runs/showdown-smoke-20261012
```

Then run 50,000 complete battles:

```bash
python3 scripts/compare-showdown.py \
  --battles 50000 --seed 20261013 \
  --workers 4 \
  --out runs/showdown-50000-20261013
```

Use a **new base seed and output directory** for each fresh run. Batch seeds are spaced apart to avoid simply reusing the preceding run's batches when the base seed increases by one. Identical seeds, options and generator versions regenerate the same battles. The runner refuses to overwrite an existing directory.

The runner builds the current Rust code with `cargo build --locked --release` and freezes copies of both executables for the entire run. Their hashes and the engine/Showdown commits are recorded in `provenance.json`. `--no-build` explicitly opts into existing release executables; their hashes are still recorded, but their correspondence to current source is not established by a build.

## Results to send back

Upload the resulting **`differences.tar.gz`**. It contains:

- `failures.json`: every failing battle's generator seed, original generator battle ID, four-word simulator seed, sheet mode, options, first observation difference and mechanics result.
- `failures/<name>/case.jsonl`: complete teams, choices, states, battle logs and both players' private requests. These recordings are enough to replay without regenerating the battle.
- `failures/<name>/follow.txt` and `mechanics.txt`: detailed differences and the mechanics comparison.
- `provenance.json`, `manifest.json`, `summary.json`, `results.json` and `working-tree.patch`: the tested versions, generator hashes, batch settings, totals and any tracked local changes.

The reports and failure recordings are written as batches finish, so discoveries survive a later failed batch. `summary.json` updates during the run. A completed run exits **0** with no findings, **1** when differences or unsupported cases were saved, and **2** for an incomplete run/error. Unsupported battles are labeled separately from observation differences. If the run crashes before producing the final archive, send the existing `failures/` directory and JSON reports; the current batch recording is retained too.

You can send just one failure directory together with `provenance.json`. **Seeds alone are insufficient:** also preserve the original battle ID, generation options and versions. The four-word simulator seed alone does not recreate the randomly generated teams and choices.

## Replay a saved failure

From the repository, substitute the actual failure directory:

```bash
cargo build --locked --release --bin followcheck --bin difftest
target/release/followcheck runs/showdown-50000-20261013/failures/NAME/case.jsonl --list
target/release/difftest runs/showdown-50000-20261013/failures/NAME/case.jsonl --by-hand
```

Use `followcheck ... --battle ID` to print every scalar-feature difference for the recording's `id`. `followcheck` exits 1 for differing observations as well as follower errors. Both tools accept `--report output.json` for machine-readable per-battle results (`difftest` in mechanics mode).

To regenerate rather than replay, use the generator seed, **original** battle ID and options from `metadata.json`:

```bash
node oracle/gen_cases.js --seed GENERATOR_SEED --only ORIGINAL_ID \
  --log --requests --out /tmp/reproduction.jsonl GENERATION_OPTIONS
target/release/followcheck /tmp/reproduction.jsonl --list
```

The regenerated battle uses its original ID; the runner's saved recording uses a unique ID within its batch. Preserve the pinned source version when regenerating.

## Scope and resource use

With the default 1,000-battle batches, the mixed profile runs 30,000 general and 20,000 targeted battles in a 50,000-battle run, alternating open and closed sheets. The targeted groups cover types, Pressure, Encore/items, trapping, grounding, Speed ordering, ability changes and called moves. `--profile general` generates only general battles.

This is a mechanics stress corpus: some teams have injected abilities or duplicate species. The runner excludes multiple initially brought Illusion users per side and impossible closed-sheet rosters with non-native Illusion, matching the earlier validation scope. It replaces excluded and truncated candidates until it has the requested number of complete recordings. Comparison reports retain the existing hidden-information pass-over count; they do not claim those observations were checked.

Passing recordings are removed after comparison by default, keeping disk use bounded by active batches. `--keep-passing` retains every full recording and can consume substantial disk space. Lower `--workers` or `--batch-size` if memory or temporary disk space is tight. Failure recordings and reports are always retained.
