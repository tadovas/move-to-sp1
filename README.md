# move2sp1 — Sui transactions, proven

**move2sp1** compiles Move — from source, or straight from the bytecode of packages deployed on
Sui — ahead of time to RISC-V and runs it in [SP1](https://github.com/succinctlabs/sp1),
Succinct's zero-knowledge virtual machine. On top of it, **sui-replay** takes any Sui mainnet
transaction by its digest, re-executes it inside SP1, and checks the result byte for byte
against what the chain recorded. Every such run can be proven, and proofs of many transactions
can be folded into one.

* **572 of 572** user transactions of 20 consecutive mainnet checkpoints (~5 s of Sui) replayed
  inside SP1 with every written object, event and balance change **byte-identical** to mainnet.
* **One proof for a whole checkpoint**: each transaction proven, then all of them verified inside
  SP1 by an aggregation program — a single constant-size proof for the batch.
* Real mainnet transactions (DeepBook v3 orders, DeFi routes, lending) **proven on a laptop CPU in
  26–83 s**.
* DeepBook v3's own unit test suite, compiled to RISC-V: **404/404 pass** inside the zkVM.

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

The transaction above on the explorer: [AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq](https://suiscan.xyz/mainnet/tx/AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq).

## Why

Sui executes Move in a VM on its validators; checking a Sui execution means re-running it on a
full node. Proving Move execution in a zkVM makes it checkable by anyone — a light client,
another chain, a rollup that wants Sui's programming model, an auditor. A Move *interpreter*
inside a zkVM would prove the interpreter's loop for every instruction; move2sp1 instead
**compiles** Move to native RISC-V, so the zkVM proves the program itself, with Sui's natives
backed by SP1's precompiles (SHA-256, Keccak, secp256k1/r1, ed25519, BLS12-381).

## What works

* **A Move compiler for SP1** (`compiler/`, binary `move2sp1`): Move bytecode → LLVM IR →
  riscv64 → its own ELF linker, plus a `no_std` Rust runtime (`runtime/`). Structs, enums,
  generics (monomorphised), references, vectors, abilities; aborts with Sui's exact codes and
  locations; the complete Move standard library and Sui framework.
  * Input: Move **source** (Sui's front end, `Move.toml` packages, frameworks embedded), or
    **bytecode** from chain, including upgraded packages (linkage tables, type origins).
  * Sui semantics, exactly: object and dynamic-field IDs, type tags, BCS, events, transfers,
    shared/owned/child objects, `TxContext` (sender, sponsor, epoch, gas price, fresh IDs),
    `Clock`, address balances, crypto natives (secp256k1, secp256r1, ed25519, BLS12-381
    signatures and group operations, HMAC, Blake2b, Keccak, SHA-2/3).
* **Transactions as data**: a program *image* compiles a set of functions plus a dispatch table;
  the transaction — a programmable transaction block (PTB) — is *input* to a PTB interpreter in
  the runtime. One image, one verifying key, serves every transaction calling those functions.
  All PTB commands except `Publish`/`Upgrade`: `MoveCall`, `TransferObjects`, `SplitCoins`,
  `MergeCoins`, `MakeMoveVec`; gas coins with smashing, sponsors, address-balance gas, coin
  reservations, address-balance withdrawals.
* **State in, state out**: a run reads a canonical state snapshot and commits
  `sha256(input state)`, its results, its effects and the output state, so proofs of
  consecutive transactions chain.
* **sui-replay** (`replay/`): fetches a mainnet transaction and its certified effects over
  Sui's public GraphQL API, the exact package versions it ran (and the framework of its epoch),
  its input objects at their input versions; compiles an image; executes it in SP1; and compares
  **every written object, every event and every address-balance change** with the chain — or,
  for a transaction that failed on chain, that it fails with the same abort code in the same
  module. Input objects and the chain's outputs are checked against the digests certified in the
  effects.
* **Proving** (`runner/`, binary `movevm`): `movevm prove` proves and verifies a run on the local
  CPU prover; `movevm aggregate` proves a batch of runs and folds them into one proof: an SP1
  program (`runtime/aggregator`) verifies each run's compressed proof inside the zkVM and commits
  to (verifying key, public-values digest) for each.
* **Unit tests in the zkVM**: `move2sp1 test` + `movevm test` compile and run a package's
  `#[test]`s, checking `#[expected_failure]` codes and locations.

## Results

**Mainnet replay** — every transaction compared byte for byte with chain:

| sample | transactions | byte-identical | other |
|---|---|---|---|
| checkpoints [344,393,000](https://suiscan.xyz/mainnet/checkpoint/344393000)–[019](https://suiscan.xyz/mainnet/checkpoint/344393019) (20 consecutive, ~5 s of Sui) | 572 | **572** | — |
| checkpoints [344,395,000](https://suiscan.xyz/mainnet/checkpoint/344395000)–[004](https://suiscan.xyz/mainnet/checkpoint/344395004) | 127 | **127** | — |
| checkpoints [335,000,000](https://suiscan.xyz/mainnet/checkpoint/335000000)–[004](https://suiscan.xyz/mainnet/checkpoint/335000004) (a month earlier) | 42 | **42** | — |
| checkpoints [150,000,000](https://suiscan.xyz/mainnet/checkpoint/150000000)–[009](https://suiscan.xyz/mainnet/checkpoint/150000009) (May 2025) | 61 | **61** | — |
| 200 random recent transactions | 200 | 199 (the other one fixed since) | — |

`scripts/replay-sample.py --checkpoint A-B` (8 in parallel: 572 transactions in 2 min 48 s, ~510k
cycles each on average). Transactions that failed on chain count as byte-identical when the
replay fails with the same abort code in the same module.

**Transactions worth a look** (each replays with `target/release/sui-replay <digest>`):

* [`GpL3CbLNzdNjq7MPeyJfhrMeEhmBzdQaYUxpjjmnUQpz`](https://suiscan.xyz/mainnet/tx/GpL3CbLNzdNjq7MPeyJfhrMeEhmBzdQaYUxpjjmnUQpz) — failed on chain (abort 47 in `clearing_house`); the replay fails identically
* [`H5Zxu6H8GSswPDUDFEYWMcikQbZQG3PTyLeQMRt2gtJL`](https://suiscan.xyz/mainnet/tx/H5Zxu6H8GSswPDUDFEYWMcikQbZQG3PTyLeQMRt2gtJL) — a coin reservation: USDC drawn from the address balance as a coin input
* [`2i8CLWTLXyaLX8BY626VjhpgPYWHGA7NyD3NRPqN7FR9`](https://suiscan.xyz/mainnet/tx/2i8CLWTLXyaLX8BY626VjhpgPYWHGA7NyD3NRPqN7FR9) — BLS12-381: aggregates public keys (`group_ops`) and verifies a min-pk signature, 5.3M cycles
* [`9DznnpWWjvuQPEAwP4uCBZMMAksCwY3YqbUkaohkh7bT`](https://suiscan.xyz/mainnet/tx/9DznnpWWjvuQPEAwP4uCBZMMAksCwY3YqbUkaohkh7bT) — BLS12-381 min-sig signature check, May 2025
* [`D46QLmVyfRNEDFsySE8HVAHBFezYD9CD7p6Z5s6g6xMz`](https://suiscan.xyz/mainnet/tx/D46QLmVyfRNEDFsySE8HVAHBFezYD9CD7p6Z5s6g6xMz) — sponsored: the gas coin belongs to someone else and is passed to Move
* [`8jJPwkFrk4H8dP1CvT2fWuuA6M2KCFVhhAjsnkYSevRR`](https://suiscan.xyz/mainnet/tx/8jJPwkFrk4H8dP1CvT2fWuuA6M2KCFVhhAjsnkYSevRR) — a gas station contract reading `tx_context::sponsor()`
* [`6nbjaQVzRszuX3drRBym3pWWZL4o2MwbC5nFo4SnSA3X`](https://suiscan.xyz/mainnet/tx/6nbjaQVzRszuX3drRBym3pWWZL4o2MwbC5nFo4SnSA3X) — 210 written objects and events, all compared
* [`CcTWoDwsfRWG8zzGXxgjL9R9YBbYTcyhEES67EhcKg5J`](https://suiscan.xyz/mainnet/tx/CcTWoDwsfRWG8zzGXxgjL9R9YBbYTcyhEES67EhcKg5J) — calls DeepBook v2 code (an old package version) — the case that exposed the GraphQL version quirk
* [`HPNM9z6cMd9qKMTfMXddAGQK4m8tdMKj3Z244Eg3wCHB`](https://suiscan.xyz/mainnet/tx/HPNM9z6cMd9qKMTfMXddAGQK4m8tdMKj3Z244Eg3wCHB) — gas from the address balance; the gas coin takes the first new object ID

**Proving on a laptop** (`scripts/bench-replay.sh`: replay, check against chain, then prove and
verify exactly that run; SP1 6.10 CPU prover, Apple M1 Max, 10 cores, no GPU):

| transaction | PTB | cycles | prove | verify | proof |
|---|---|---|---|---|---|
| DeepBook v3 market order ([`AjzaKV…`](https://suiscan.xyz/mainnet/tx/AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq)) | 2 Move calls | 596k | 26.2 s | 85 ms | 2.7 MiB |
| DeFi route ([`4R9QDP…`](https://suiscan.xyz/mainnet/tx/4R9QDP1EfXCYTD6MN15i1nU2absKgiDjdYDGkf3zbbTC)) | 26 calls, 2 splits, 2 merges, balance withdrawal | 576k | 27.2 s | 85 ms | 2.7 MiB |
| lending repay ([`Haf4Th…`](https://suiscan.xyz/mainnet/tx/Haf4Thpbobnau4HLv2UXNSbch9Q295rRGioJY8WbhxmA)) | 2 calls, 2 splits, 2 merges, gas reservation | 863k | 30.2 s | 85 ms | 2.7 MiB |
| 17 Move calls ([`8zj6P6…`](https://suiscan.xyz/mainnet/tx/8zj6P6w13Z2SbLn2epw1sF2sRUeqn2zDn43fz7hTpztt)) | 17 calls | 1.61M | 37.8 s | 86 ms | 2.7 MiB |
| ed25519 signature checks ([`8sc3Gw…`](https://suiscan.xyz/mainnet/tx/8sc3GwqkF1PpBSTRbS5WccfG1FMo18YVJC3q6LFpDSkt)) | 13 calls | 2.64M | 82.8 s | 322 ms | 9.8 MiB |
| DeepBook market order, compressed proof | 2 Move calls | 596k | 60.8 s | 36 ms | 1.2 MiB |

Core proofs unless noted; a compressed proof is SP1's constant-size recursive proof. A trivial
program takes ~22 s, so for typical transactions most of the time is the prover's fixed cost.
Execution alone takes 15–100 ms per transaction.

**One proof per batch** (`scripts/prove-checkpoint.sh A-B`; same laptop): every user transaction
of mainnet checkpoints [150,000,003](https://suiscan.xyz/mainnet/checkpoint/150000003)–[006](https://suiscan.xyz/mainnet/checkpoint/150000006) — 12 transactions, 11.0M cycles in total — replayed
(byte-identical), each proven with a compressed proof (76–206 s each), then all 12 verified
inside SP1 by the aggregation program in **294 s**. The result is a single **1.2 MiB proof that
verifies in 38 ms**, committing to each transaction's program and public values (input state
hash, effects, output state). 22 minutes end to end.

**Other**

* DeepBook v3's unit tests: 404/404 (`scripts/test-packages.sh`); regression suite
  `scripts/check.sh`: 37/37.
* Natives: 82 of the 105 natives Sui's framework has declared over its history
  (`scripts/natives-coverage.py --history`; only system packages may declare natives, so the list
  is complete). The rest — Groth16, Poseidon, VRF, VDF, zkLogin, Nitro attestation,
  bulletproofs, BLS12-381 pairings/GT, receiving and party transfers — compile to an abort, so a
  transaction fails to replay only if it actually calls one, with a clean "not supported".

## What's faked, mocked or not covered

None of these affect the byte-for-byte comparisons, which are made against data certified by
Sui's validators; they bound what a proof attests to.

* **Gas is not metered.** Gas price and budget are passed in so Move code sees the right values;
  the gas *charged* (to a gas coin or an address balance) is taken from the chain's effects and
  printed as "from chain, not computed". A proof does not attest to gas.
* **Address balances are not modelled as state.** Deposits and withdrawals are recorded and
  compared with the chain, but a withdrawal is not checked against the available balance (those
  balances live outside the objects a transaction reads).
* **The input state is checked outside the proof, not inside it.** Sui has no global state root
  to prove objects against. A proof says: *given* the input state with this hash, the transaction
  produces these results. The replay binds that state to the chain where Sui makes it possible:
  every object the transaction wrote, and every shared object it read, is re-hashed
  (`Blake2b("Object::" || object)`) and must equal the digest certified in the effects — as must
  every chain-side object the outputs are compared with. Dynamic-field children that were only
  *read* are recorded in the effects by version, not digest, and come from the GraphQL endpoint
  unverified (the replay counts them). The effects themselves are taken from GraphQL; checking
  them against checkpoint signatures is not implemented.
* **Coin reservations** (an input standing for funds reserved from the sender's address
  balance: ID = the balance's accumulator ID XOR the chain identifier, "digest" = amount ‖ epoch
  ‖ `0xac…`) are decoded from that encoding — worked out from mainnet data, not from a
  specification — and become a fresh coin, as Sui's address-balance gas does.
* **Not supported** (clean error): `Publish`/`Upgrade` commands, system transactions,
  `Receiving` inputs, the unimplemented natives above, objects the GraphQL endpoint no longer
  serves.
* **BLS12-381** `group_ops`: scalars, G1, G2 and uncompressed G1 with Sui's encodings; GT and
  pairings are not implemented (Sui's blst encoding of GT is not reproduced) and abort as
  unsupported.
* **ed25519** follows `ed25519-dalek`; Sui uses ZIP-215 rules. They agree on all honestly
  generated signatures and may differ on adversarial edge cases.
* **`--fast`** (opt-in, off by default and for all replays): SHA-256 (an SP1 precompile) instead
  of Blake2b for object IDs — ~40% fewer cycles on DeepBook, but not Sui's IDs.
* **Discovery reruns.** Dynamic-field children a transaction only *looks up* (and that may not
  exist) are not listed in the effects; the replay finds them by running, stopping at the first
  unknown child, fetching it and running again. SP1 v6's executor ignores user hooks (`with_hook`
  is accepted but never called), so this cannot be a single pass.
* `examples/deepbook-v3-demo` uses DeepBook's `#[test_only]` helpers to set up a pool;
  `examples/deepbook-v3-chain` and all replays run production code only.

## How it works

```
Move source ──(Sui move-compiler)──┐
                                   ├─► bytecode ─► LLVM IR ─► riscv64 object ─► ELF (+ runtime) ─► SP1
mainnet package (GraphQL) ─────────┘   translate/   (inkwell)    LLVM O2        link.rs
```

* **Compiler** (`compiler/src/translate/`): Move values become typed LLVM aggregates; generics
  are monomorphised; every type gets a runtime descriptor (layout and name) so the runtime can
  (de)serialize BCS, derive type tags and object IDs, store objects and print values.
* **Runtime** (`runtime/`, `no_std` Rust linked into every program): entry and exit, BCS, Move
  and Sui natives, the object store and dynamic fields, the PTB interpreter, effects, state
  snapshots, address balances.
* **Replay** (`replay/`): package versions resolved exactly (a storage ID names one version; the
  framework is the epoch's), input objects at their input versions, discovery runs for children
  the effects do not list, digests checked, outputs compared. Every GraphQL response is cached in
  `out/replay/cache/`, so replays are reproducible offline.
* **Batch proofs** (`runtime/aggregator`): an SP1 program reads each run's verifying-key digest
  and public values, verifies the run's compressed proof with SP1's recursion syscall, and
  commits the list.

Things mainnet taught (each found by a differing replay, then fixed): GraphQL's
`package(address)` returns the *latest* version of an upgraded package, so a call to DeepBook v2
must be resolved through the object at that address; system packages run at the epoch's version,
not the one in the caller's linkage table; when gas is paid from an address balance, Sui
materialises the gas coin first and it takes the transaction's first new object ID;
`tx_context::sponsor` matters to gas-station contracts; older frameworks had `type_name::get`
as a native.

## How to run

Requirements: Rust (rustup), LLVM 20 (`brew install llvm@20`; the path is set in
`.cargo/config.toml`), and SP1's toolchain (`curl -L https://sp1up.succinct.xyz | bash && sp1up`)
for the runtime.

```sh
cargo build --release

# replay a mainnet transaction, then prove exactly that run
target/release/sui-replay AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq
d=out/replay/AjzaKVKPjr49AZk68PpKJzo3D7spa753oMgnrA8DPGdq
target/release/movevm prove $d/image.elf "$(cat $d/ctx.json)" --ptb $d/ptb.json --state-in $d/state_in.bin

# every user transaction of a checkpoint range; or a random sample of recent ones
scripts/replay-sample.py --checkpoint 344393000-344393019
scripts/replay-sample.py 200

# one proof for whole checkpoints
scripts/prove-checkpoint.sh 150000003-150000006

# compile and run Move directly
target/release/move2sp1 build examples/fib.move -a demo=0x100 -e fib -o fib.elf
target/release/movevm run fib.elf 30

# a package's unit tests in the zkVM (default: DeepBook v3); regression suite
scripts/test-packages.sh
scripts/check.sh

# the whole walkthrough, step by step
scripts/demo.sh
```

## Repository layout

| path | |
|---|---|
| `compiler/` | `move2sp1`: front end, translator to LLVM, target, linker; embeds the runtime and the Move/Sui frameworks |
| `runtime/` | the `no_std` runtime linked into programs (`core`, `sui`) and the batch `aggregator` program |
| `runner/` | `movevm`: execute, prove, aggregate, run unit tests; decodes ABIs and public values |
| `replay/` | `sui-replay`: mainnet transaction replay and comparison |
| `examples/` | Move programs and packages, including DeepBook v2/v3 |
| `scripts/` | checks, benchmarks, sampling, checkpoint proofs, demo |
