#!/usr/bin/env bash
# DeepBook v3 as a chain of real transactions, one SP1 run each, linked only by state
# snapshots (examples/deepbook-v3-chain, production code only):
#   1 publish      every module's `init` (Registry, admin cap, coin treasuries)
#   2 create_pool  admin creates a whitelisted BASE/QUOTE pool
#   3 make         maker posts asks 1.00/1.01/1.02 and bids 0.99/0.98
#   4 take         taker market-buys 4_000_000 BASE
# Checks: each run's input-state hash is the previous run's output-state hash, the final
# outcome, and that an owned object cannot be used by another sender.
set -euo pipefail
cd "$(dirname "$0")/.."
PROFILE=${PROFILE:-debug}
[[ $PROFILE == release ]] && REL=--release || REL=
(cd compiler && cargo build -q $REL) && (cd runner && cargo build -q $REL)
M2S=compiler/target/$PROFILE/move2sp1
VM=runner/target/$PROFILE/movevm
PKG=examples/deepbook-v3-chain
OUT=out/chain
mkdir -p $OUT && rm -f $OUT/*.bcs

ctx() { echo "{\"sender\":\"$2\",\"tx_hash\":\"0x$(printf '%064x' "$1")\",\"epoch\":1,\"epoch_timestamp_ms\":1700000000000,\"ids_created\":0}"; }

"$M2S" build $PKG --publish -o $OUT/publish.elf 2>/dev/null
for e in create_pool make take; do "$M2S" build $PKG -e $e -o $OUT/$e.elf 2>/dev/null; done

prev_out=""
step() { # name elf state_in state_out args...
  local name=$1 elf=$2 sin=$3 sout=$4; shift 4
  local log
  log=$("$VM" run --compact "$elf" "$@" ${sin:+--state-in "$sin"} --state-out "$sout")
  local line in out cycles
  line=$(grep '^state:' <<<"$log")
  in=$(awk '{print $3}' <<<"$line"); out=$(awk '{print $6}' <<<"$line")
  cycles=$(grep -o 'cycles: [0-9]*' <<<"$log" | head -1)
  if [[ -n "$prev_out" && "$in" != "$prev_out" ]]; then echo "FAIL $name: input state $in != previous output $prev_out"; exit 1; fi
  printf "%-12s state %s -> %s   [%s]\n" "$name" "$in" "$out" "$cycles"
  prev_out=$out
  LAST_LOG=$log
}

step publish     $OUT/publish.elf     ""          $OUT/s1.bcs "$(ctx 1 0xa11ce)"
step create_pool $OUT/create_pool.elf $OUT/s1.bcs $OUT/s2.bcs _ _ "$(ctx 2 0xa11ce)"
step make        $OUT/make.elf        $OUT/s2.bcs $OUT/s3.bcs _ _ _ clock "$(ctx 3 0xa11ce)"
step take        $OUT/take.elf        $OUT/s3.bcs $OUT/s4.bcs _ _ clock "$(ctx 4 0xa11ce)"

expected='{"taker_base_bought":4000000,"taker_quote_paid":4020000,"best_bid":990000000,"best_ask":1010000000,"mid_price":1000000000}'
actual=$(sed -n 's/^[^ ]*(.*) = //p' <<<"$LAST_LOG")
python3 -c 'import json,sys; sys.exit(json.loads(sys.argv[1]) != json.loads(sys.argv[2]))' "$actual" "$expected" \
  || { echo "FAIL take outcome: $actual"; exit 1; }
echo "ok   take outcome: $actual"

# owned objects are usable only by their owner
denied=$("$VM" run --compact $OUT/take.elf _ _ clock "$(ctx 5 0xb0b)" --state-in $OUT/s3.bcs 2>&1 || true)
if grep -q 'aborted with code 4 in move2sp1::state' <<<"$denied"; then
  echo "ok   another sender cannot use 0xa11ce's TreasuryCap (E_NOT_OWNER)"
else
  echo "FAIL ownership not enforced"; exit 1
fi
echo "chain ok"
