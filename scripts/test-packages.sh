#!/usr/bin/env bash
# Run Move packages' own unit tests inside SP1: `move2sp1 test` compiles every #[test] into
# its own ELF, `movevm test` executes them and checks #[expected_failure] abort codes.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build -q --release
for pkg in "${@:-examples/deepbook-v3/deepbook}"; do
  out="out/tests/$(basename "$pkg")"
  rm -rf "$out"
  echo "== $pkg"
  target/release/move2sp1 test "$pkg" -o "$out" 2>&1 | grep -E 'not compiled|test\(s\) compiled'
  target/release/movevm test "$out" | grep -vE '^stderr' | tee "$out/report.txt" | grep -E '^FAIL|passed'
done
