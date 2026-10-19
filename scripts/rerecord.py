#!/usr/bin/env python3
"""Re-records the battles of a fixture that the engine no longer plays as recorded, at the
Showdown PS_DIR names (or oracle/pokemon-showdown): after a Showdown bump changed something
they reach. Each battle is replayed with its own recorded choices (`gen_cases.js --script
--until-rejected`), so its line of play is kept up to the first choice the new Showdown no
longer accepts, where the battle now ends (`truncated`).

  scripts/rerecord.py tests/fixtures/NAME.jsonl [...]           re-record the lines that fail, in place
  scripts/rerecord.py --check tests/fixtures/NAME.jsonl [...]   only list them

A line fails if it does not pass alone: `difftest --strict` for a recording with Showdown's
state, `followcheck --export` for one with only the log and requests. A re-recorded line keeps
the fields the old one had (a follower fixture stays cut down to its log and requests), its id
and anything written beside it (`_generation`).
"""
import json
import os
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIFFTEST = os.path.join(ROOT, "target", "release", "difftest")
FOLLOWCHECK = os.path.join(ROOT, "target", "release", "followcheck")
GEN = os.path.join(ROOT, "oracle", "gen_cases.js")


def has_state(case):
    return "sides" in case["initial"]


def passes(line, work):
    path = os.path.join(work, "one.jsonl")
    with open(path, "w") as f:
        f.write(line + "\n")
    case = json.loads(line)
    if has_state(case):
        out = subprocess.run([DIFFTEST, path, "--strict"], capture_output=True, text=True)
        return out.returncode == 0 and " 0 diverged" in out.stdout
    out = subprocess.run([FOLLOWCHECK, path, "--export"], capture_output=True, text=True)
    text = out.stdout + out.stderr
    return out.returncode == 0 and " 0 not followed" in text and "compared, 0 differ" in text


def script_of(case):
    rosters = case.get("rosters") or case["teams"]
    picks = case.get("picks") or [list(range(len(t))) for t in case["teams"]]
    return {
        "id": case["id"],
        "seed": case["seed"],
        "rosters": rosters,
        "picks": picks,
        "open_sheets": case.get("open_sheets", False),
        "choices": [step["choices"] for step in case["steps"]],
    }


def like(old, new):
    """`new` cut down to the fields `old` has, at every level the recordings share."""
    out = {k: new[k] for k in old if k in new}
    for k in old:
        if k not in new:
            out[k] = old[k]  # written beside the recording (`_generation`)
    if "truncated" in new and new["truncated"]:
        out["truncated"] = True
    out["initial"] = {k: new["initial"][k] for k in old["initial"] if k in new["initial"]}
    step_keys = old["steps"][0].keys() if old["steps"] else new["steps"][0].keys() if new["steps"] else []
    after_keys = old["steps"][0]["after"].keys() if old["steps"] else []
    out["steps"] = [
        {**{k: s[k] for k in step_keys if k != "after"}, "after": {k: s["after"][k] for k in after_keys}}
        for s in new["steps"]
    ]
    return out


def rerecord(cases, work):
    """The cases recorded again, in order, with the flags their recordings were made with."""
    first = cases[0]
    flags = []
    if "log" in first["initial"]:
        flags.append("--log")
    if "requests" in first["initial"]:
        flags.append("--requests")
    if "draws" in first["initial"]:
        flags.append("--trace")
    script = os.path.join(work, "script.json")
    with open(script, "w") as f:
        json.dump([script_of(c) for c in cases], f)
    out = os.path.join(work, "out.jsonl")
    run = subprocess.run(
        ["node", GEN, "--script", script, "--until-rejected", *flags, "--out", out],
        capture_output=True, text=True,
    )
    if run.returncode != 0:
        sys.exit(f"gen_cases failed:\n{run.stderr}")
    sys.stderr.write(run.stderr)
    print("  " + run.stdout.strip().splitlines()[-1])
    return [json.loads(l) for l in open(out)]


def main():
    args = sys.argv[1:]
    check = "--check" in args
    files = [a for a in args if a != "--check"]
    with tempfile.TemporaryDirectory() as work:
        for path in files:
            lines = open(path).read().splitlines()
            if not lines or "initial" not in json.loads(lines[0]):
                print(f"{os.path.basename(path)}: not recorded battles; passed over")
                continue
            failing = [i for i, line in enumerate(lines) if not passes(line, work)]
            ids = [json.loads(lines[i])["id"] for i in failing]
            print(f"{os.path.basename(path)}: {len(failing)} of {len(lines)} battles fail alone: ids {ids}")
            if check or not failing:
                continue
            old = [json.loads(lines[i]) for i in failing]
            new = rerecord(old, work)
            for i, o, n in zip(failing, old, new):
                kept = like(o, n)
                lines[i] = json.dumps(kept, separators=(",", ":"), ensure_ascii=False)
                end = "ends at a rejected choice" if kept.get("truncated") and not o.get("truncated") else "plays out"
                print(f"  battle {o['id']} (line {i + 1}): {len(o['steps'])} steps recorded, {len(n['steps'])} now; {end}")
            still = [json.loads(lines[i])["id"] for i in failing if not passes(lines[i], work)]
            if still:
                sys.exit(f"  still failing after re-recording: ids {still}")
            with open(path, "w") as f:
                f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
