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
# file | entry | args | expected result (or "abort:<text>")
while IFS='|' read -r file entry args expected; do
  [[ -z "$file" || "$file" == \#* ]] && continue
  elf="out/${entry}.elf"
  if ! "$M2S" build "examples/$file" -a demo=0x2 -e "$entry" -o "$elf" 2>/tmp/m2s.err; then
    echo "FAIL $entry: compile error"; sed 's/^/    /' /tmp/m2s.err | tail -5; fail=$((fail+1)); continue
  fi
  # shellcheck disable=SC2086
  out=$("$VM" run "$elf" $args 2>&1 || true)
  if [[ "$expected" == abort:* ]]; then
    grep -q "${expected#abort:}" <<<"$out" && ok=1 || ok=0
  else
    grep -q "= ${expected}\$" <<<"$out" && ok=1 || ok=0
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
CASES

echo "$pass passed, $fail failed"
(( fail == 0 ))
