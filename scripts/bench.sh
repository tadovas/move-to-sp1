#!/usr/bin/env bash
# Cycle benchmark: the AMM state transition over growing batches of actions.
# Every result is checked against the reference model; cycles are appended to
# bench/history.csv (commit, case, cycles) so improvements can be compared over time.
set -euo pipefail
cd "$(dirname "$0")/.."

(cd compiler && cargo build -q --release) && (cd runner && cargo build -q --release)
M2S=compiler/target/release/move2sp1
VM=runner/target/release/movevm
mkdir -p out bench
"$M2S" build examples/amm.move -a demo=0x100 -e apply -o out/amm.elf 2>/dev/null

commit=$(git rev-parse --short HEAD)$(git diff --quiet HEAD -- . ':!bench' || echo "+dirty")
[[ -f bench/history.csv ]] || echo "commit,case,cycles" > bench/history.csv

printf "%-14s %12s %16s\n" "actions" "cycles" "cycles/action"
prev_n=0 prev_c=0
for n in 1 10 100 1000; do
  read -r pool actions < <(scripts/amm_model.py --gen "$n")
  out=$("$VM" run --compact out/amm.elf "$pool" "$actions")
  actual=$(sed -n 's/^[^ ]*(.*) = //p' <<<"$out")
  expected=$(scripts/amm_model.py "$pool" "$actions")
  python3 -c 'import json,sys; sys.exit(json.loads(sys.argv[1]) != json.loads(sys.argv[2]))' "$actual" "$expected" \
    || { echo "MISMATCH vs reference model at n=$n"; exit 1; }
  cycles=$(grep -o 'cycles: [0-9]*' <<<"$out" | grep -o '[0-9]*')
  marginal=$(( prev_n == 0 ? 0 : (cycles - prev_c) / (n - prev_n) ))
  printf "%-14s %12s %16s\n" "amm x$n" "$cycles" "$([[ $prev_n == 0 ]] && echo - || echo "$marginal")"
  echo "$commit,amm_x$n,$cycles" >> bench/history.csv
  prev_n=$n prev_c=$cycles
done
echo "(all results match scripts/amm_model.py; appended to bench/history.csv)"
