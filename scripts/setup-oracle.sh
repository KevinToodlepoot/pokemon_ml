#!/usr/bin/env bash
# Fetches and builds the pinned Pokémon Showdown commit the engine is checked against.
# Needs git and Node.js 22 or newer.
set -euo pipefail
cd "$(dirname "$0")/../oracle"

COMMIT=7b02eb39878f4b486f658a752b38c161379bd352   # 2026-10-18, vgc-bot's Showdown pin (D55, D88, D90): has 0ac1403 (Future Sight and Endure) and a7ce30c (Emergency Exit and Sheer Force)

if [ ! -d pokemon-showdown/.git ]; then
  mkdir -p pokemon-showdown
  git -C pokemon-showdown init -q
  git -C pokemon-showdown remote add origin https://github.com/smogon/pokemon-showdown.git
fi
git -C pokemon-showdown fetch -q --depth 1 origin "$COMMIT"
git -C pokemon-showdown checkout -q FETCH_HEAD
(cd pokemon-showdown && npm install --no-audit --no-fund && node build)
echo "Showdown $COMMIT built in oracle/pokemon-showdown"
