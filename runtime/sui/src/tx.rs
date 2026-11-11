//! `sui::tx_context` natives, backed by the transaction context the program received.

use alloc::vec::Vec;

use move_rt_core::{
    desc::{TypeDesc, Value, VecHeader},
    global::Global,
};
use sha2::{Digest, Sha256};

use crate::Address;

struct TxState {
    sender: Address,
    tx_hash: Vec<u8>,
    epoch: u64,
    epoch_timestamp_ms: u64,
    ids_created: u64,
    rgp: u64,
    gas_price: u64,
    gas_budget: u64,
}

static TX: Global<TxState> = Global::new(TxState {
    sender: [0; 32],
    tx_hash: Vec::new(),
    epoch: 0,
    epoch_timestamp_ms: 0,
    ids_created: 0,
    rgp: 1_000,
    gas_price: 1_000,
    gas_budget: 50_000_000_000,
});

fn u64_of(v: Value) -> u64 {
    u64::from_le_bytes(v.prim_bytes().try_into().unwrap())
}

/// Load the transaction context from a `sui::tx_context::TxContext` value (called by the
/// entry glue for each `TxContext` argument).
///
/// # Safety
/// `p` points to an initialized `TxContext` described by `d`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_tx_init(d: &TypeDesc, p: *const u8) {
    // SAFETY: guaranteed by the caller.
    let ctx = unsafe { Value::new(d, p) };
    TX.with(|tx| {
        for (f, v) in ctx.fields() {
            match f.name() {
                "sender" => tx.sender.copy_from_slice(v.prim_bytes()),
                "tx_hash" => tx.tx_hash = v.vec_bytes().to_vec(),
                "epoch" => tx.epoch = u64_of(v),
                "epoch_timestamp_ms" => tx.epoch_timestamp_ms = u64_of(v),
                "ids_created" => tx.ids_created = u64_of(v),
                _ => {}
            }
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_sender(out: &mut Address) {
    *out = TX.with(|tx| tx.sender);
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_epoch() -> u64 {
    TX.with(|tx| tx.epoch)
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_epoch_timestamp_ms() -> u64 {
    TX.with(|tx| tx.epoch_timestamp_ms)
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_ids_created() -> u64 {
    TX.with(|tx| tx.ids_created)
}

/// A fresh object ID: `sha256(tx_hash || ids_created)` (deterministic per transaction; Sui
/// itself uses a Blake2b-based derivation, so IDs differ from on-chain ones).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_fresh_id(out: &mut Address) {
    *out = TX.with(|tx| {
        let mut h = Sha256::new();
        h.update(&tx.tx_hash);
        h.update(tx.ids_created.to_le_bytes());
        tx.ids_created += 1;
        h.finalize().into()
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_rgp() -> u64 {
    TX.with(|tx| tx.rgp)
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_gas_price() -> u64 {
    TX.with(|tx| tx.gas_price)
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_gas_budget() -> u64 {
    TX.with(|tx| tx.gas_budget)
}

/// `tx_context::replace` (test-only; `sui::test_scenario` starts each transaction with it).
/// Sponsors are not modelled.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn move_rt_sui_tx_replace(
    sender: &Address,
    tx_hash: &VecHeader,
    epoch: u64,
    epoch_timestamp_ms: u64,
    ids_created: u64,
    rgp: u64,
    gas_price: u64,
    gas_budget: u64,
) {
    TX.with(|tx| {
        *tx = TxState {
            sender: *sender,
            tx_hash: tx_hash.bytes().to_vec(),
            epoch,
            epoch_timestamp_ms,
            ids_created,
            rgp,
            gas_price,
            gas_budget,
        }
    });
}
