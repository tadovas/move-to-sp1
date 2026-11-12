# move2sp1 — prove Sui transactions in a zkVM

**move2sp1** compiles Move — from source, or the bytecode of any package deployed on Sui —
ahead of time to RISC-V, and runs it in [SP1](https://github.com/succinctlabs/sp1), Succinct's
zkVM. **sui-replay** uses it to take *any* Sui mainnet transaction by its digest, re-execute
it inside SP1, and check the result byte for byte against what the chain recorded. The same
run can be proven: a succinct proof that this transaction, on this input state, produces
these objects and events.

```
$ sui-replay AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq      # a DeepBook v3 market order
image: 2 function instance(s) from 8 package(s) (122 modules) -> 461 KiB ELF in 0.8s
  0x2c8d::balance_manager::generate_proof_as_owner
  0x2c8d::pool::place_market_order<0x5145::ns::NS,0xdba3::usdc::USDC>
state: 12 object(s) read or written (from effects), 0 created; 10 verified against digests in the effects (2 only read: the effects record their version, not a digest)
discovery: 2 run(s), 12 object(s) in the final input state
executed in SP1: 587953 cycles (33ms)
objects written (chain vs replay):
  ok   0x0bff98…13bc  227 bytes, 0x2::dynamic_field::Field<0x2::object::ID,0x2c8d::account::Account>
  ok   0x9cc3c9…fe8f  3968 bytes, 0x2::dynamic_field::Field<u64,0x2c8d::big_vector::Slice<0x2c8d::order::Order>>
  ...  (9 objects)
events (chain vs replay):
  ok   0xc1a5::ewma::EWMAUpdate (64 bytes)
  ok   0x2c8d::order_info::OrderFilled (195 bytes)
  ...  (4 events)
address balances (net change, chain vs replay; gas part from chain):
  ok   0x1eeff9…6363 0x2::balance::Balance<0x2::sui::SUI>: -107102 vs -107102
MATCH: every written object and event is byte-identical to mainnet
```

## Why

Sui executes Move in a VM on validators; nobody can check a Sui execution without re-running
it on a full node. Proving Move execution in a zkVM makes it checkable by anyone — by a light
client, another chain, or a rollup that wants Sui's programming model. Interpreting Move inside
a zkVM is possible but costly (an interpreter loop per instruction, proven). move2sp1 instead
**compiles** Move to native RISC-V, so the zkVM proves the program itself.

## What works

* **A Move compiler to SP1** (`compiler/`, binary `move2sp1`): Move bytecode → LLVM IR →
  riscv64 → our own ELF linker, with a `no_std` Rust runtime (`runtime/`). Structs, enums,
  generics (monomorphised), references, vectors, abilities, aborts with Sui's exact abort codes
  and locations, the full Move standard library and Sui framework.
  * Input: Move **source** (Sui's own front end, `Move.toml` packages, embedded frameworks), or
    **bytecode** fetched from chain — including upgraded packages (linkage tables, type origins).
  * Sui fidelity: object IDs, dynamic-field IDs, type tags, BCS, events, transfers, shared and
    owned objects, dynamic fields, `TxContext` (sender, sponsor, epoch, gas price, fresh IDs),
    `Clock`, address balances (funds accumulator), crypto natives (secp256k1, secp256r1,
    ed25519, HMAC, Blake2b, Keccak, SHA-2/3).
* **Unit tests in the zkVM**: `move2sp1 test` + `movevm test` run DeepBook v3's own test suite —
  **404/404 pass**, with the exact expected abort codes and locations.
* **Transactions as data** (program *images*): an image compiles a set of functions plus a
  dispatch table; the transaction (a programmable transaction block, PTB) is *input* read by a
  PTB interpreter in the runtime. One image (one verifying key) serves every transaction that
  calls the same functions. All PTB commands except `Publish`/`Upgrade`: `MoveCall`,
  `TransferObjects`, `SplitCoins`, `MergeCoins`, `MakeMoveVec`, gas coins (smashing, sponsors,
  address-balance gas reservations).
* **State in, state out**: a run reads a canonical state snapshot and commits
  `sha256(input state) || output state` to its public values, so proofs of consecutive
  transactions chain.
* **sui-replay** (`replay/`): any mainnet transaction by digest — fetches it and its certified
  effects over Sui's public GraphQL API, the exact package versions it called, its input objects
  at their input versions, compiles an image, executes it in SP1, and compares **every written
  object (BCS), every event (BCS), every address-balance change** with the chain — or, for a
  transaction that failed on chain, that it fails with the same abort code in the same module.
  Input objects and the chain's outputs are verified against the digests certified in the
  effects.
* **Proving**: `movevm prove` proves and verifies any run on the local CPU prover.

### Results

* **199 of 200** random recent mainnet transactions replayed byte-identically on the first run
  of the final sampler (`scripts/replay-sample.py 200`, 66 s with 8 in parallel; the one
  failure, a type added in a later package version, is fixed). Earlier samples found and fixed:
  a GraphQL quirk returning the *latest* version of an upgraded package (we ran DeepBook v9 for
  a v2 call), gas-coin semantics of address-balance gas (the gas coin takes the first new object
  ID), sponsored transactions, framework versions (always the epoch's, not the linkage's), and
  missing natives. Transactions from a year ago (May 2025) replay too.
* DeepBook v3 unit tests: 404/404 (`scripts/test-packages.sh`).
* Proving: **TBD** (`scripts/bench-replay.sh`).
* Natives: **67 of 102** Sui framework natives implemented (`scripts/natives-coverage.py`); the
  rest are mostly heavy cryptography (BLS12-381, Groth16, group operations, Poseidon, VRF, VDF,
  zkLogin, Nitro attestation) and stop with a clean "unsupported native" error at compile time.

## What's faked, mocked, or not covered

Honest list — none of these affect the byte-for-byte comparisons above, which are made against
data certified by Sui validators, but they bound what a proof means:

* **Gas is not metered.** Gas price/budget are passed in so Move code sees the right values;
  the gas *charged* (to a gas coin or an address balance) is taken from the chain's effects and
  printed as "from chain, not computed". A proof therefore does not attest to gas.
* **Address balances are not modelled as state.** Deposits/withdrawals are recorded and compared
  with the chain, but a withdrawal is not checked against the owner's available balance (those
  balances live outside the objects a transaction reads).
* **The input state is checked outside the proof, not inside it.** Sui has no global state
  root to prove objects against. A proof says: *given* the input state with this hash, the
  transaction produces these results. The replay binds that state to the chain where Sui makes
  it possible: every object the transaction wrote, and every shared object it read, is
  re-hashed (`Blake2b("Object::" || object)`) and must equal the digest certified in the
  transaction's effects; so must every chain-side output object it is compared against.
  Dynamic-field children that were only *read* are recorded in the effects by version, not
  digest — those come from the GraphQL endpoint unverified (counted in the output). The effects
  themselves are taken from GraphQL; checking them against checkpoint signatures is out of
  scope.
* **Not supported** (clean error): `Publish`/`Upgrade` commands, system transactions,
  `Receiving` inputs, the unimplemented natives above, objects the GraphQL endpoint no longer
  serves (old history).
* **ed25519** follows `ed25519-dalek` semantics; Sui uses ZIP-215 rules. They agree on all
  honestly generated signatures and may differ on adversarial edge cases.
* **`--fast`** (opt-in, off by default): SHA-256 (an SP1 precompile) instead of Blake2b for
  object IDs — about 40% fewer cycles on DeepBook, but not Sui's IDs. Off for all replays.
* **Discovery reruns.** Dynamic-field children that a transaction only *looks up* (and that may
  not exist) are not listed in the effects; replay finds them by running, stopping at the first
  unknown child, fetching it, and running again. SP1 v6's executor ignores user hooks
  (`with_hook` is accepted but never called), so this cannot be done in one pass.
* `examples/deepbook-v3-demo` uses DeepBook's `#[test_only]` helpers to set up a pool; the
  chain demo (`examples/deepbook-v3-chain`) and all replays use production code only.

## How it works

```
Move source ──(Sui move-compiler)──┐
                                   ├─► bytecode ─► LLVM IR ─► riscv64 object ─► ELF (+ runtime) ─► SP1
mainnet package (GraphQL) ─────────┘   translate/   (inkwell)    LLVM O2        link.rs
```

* `compiler/src/translate/` — bytecode → LLVM: values are typed LLVM aggregates, generics are
  monomorphised, every type gets a runtime descriptor (layout + name) so the runtime can
  (de)serialize BCS, hash type tags, store objects and print values.
* `runtime/` — `no_std` Rust, linked into every program: entry/exit, BCS, natives, the object
  store and dynamic fields, the PTB interpreter, effects, state snapshots. SP1 precompiles for
  SHA-256, Keccak, secp256k1/r1, ed25519.
* `runner/` (`movevm`) — executes/proves ELFs with the SP1 SDK and decodes public values.
* `replay/` (`sui-replay`) — the mainnet replay tool; `out/replay/cache/` caches every GraphQL
  response, so replays are reproducible offline.

## How to run

Requirements: Rust (rustup), LLVM 20 (`brew install llvm@20`; path set in `.cargo/config.toml`),
and SP1's toolchain (`curl -L https://sp1up.succinct.xyz | bash && sp1up`) for the runtime.

```sh
cargo build --release
# replay a mainnet transaction
target/release/sui-replay AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq
# ... and prove exactly that run
d=out/replay/AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq
target/release/movevm prove $d/image.elf "$(cat $d/ctx.json)" --ptb $d/ptb.json --state-in $d/state_in.bin
# replay a sample of the latest mainnet transactions
scripts/replay-sample.py 50
# compile and run Move directly
target/release/move2sp1 build examples/fib.move -a demo=0x100 -e fib -o fib.elf
target/release/movevm run fib.elf 30
# DeepBook v3's unit tests in the zkVM
scripts/test-packages.sh
# regression suite
scripts/check.sh
```
