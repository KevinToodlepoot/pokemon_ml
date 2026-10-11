#!/usr/bin/env python3
"""Compare fresh Showdown battles and preserve every failure with its provenance."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import tarfile
import threading
import time

REPO = Path(__file__).resolve().parents[1]
THEMES = [
    ('types', ['--moves', 'reflecttype,soak,trickortreat,forestscurse,burnup,doubleshock,roost', '--moves-per', '3']),
    ('pressure', ['--abilities', 'pressure,innerfocus', '--moves', 'encore,expandingforce,curse,comeuppance,metalburst']),
    ('choice-swap', ['--abilities', 'klutz,innerfocus,pressure', '--items', 'choicescarf,mentalherb,leftovers', '--moves', 'skillswap,encore,magicroom,trick,switcheroo,instruct']),
    ('mental-herb', ['--abilities', 'innerfocus,pressure,klutz', '--items', 'mentalherb,choicescarf,leftovers', '--moves', 'encore,taunt,disable,torment,throatchop,imprison', '--moves-per', '3']),
    ('item-control', ['--moves', 'fling,thief,covet,trick,switcheroo,knockoff', '--moves-per', '3']),
    ('trapping', ['--moves', 'octolock,bind,wrap,uturn,voltswitch,quash,afteryou,round', '--moves-per', '3', '--items', 'ejectbutton,redcard,bindingband,leftovers']),
    ('airborne', ['--moves', 'gravity,smackdown,fly,bounce,magnetrise,skyattack,meteorbeam,phantomforce,dive', '--moves-per', '3']),
    ('speed-order', ['--moves', 'speedswap,trickroom,tailwind,quash,afteryou,round,allyswitch', '--moves-per', '3', '--items', 'choicescarf,quickclaw,ironball,leftovers']),
    ('ability-control', ['--moves', 'skillswap,gastroacid,roleplay,entrainment,worryseed,simplebeam', '--moves-per', '3']),
    ('called-moves', ['--moves', 'instruct,copycat,sleeptalk,encore,transform,uproar', '--moves-per', '3', '--abilities', 'pressure,innerfocus,prankster']),
]


# New uploaded failures plus earlier observation regressions. With 50-battle
# batches, 1,000 battles exercises each theme in both sheet modes.
REGRESSION_THEMES = [
    ('illusion-form', ['--species', 'zoroark,zoroarkhisui,aegislash', '--moves', 'uturn,kingsshield,willowisp,toxic,bounce,ironhead', '--moves-per', '3']),
    ('illusion-items', ['--species', 'zoroark,zoroarkhisui,aegislash', '--moves', 'trick,switcheroo,uturn,protect,scaryface', '--moves-per', '3', '--items', 'choicescarf,ironball,focusband,leftovers']),
    ('residual-weather', ['--species', 'politoed,pelipper,passimian', '--abilities', 'receiver,drizzle,drought,innerfocus', '--moves', 'toxic,willowisp,protect,raindance,sunnyday', '--moves-per', '3']),
    ('broken-guards', ['--species', 'sirfetchd,golisopod,arcanine', '--abilities', 'emergencyexit,innerfocus', '--moves', 'feint,quickguard,wideguard,protect,uturn', '--moves-per', '3', '--items', 'ejectbutton,redcard,leftovers']),
    *[theme for theme in THEMES if theme[0] in {'types', 'pressure', 'choice-swap', 'mental-herb', 'trapping', 'called-moves'}],
]


def write_json(path, value):
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def git(*args, cwd=REPO):
    return subprocess.check_output(['git', *args], cwd=cwd, text=True).strip()


def command(args, output):
    with output.open('w') as stream:
        result = subprocess.run([str(a) for a in args], cwd=REPO, stdout=stream, stderr=subprocess.STDOUT)
    return result.returncode


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--battles', type=int, default=50000, help='complete battles to retain and compare')
    parser.add_argument('--seed', type=int, required=True, help='base generator seed (use a new one for each run)')
    parser.add_argument('--out', type=Path, required=True, help='new results directory; refuses to overwrite an existing run')
    parser.add_argument('--batch-size', type=int, default=1000)
    parser.add_argument('--workers', type=int, default=min(4, os.cpu_count() or 1))
    parser.add_argument('--profile', choices=['mixed', 'general', 'regressions'], default='mixed', help='mixed uses general and targeted stress battles; regressions targets retained observation failures')
    parser.add_argument('--keep-passing', action='store_true', help='keep complete passing recordings too (large)')
    parser.add_argument('--no-build', action='store_true', help='use existing release binaries; their hashes are always recorded')
    args = parser.parse_args()
    if min(args.battles, args.batch_size, args.workers) < 1 or not 0 < args.seed < 2**32:
        parser.error('counts must be positive and seed must be between 1 and 4294967295')
    out = args.out.resolve()
    if out.exists():
        parser.error(f'{out} already exists; choose a new directory')
    if not (REPO / 'oracle/pokemon-showdown/dist/sim/index.js').exists():
        parser.error('run scripts/setup-oracle.sh first')
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--locked', '--release', '--bin', 'followcheck', '--bin', 'difftest'], cwd=REPO, check=True)
    out.mkdir(parents=True)
    (out / 'bin').mkdir()
    (out / 'failures').mkdir()
    (out / 'batches').mkdir()
    binary_hashes = {}
    suffix = '.exe' if os.name == 'nt' else ''
    for name in ['followcheck', 'difftest']:
        src = REPO / 'target/release' / (name + suffix)
        dest = out / 'bin' / (name + suffix)
        shutil.copy2(src, dest)
        binary_hashes[name] = hashlib.sha256(dest.read_bytes()).hexdigest()
    oracle_sources = ['oracle/gen_cases.js', 'oracle/lib.js', 'oracle/pool.json']
    provenance = dict(schema_version=1, started_utc=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
        engine_commit=git('rev-parse', 'HEAD'), working_tree=git('-c', 'core.fsmonitor=false', 'status', '--porcelain'),
        showdown_commit=git('rev-parse', 'HEAD', cwd=REPO / 'oracle/pokemon-showdown'),
        showdown_working_tree=git('-c', 'core.fsmonitor=false', 'status', '--porcelain', cwd=REPO / 'oracle/pokemon-showdown'),
        node_version=subprocess.check_output(['node', '--version'], text=True).strip(),
        generator_hashes={p: hashlib.sha256((REPO / p).read_bytes()).hexdigest() for p in oracle_sources},
        binaries=binary_hashes, build_requested=not args.no_build, command=sys.argv, scope='complete stress battles; single initially brought Illusion per side; closed sheets exclude rosters with non-native Illusion')
    write_json(out / 'provenance.json', provenance)
    (out / 'working-tree.patch').write_text(git('diff', 'HEAD'))
    pool = json.loads((REPO / 'oracle/pool.json').read_text())
    native_illusion = {s['id'] for s in pool['species'] if 'illusion' in s['abilities']}
    tasks = []
    left, targeted = args.battles, 0
    while left:
        i = len(tasks)
        options = []
        label = 'general'
        if args.profile == 'mixed' and i % 5 >= 3:
            label, options = THEMES[(targeted // 2) % len(THEMES)]
            options = list(options)
            targeted += 1
        if args.profile == 'regressions':
            label, options = REGRESSION_THEMES[i % len(REGRESSION_THEMES)]
            options = list(options)
            open_sheets = (i // len(REGRESSION_THEMES)) % 2 == 1
        else:
            open_sheets = i % 2 == 1
        if open_sheets:
            options += ['--open-sheets']
        # Space batch seeds apart so adjacent base seeds do not reuse whole batches.
        tasks.append(dict(label=f'{i + 1:04d}-{label}', seed=(args.seed + i * 1000003) % (2**32 - 1) or 1,
                          n=min(left, args.batch_size), options=options))
        left -= tasks[-1]['n']
    write_json(out / 'manifest.json', tasks)
    lock, results, findings = threading.Lock(), [], []
    start = time.monotonic()

    def publish(row, discovered):
        with lock:
            results.append(row)
            findings.extend(discovered)
            totals = {}
            for result in results:
                for category in ['observations', 'mechanics']:
                    for key, value in result[category].items():
                        totals[category + '_' + key] = totals.get(category + '_' + key, 0) + value
            summary = dict(completed_batches=len(results), compared_battles=sum(r['accepted'] for r in results),
                           failing_battles=len(findings), elapsed_seconds=round(time.monotonic() - start), **totals)
            write_json(out / 'summary.json', summary)
            write_json(out / 'failures.json', sorted(findings, key=lambda f: f['key']))
            write_json(out / 'results.json', sorted(results, key=lambda r: r['batch']))
            print(json.dumps(dict(batch=row['batch'], accepted=row['accepted'], failures=len(discovered), totals=summary)), flush=True)

    def run(task):
        directory = out / 'batches' / task['label']
        directory.mkdir()
        selected = directory / 'cases.jsonl'
        accepted, generated, part = 0, 0, 0
        excluded = {'truncated': 0, 'multiple_illusion': 0, 'closed_injected_illusion': 0, 'surplus': 0}
        with selected.open('w') as destination:
            while accepted < task['n']:
                if part >= 100:
                    raise RuntimeError(f"{task['label']}: could not fill batch after 100 attempts")
                seed = (task['seed'] + part * 1000000007) % (2**32 - 1) or 1
                raw = directory / 'generated.jsonl'
                gen = ['node', 'oracle/gen_cases.js', '--n', str(task['n'] - accepted + 10), '--seed', str(seed),
                       '--log', '--requests', '--out', str(raw), *task['options']]
                if command(gen, directory / f'generator-{part}.txt'):
                    raise RuntimeError(f'generator failed; inspect {directory}')
                for line in raw.open():
                    case = json.loads(line)
                    generated += 1
                    if case.get('truncated'):
                        excluded['truncated'] += 1
                        continue
                    illusion = [[mon for mon in team if mon['ability'] == 'illusion'] for team in case['teams']]
                    if any(len(team) > 1 for team in illusion):
                        excluded['multiple_illusion'] += 1
                        continue
                    if not case.get('open_sheets') and any(m['ability'] == 'illusion' and m['species'] not in native_illusion for team in case['rosters'] for m in team):
                        excluded['closed_injected_illusion'] += 1
                        continue
                    if accepted >= task['n']:
                        excluded['surplus'] += 1
                        continue
                    case['_generation'] = dict(seed=seed, original_id=case['id'], options=task['options'])
                    accepted += 1
                    case['id'] = accepted
                    destination.write(json.dumps(case, separators=(',', ':')) + '\n')
                raw.unlink()
                part += 1
        follow_report, mechanics_report = directory / 'follow.json', directory / 'mechanics.json'
        follow_code = command([out / 'bin' / ('followcheck' + suffix), selected, '--list', '--report', follow_report], directory / 'follow.txt')
        mechanics_code = command([out / 'bin' / ('difftest' + suffix), selected, '--by-hand', '--report', mechanics_report], directory / 'mechanics.txt')
        if follow_code not in (0, 1) or mechanics_code not in (0, 1) or not follow_report.exists() or not mechanics_report.exists():
            raise RuntimeError(f'comparison crashed; recordings retained in {directory}')
        follow = json.loads(follow_report.read_text())
        mechanics = json.loads(mechanics_report.read_text())
        fs = {c['id']: c for c in follow['cases']}
        ms = {c['id']: c for c in mechanics['cases']}
        discovered = []
        for line in selected.open():
            case = json.loads(line)
            ident = case['id']
            f, m = fs[ident], ms[ident]
            # Keep unsupported cases as distinct findings, never silently call them passes.
            if f['differences'] == 0 and f['status'] == 'followed' and m['status'] == 'matched':
                continue
            key = f"{task['label']}-battle-{ident}"
            failure = out / 'failures' / key
            failure.mkdir()
            replay = failure / 'case.jsonl'
            replay.write_text(line)
            generation = case['_generation']
            record = dict(key=key, generation=generation, battle_seed=case['seed'],
                          open_sheets=case.get('open_sheets', False), observations=f, mechanics=m)
            write_json(failure / 'metadata.json', record)
            regenerate = ['node', 'oracle/gen_cases.js', '--seed', str(generation['seed']),
                          '--only', str(generation['original_id']), '--log', '--requests',
                          '--out', '/tmp/showdown-reproduction.jsonl', *generation['options']]
            (failure / 'README.md').write_text(
                f"# {key}\n\nObservation status: {f['status']}; differing observations: {f['differences']}. "
                f"Mechanics status: {m['status']}. Root cause awaits inspection.\n\n"
                "The complete recording can be replayed without regenerating it. From the repository:\n\n"
                "```bash\ncargo build --locked --release --bin followcheck --bin difftest\n"
                "target/release/followcheck /path/to/this/case.jsonl --list\n"
                "target/release/difftest /path/to/this/case.jsonl --by-hand\n```\n\n"
                "Regenerate with the pinned versions in provenance.json:\n\n```bash\n"
                + shlex.join(regenerate) + "\n```\n\n"
                f"Saved recording ID: {ident}; original generator ID: {generation['original_id']}.\n")
            command([out / 'bin' / ('followcheck' + suffix), replay, '--battle', str(ident)], failure / 'follow.txt')
            command([out / 'bin' / ('difftest' + suffix), replay, '--by-hand'], failure / 'mechanics.txt')
            discovered.append(record)
        row = dict(batch=task['label'], accepted=accepted, generated=generated, excluded=excluded,
                   observations=follow['summary'], mechanics=mechanics['summary'])
        write_json(directory / 'result.json', row)
        publish(row, discovered)
        if not args.keep_passing:
            selected.unlink()
        return row

    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
            for _ in executor.map(run, tasks):
                pass
    except Exception as exc:
        print(f'Run incomplete: {exc}. Completed results and current recordings are retained at {out}', file=sys.stderr)
        return 2
    provenance['completed_utc'] = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    provenance['generator_unchanged'] = all(hashlib.sha256((REPO / p).read_bytes()).hexdigest() == h for p, h in provenance['generator_hashes'].items())
    write_json(out / 'provenance.json', provenance)
    bundle = out / 'differences.tar.gz'
    with tarfile.open(bundle, 'w:gz') as archive:
        for name in ['provenance.json', 'working-tree.patch', 'manifest.json', 'summary.json', 'results.json', 'failures.json', 'failures']:
            archive.add(out / name, arcname=name)
    print(f'Results: {out}\nSend differences.tar.gz for diagnosis ({len(findings)} failing/unsupported battles).', flush=True)
    return 1 if findings else 0


if __name__ == '__main__':
    sys.exit(main())
