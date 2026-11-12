#!/usr/bin/env bash
# The demo, step by step.
#
# usage: scripts/demo.sh [-i]
#   -i   interactive: pause before each step until a key is pressed (for recording)
# env: PROVE=0 skips proving; CHECKPOINTS=A-B picks the checkpoints of step 7.
set -euo pipefail
cd "$(dirname "$0")/.."

interactive=0
while getopts "i" opt; do
  case $opt in
    i) interactive=1 ;;
    *) echo "usage: $0 [-i]" >&2; exit 2 ;;
  esac
done

step() {
  printf '\n\033[1;36m== %s\033[0m\n' "$*"
  if [[ $interactive == 1 ]]; then
    read -r -s -n 1 -p $'\033[2m(press any key)\033[0m' _ </dev/tty || true
    printf '\r\033[K'
  fi
}
run() { printf '\033[2m$ %s\033[0m\n' "$*"; "$@"; }

mkdir -p out

step "1. Compile Move source to a RISC-V ELF and run it in SP1"
run target/release/move2sp1 build examples/fib.move -a demo=0x100 -e fib -o out/fib.elf
run target/release/movevm run out/fib.elf 30

step "2. DeepBook v3's own unit tests, compiled and run in the zkVM"
run scripts/test-packages.sh 2>&1 | tail -3

step "3. Replay a mainnet DeepBook v3 market order (bytecode from chain, objects at input versions)"
run target/release/sui-replay AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq

step "4. A transaction that FAILED on mainnet fails the same way in the replay"
run target/release/sui-replay GpL3CbLNzdNjq7MPeyJfhrMeEhmBzdQaYUxpjjmnUQpz | tail -4

step "5. A 30-command PTB with coin splits/merges and an address-balance withdrawal"
run target/release/sui-replay 4R9QDP1EfXCYTD6MN15i1nU2absKgiDjdYDGkf3zbbTC | tail -8

if [[ ${PROVE:-1} == 1 ]]; then
  step "6. Prove the DeepBook replay (local CPU prover) and verify the proof"
  d=out/replay/AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq
  run target/release/movevm prove $d/image.elf "$(cat $d/ctx.json)" --ptb $d/ptb.json --state-in $d/state_in.bin
fi

step "7. Every user transaction of 4 consecutive mainnet checkpoints (~1 s of Sui), 8 in parallel"
run scripts/replay-sample.py --checkpoint "${CHECKPOINTS:-344393009-344393012}" --jobs 8

step "8. Random recent mainnet transactions"
run scripts/replay-sample.py 20 --jobs 8
