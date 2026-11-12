//! `sui::tx_context` natives, backed by the transaction context the program received.

use alloc::vec::Vec;

use move_rt_core::{
    desc::{TypeDesc, Value, VecHeader},
    global::Global,
};

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
    /// The gas owner, when not the sender.
    sponsor: Option<Address>,
}

static TX: Global<TxState> = Global::new(TxState {
    sender: [0; 32],
    tx_hash: Vec::new(),
    epoch: 0,
    epoch_timestamp_ms: 0,
    ids_created: 0,
    // as in Sui's unit-test runner (`sui-move/src/unit_test.rs`)
    rgp: 0,
    gas_price: 0,
    gas_budget: 0,
    sponsor: None,
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

/// The transaction sender.
pub(crate) fn sender() -> Address {
    TX.with(|tx| tx.sender)
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

/// A fresh object ID, derived exactly as Sui does (`ObjectID::derive_id`):
/// `Blake2b256(0xF1 || tx_digest || ids_created as u64 LE)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_fresh_id(out: &mut Address) {
    *out = fresh_id(false);
}

/// `--fast`: the same derivation with SHA-256 (SP1 precompile) instead of Blake2b.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_fresh_id_fast(out: &mut Address) {
    *out = fresh_id(true);
}

unsafe extern "C" {
    /// Emitted by the compiler: 1 if the program was built with `--fast` IDs.
    static __move_fast_ids: u8;
}

/// A fresh object ID derived as the program's own `object::new` calls do (PTB commands).
pub(crate) fn new_id() -> Address {
    // SAFETY: a constant emitted by the compiler.
    fresh_id(unsafe { __move_fast_ids } != 0)
}

fn fresh_id(fast: bool) -> Address {
    TX.with(|tx| {
        let counter = tx.ids_created.to_le_bytes();
        let id = crate::hash::id_hash(
            fast,
            &[&[crate::hash::REGULAR_OBJECT_ID], &tx.tx_hash, &counter],
        );
        tx.ids_created += 1;
        id
    })
}

/// Gas parameters of a transaction run as a PTB (the PTB's gas header).
pub(crate) fn set_gas(gas_price: u64, rgp: u64, gas_budget: u64) {
    TX.with(|tx| {
        tx.gas_price = gas_price;
        tx.rgp = rgp;
        tx.gas_budget = gas_budget;
    });
}

/// The gas owner of a PTB: a sponsor if it is not the sender.
pub(crate) fn set_gas_owner(owner: &Address) {
    TX.with(|tx| tx.sponsor = (*owner != [0; 32] && *owner != tx.sender).then_some(*owner));
}

/// `tx_context::native_sponsor`: `vector<address>`, empty or the sponsor.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_sponsor(out: &mut core::mem::MaybeUninit<VecHeader>) {
    let s = TX.with(|tx| tx.sponsor);
    out.write(VecHeader::from_elems(
        s.as_ref().map_or(&[][..], |a| &a[..]),
        32,
    ));
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
            sponsor: None,
        }
    });
}
