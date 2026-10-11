#!/usr/bin/env python3
"""Run 200,000 fresh reference battles and retain differences for diagnosis."""
import argparse
from pathlib import Path
import secrets
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seed', type=int, help='reproducible base seed; omitted selects a random new seed')
    parser.add_argument('--workers', type=int, default=4)
    parser.add_argument('--out', type=Path, help='new directory; default ~/showdown-runs/showdown-SEED')
    args = parser.parse_args()
    seed = args.seed if args.seed is not None else secrets.randbelow(2**32 - 1) + 1
    if not 0 < seed < 2**32 or args.workers < 1:
        parser.error('seed must be 1..4294967295 and workers must be positive')
    out = args.out or Path.home() / 'showdown-runs' / f'showdown-{seed}'
    print(f'New run: 200,000 battles, base seed {seed}, results {out.resolve()}', flush=True)
    return subprocess.call([
        sys.executable, str(Path(__file__).with_name('compare-showdown.py')),
        '--battles', '200000', '--seed', str(seed), '--workers', str(args.workers), '--out', str(out),
    ])


if __name__ == '__main__':
    sys.exit(main())
