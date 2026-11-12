#!/usr/bin/env bash
# Prove replayed mainnet transactions: for each digest, `sui-replay` (which checks the result
# against chain and leaves the program + inputs in out/replay/<digest>/), then `movevm prove`
# on exactly those inputs (local CPU prover; core proof, or compressed with COMPRESSED=1).
# Appends one CSV row per transaction to out/bench/replay.csv.
#
# usage: scripts/bench-replay.sh DIGEST...
set -euo pipefail
cd "$(dirname "$0")/.."

mkdir -p out/bench
csv=out/bench/replay.csv
[[ -f $csv ]] || echo "date,digest,commands,cycles,prove_s,verify_ms,proof_kib,mode,machine" >"$csv"
machine="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m) ($(sysctl -n hw.ncpu 2>/dev/null || nproc) cores)"
mode=core
flag=()
if [[ ${COMPRESSED:-0} == 1 ]]; then mode=compressed; flag=(--compressed); fi

for d in "$@"; do
  out=$(target/release/sui-replay "$d" 2>&1) || { echo "$d: replay failed"; echo "$out" | tail -3; continue; }
  echo "$out" | grep -q "^MATCH" || { echo "$d: no match"; continue; }
  commands=$(echo "$out" | sed -n 's/^  commands: //p' | head -1)
  dir=out/replay/$d
  p=$(target/release/movevm prove "$dir/image.elf" "$(cat "$dir/ctx.json")" \
      --ptb "$dir/ptb.json" --state-in "$dir/state_in.bin" ${flag[@]+"${flag[@]}"} 2>&1)
  echo "$p" | tail -3
  cycles=$(echo "$p" | sed -n 's/^proving .*(\([0-9]*\) cycles.*/\1/p')
  prove=$(echo "$p" | sed -nE 's/.*prove ([0-9.]+)(ms|s) .*/\1 \2/p' | awk '{print ($2=="ms") ? $1/1000 : $1}')
  verify=$(echo "$p" | sed -nE 's/.*verify ([0-9.]+)(ms|s);.*/\1 \2/p' | awk '{print ($2=="s") ? $1*1000 : $1}')
  kib=$(echo "$p" | sed -n 's/.*proof \([0-9]*\) KiB.*/\1/p')
  echo "$(date -u +%FT%TZ),$d,\"$commands\",$cycles,$prove,$verify,$kib,$mode,\"$machine\"" >>"$csv"
done
column -s, -t <"$csv" | tail -n +1
