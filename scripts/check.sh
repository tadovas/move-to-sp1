#!/usr/bin/env bash
# End-to-end checks: compile each example entry with move2sp1, run it in the SP1 executor
# with movevm and compare the printed result.
set -euo pipefail
cd "$(dirname "$0")/.."

(cd compiler && cargo build -q) && (cd runner && cargo build -q)
M2S=compiler/target/debug/move2sp1
VM=runner/target/debug/movevm
mkdir -p out

pass=0 fail=0
# file | entry | args | expected result ("abort:<text>", or "=model" to ask the
# reference model scripts/<name>_model.py for the expected result)
while IFS='|' read -r file entry args expected; do
  [[ -z "$file" || "$file" == \#* ]] && continue
  if [[ "$expected" == "=model" ]]; then
    # shellcheck disable=SC2086
    expected=$("scripts/${file%.move}_model.py" $args)
  fi
  elf="out/${entry}.elf"
  if ! "$M2S" build "examples/$file" -a demo=0x100 -e "$entry" -o "$elf" 2>/tmp/m2s.err; then
    echo "FAIL $entry: compile error"; sed 's/^/    /' /tmp/m2s.err | tail -5; fail=$((fail+1)); continue
  fi
  # shellcheck disable=SC2086
  out=$("$VM" run --compact "$elf" $args 2>&1 || true)
  if [[ "$expected" == abort:* ]]; then
    grep -q "${expected#abort:}" <<<"$out" && ok=1 || ok=0
  else
    # compare as JSON values (formatting/key order independent)
    actual=$(sed -n 's/^[^ ]*(.*) = //p' <<<"$out" | head -1)
    python3 -c 'import json,sys; sys.exit(json.loads(sys.argv[1]) != json.loads(sys.argv[2]))' \
      "$actual" "$expected" 2>/dev/null && ok=1 || ok=0
  fi
  if (( ok )); then
    echo "ok   $entry($args) -> $expected   [$(grep -o 'cycles: [0-9]*' <<<"$out" || echo aborted)]"; pass=$((pass+1))
  else
    echo "FAIL $entry($args): expected $expected"; sed 's/^/    /' <<<"$out" | tail -3; fail=$((fail+1))
  fi
done <<'CASES'
add.move|main|40 2|42
add.move|main|18446744073709551615 1|abort:ARITHMETIC_ERROR
fib.move|main|92|7540113804746346429
fib.move|main|93|abort:ARITHMETIC_ERROR
types.move|test_point||70
types.move|test_vec|100|4950
types.move|test_generic||201
types.move|test_copy||34
types.move|test_eq||11
types.move|test_sort|50|1
types.move|test_pop||206
types.move|test_oob||abort:VECTOR_OPERATION_ERROR
types.move|test_std||754329
enums.move|test_shapes||24
enums.move|test_widen||2412
enums.move|test_generic||512
enums.move|test_eq||111
io.move|scale|{"x":3,"y":4} 10|{"x":30,"y":40}
io.move|sum_points|[{"x":1,"y":2},{"x":10,"y":20}]|{"x":11,"y":22}
io.move|grow|{"Rect":{"w":3,"h":4}}|{"Rect":{"w":4,"h":5}}
io.move|grow|"Empty"|"Empty"
io.move|greet|move|"hello, move"
io.move|digests|abc|["0xba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","0x3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"]
io.move|encode|{"x":1,"y":2}|"0x01000000000000000200000000000000"
amm.move|apply|{"reserve_a":1000000,"reserve_b":500000,"lp_supply":707106,"fee_bps":30} [{"SwapAForB":{"amount_in":10000,"min_out":4900}},{"SwapBForA":{"amount_in":2500,"min_out":0}},{"AddLiquidity":{"amount_a":50000,"amount_b":30000}},{"RemoveLiquidity":{"lp":100000}}]|=model
amm.move|apply|{"reserve_a":1000000,"reserve_b":500000,"lp_supply":707106,"fee_bps":30} [{"SwapAForB":{"amount_in":10000,"min_out":5000}}]|=model
amm.move|apply|{"reserve_a":0,"reserve_b":0,"lp_supply":0,"fee_bps":5} [{"AddLiquidity":{"amount_a":4000000,"amount_b":9000000}},{"SwapBForA":{"amount_in":90000,"min_out":39000}}]|=model
CASES

echo "$pass passed, $fail failed"
(( fail == 0 ))
