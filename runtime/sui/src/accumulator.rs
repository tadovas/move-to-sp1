//! Address balances (`sui::funds_accumulator`): deposits and withdrawals are recorded as
//! effects (`accumulator` records: owner, balance type, merge/split, amount), as Sui records
//! them for settlement at the end of the checkpoint.
//!
//! The balances themselves are not modelled (they live outside the objects a transaction
//! reads): a withdrawal is not checked against the owner's available funds.

use move_rt_core::{
    abort_in,
    desc::{Kind, TypeDesc},
};

use crate::Address;

const FUNDS: &str =
    "0000000000000000000000000000000000000000000000000000000000000002::funds_accumulator";
/// Not a balance-like type (a struct whose single field is, recursively, a `u64`).
const E_NOT_A_BALANCE: u64 = 100;
const E_AMOUNT_TOO_LARGE: u64 = 101;

pub(crate) const MERGE: u8 = 0;
pub(crate) const SPLIT: u8 = 1;

/// Offset of the single `u64` inside a balance-like value of type `d`.
fn amount_offset(d: &TypeDesc) -> usize {
    match d.kind() {
        Kind::U64 => 0,
        Kind::Struct if d.fields().len() == 1 => {
            let f = &d.fields()[0];
            f.offset() + amount_offset(f.ty())
        }
        _ => abort_in(FUNDS, E_NOT_A_BALANCE),
    }
}

/// `funds_accumulator::add_to_accumulator_address<T>(_, recipient, value)`.
///
/// # Safety
/// `value` points to a `T` described by `d`; `name` to `name_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_accumulator_add(
    d: &TypeDesc,
    name: *const u8,
    name_len: u64,
    value: *const u8,
    recipient: &Address,
) {
    // SAFETY: guaranteed by the caller.
    let amount = unsafe { *(value.add(amount_offset(d)) as *const u64) };
    // SAFETY: as above.
    unsafe { crate::effects::record_accumulator(name, name_len, recipient, MERGE, amount) };
}

/// `funds_accumulator::withdraw_from_accumulator_address<T>(_, owner, value)`: a `T` holding
/// `value`, taken from `owner`'s balance.
///
/// # Safety
/// `out` is valid for writes of a `T` described by `d`; `name` points to `name_len` bytes;
/// `value` to a `u256`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_accumulator_withdraw(
    d: &TypeDesc,
    name: *const u8,
    name_len: u64,
    owner: &Address,
    value: *const [u8; 32],
    out: *mut u8,
) {
    // SAFETY: guaranteed by the caller.
    let v = unsafe { *value };
    if v[8..].iter().any(|&b| b != 0) {
        abort_in(FUNDS, E_AMOUNT_TOO_LARGE);
    }
    let amount = u64::from_le_bytes(v[..8].try_into().unwrap());
    // SAFETY: `out` has room for a `T`, whose only data is the amount.
    unsafe {
        core::ptr::write_bytes(out, 0, d.size() as usize);
        *(out.add(amount_offset(d)) as *mut u64) = amount;
        crate::effects::record_accumulator(name, name_len, owner, SPLIT, amount);
    }
}
