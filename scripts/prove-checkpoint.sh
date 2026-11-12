#!/usr/bin/env bash
# One proof for whole Sui checkpoints: replay every user transaction of the checkpoint range
# (checked byte for byte against chain), prove each with a compressed proof, and aggregate
# all of them into a single proof (`movevm aggregate`). Keeps the machine awake (caffeinate).
#
# usage: scripts/prove-checkpoint.sh SEQ[-SEQ]
set -euo pipefail
cd "$(dirname "$0")/.."
range=${1:?usage: $0 SEQ[-SEQ]}

python3 scripts/replay-sample.py --checkpoint "$range" --jobs 8 | grep -v '^MATCH  '
dirs=$(python3 -c "
import json
r = json.load(open('out/replay/sample.json'))
bad = [x['digest'] for x in r if x['verdict'] != 'MATCH']
if bad: raise SystemExit('not every transaction replayed: ' + ' '.join(bad))
print(' '.join('out/replay/' + x['digest'] for x in r))")
mkdir -p out/bench
caffeinate -i target/release/movevm aggregate --label "sui:mainnet:checkpoints:$range" $dirs \
  | tee "out/bench/aggregate-$range.txt"
