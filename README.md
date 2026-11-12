# move2sp1 — Sui transactions, proven

**[▶ Demo video (10 min)](https://www.loom.com/share/a5f2ac7350a2419e808001565b5b1919)**

**move2sp1** compiles Move — from source, or from bytecode deployed on Sui — ahead of time to
RISC-V and runs it in [SP1](https://github.com/succinctlabs/sp1), Succinct's zkVM.
**sui-replay** re-executes any Sui mainnet transaction inside SP1 and checks the result byte for
byte against the chain. Runs can be proven, and many proofs folded into one.

Why: checking a Sui execution today means re-running it on a full node. A proof lets anyone
check it. Compiling instead of interpreting means the zkVM proves the program itself.

## Results

* **572/572** user transactions of 20 consecutive mainnet checkpoints
  ([344,393,000](https://suiscan.xyz/mainnet/checkpoint/344393000)–019) byte-identical:
  every written object, event and balance change. Also 127/127, 42/42 (a month earlier), 61/61
  (May 2025).
* Mainnet transactions **proven on a laptop CPU** (M1 Max): a
  [DeepBook order](https://suiscan.xyz/mainnet/tx/AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq)
  in 26 s (596k cycles, verified in 85 ms); a
  [30-command DeFi PTB](https://suiscan.xyz/mainnet/tx/4R9QDP1EfXCYTD6MN15i1nU2absKgiDjdYDGkf3zbbTC)
  in 27 s.
* **One proof for 4 checkpoints**: 12 transactions proven, then verified inside SP1 by an
  aggregation program — a single 1.2 MiB proof, verified in 38 ms.
* DeepBook v3's own unit tests in the zkVM: **404/404**.

## What works

* Compiler: Move bytecode → LLVM → RISC-V ELF with a `no_std` Rust runtime; full Move stdlib
  and Sui framework; Sui's exact object IDs, BCS, events, dynamic fields, abort codes; 82 of 105
  framework natives, including secp256k1/r1, ed25519 and BLS12-381 on SP1 precompiles.
* Transactions as data: an image compiles the called functions; the PTB (all commands except
  `Publish`/`Upgrade`) is input to a runtime interpreter.
* Replay: exact package versions, input objects at their input versions verified against
  digests in the effects; failed transactions must fail with the same abort code and module.
* `movevm prove` / `movevm aggregate`: proofs and batch proofs on the local prover.

## What's faked or not covered

* Gas is not metered; gas charges are taken from the chain.
* Address balances are not state: withdrawals are compared with the chain, not checked
  against funds.
* The input state is checked against the effects outside the proof; read-only dynamic fields
  are not covered by digests. Checkpoint signatures are not checked.
* Not supported: `Publish`/`Upgrade`, system transactions, `Receiving` inputs, rare natives
  (Groth16, zkLogin, VRF, BLS pairings) — clean errors.
* ed25519 follows `ed25519-dalek`, not ZIP-215 (differs only on adversarial edge cases).
* Coin reservations are decoded from an encoding worked out from mainnet data.

## How to run

Needs Rust, LLVM 20 and SP1's toolchain (`sp1up`).

```sh
cargo build --release
target/release/sui-replay AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq
target/release/sui-replay --checkpoints 344393000-344393019
target/release/sui-replay --latest 20
scripts/prove-checkpoint.sh 150000003-150000006
scripts/demo.sh -i
```

Layout: `compiler/` (`move2sp1`), `runtime/` (runtime and aggregator), `runner/` (`movevm`),
`replay/` (`sui-replay`), `examples/`, `scripts/`.
