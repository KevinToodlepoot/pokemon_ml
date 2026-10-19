#!/usr/bin/env python3
"""Makes a fixture battle out of a recorded one: the battle played again at the Showdown PS_DIR names
(or oracle/pokemon-showdown) from its own first STEPS choices (`gen_cases.js --script`, which stops
when a script's choices run out), with Showdown's log, its requests and its state at every point
(`--log --requests --open-sheets` as the recording had them), under NAME, written to stdout. It is
checked as it is written: the engine plays it as Showdown did (`difftest --strict`), and a follower
reads it to the end with Showdown's state held at every step (`followcheck --export
--require-state`, which may fail: that is what a regression battle shows before its fix).

  scripts/fixture_from.py RECORDING.jsonl ID STEPS "NAME" >> tests/fixtures/FILE.jsonl

Only Dex-set battles go into the fork's fixtures (random or scripted `gen_cases` teams), never a
pool team: the fork is public.
"""
import json
import os
import subprocess
import sys
import tempfile

from rerecord import DIFFTEST, FOLLOWCHECK, GEN, script_of


def main():
    if len(sys.argv) != 5:
        sys.exit(__doc__)
    path, wanted, steps, name = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
    case = next((c for c in map(json.loads, open(path)) if c["id"] == wanted), None)
    if case is None:
        sys.exit(f"no battle {wanted} in {path}")
    script = script_of(case)
    script["choices"] = script["choices"][:steps]
    script["name"] = name
    with tempfile.TemporaryDirectory() as work:
        spec, out = os.path.join(work, "script.json"), os.path.join(work, "out.jsonl")
        with open(spec, "w") as f:
            json.dump([script], f)
        flags = ["--log", "--requests"] + (["--open-sheets"] if case.get("open_sheets") else [])
        run = subprocess.run(["node", GEN, "--script", spec, *flags, "--out", out], capture_output=True, text=True)
        if run.returncode != 0:
            sys.exit(f"gen_cases failed:\n{run.stderr}")
        line = open(out).read().strip()
        new = json.loads(line)
        if new["steps"] and [s["choices"] for s in new["steps"]] != script["choices"][: len(new["steps"])]:
            sys.exit("the battle did not play its own choices again")
        diff = subprocess.run([DIFFTEST, out, "--strict"], capture_output=True, text=True)
        if diff.returncode != 0 or " 0 diverged" not in diff.stdout:
            sys.exit(f"the engine does not play it as Showdown does:\n{diff.stdout[-800:]}")
        follow = subprocess.run([FOLLOWCHECK, out, "--export", "--require-state"], capture_output=True, text=True)
        verdict = "followed with no difference" if follow.returncode == 0 else "the follower differs (a regression battle)"
        sys.stderr.write(f"battle {wanted}: {len(new['steps'])} steps, {len(line) // 1000} KB; {verdict}\n")
        print(line)


if __name__ == "__main__":
    main()
