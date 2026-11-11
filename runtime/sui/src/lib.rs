//! move2sp1 runtime for Sui Move: the chain-agnostic core (`move-rt-core`) plus natives of
//! the Sui framework (0x2). The compiler side is `compiler/src/translate/sui.rs`.
//!
//! Sui state that would live on chain is modelled per run ("state in, state out"):
//! * `tx`      — the transaction context (sender, digest, fresh object IDs), from the
//!               program's `TxContext` input
//! * `store`   — child objects of dynamic fields (what `Table`, `Bag`, ... are built on) and
//!               shared objects (taken and returned with `sui::test_scenario`)
//! * `clock`   — the `Clock` singleton
//! * `effects` — events, transfers, shares and freezes, committed after the results
#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

// The core provides the SP1 entry point, allocation and std natives.
pub use move_rt_core;

mod clock;
mod effects;
mod store;
mod tx;

/// A 32-byte Sui address, as laid out in memory by generated code.
pub type Address = [u8; 32];
