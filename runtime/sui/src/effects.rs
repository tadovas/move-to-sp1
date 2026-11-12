//! Transaction effects: events (`sui::event::emit`) and object transfers / shares / freezes.
//!
//! Recorded during the run and committed to the public values after the entry results by
//! `move_rt_finish`, so a proof attests to them. Encoding (BCS):
//! `vector<Effect>` where each effect is `tag: u8, type: String (canonical name), [recipient: address], value: vector<u8>`
//! with tags 0 = event, 1 = transfer, 2 = share, 3 = freeze, and `value` the BCS of the object.

use alloc::vec::Vec;

use move_rt_core::{
    bcs,
    desc::{TypeDesc, Value},
    global::Global,
};

use crate::Address;

const EVENT: u8 = 0;
const TRANSFER: u8 = 1;
const SHARE: u8 = 2;
const FREEZE: u8 = 3;

/// (number of effects, their encoding)
static LOG: Global<(u64, Vec<u8>)> = Global::new((0, Vec::new()));

/// # Safety
/// `name` points to `name_len` bytes.
unsafe fn record(tag: u8, v: Value, name: *const u8, name_len: u64, recipient: Option<&Address>) {
    let mut value = Vec::new();
    bcs::encode(v, &mut value);
    // SAFETY: guaranteed by the caller.
    let name = unsafe { core::slice::from_raw_parts(name, name_len as usize) };
    LOG.with(|(n, out)| {
        *n += 1;
        out.push(tag);
        bcs::uleb128(name.len() as u64, out);
        out.extend_from_slice(name);
        if let Some(r) = recipient {
            out.extend_from_slice(r);
        }
        bcs::uleb128(value.len() as u64, out);
        out.extend_from_slice(&value);
    });
}

/// # Safety
/// `p` points to an initialized value of type `d`; `name` to `name_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_emit(
    d: &TypeDesc,
    name: *const u8,
    name_len: u64,
    p: *const u8,
) {
    // SAFETY: guaranteed by the caller.
    unsafe { record(EVENT, Value::new(d, p), name, name_len, None) };
}

/// # Safety
/// `p` points to an initialized object of type `d`; `name` to `name_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_transfer(
    d: &TypeDesc,
    name: *const u8,
    name_len: u64,
    p: *const u8,
    recipient: &Address,
) {
    // SAFETY: guaranteed by the caller.
    unsafe { record(TRANSFER, Value::new(d, p), name, name_len, Some(recipient)) };
    // Owned by `recipient` from now on (`test_scenario::take_from_address`).
    // SAFETY: as above.
    unsafe { crate::store::put(Some(*recipient), d, p) };
}

/// # Safety
/// `p` points to an initialized object of type `d`; `name` to `name_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_share(
    d: &TypeDesc,
    name: *const u8,
    name_len: u64,
    p: *const u8,
) {
    // SAFETY: guaranteed by the caller.
    unsafe { record(SHARE, Value::new(d, p), name, name_len, None) };
    // Shared objects stay available to later transactions (`test_scenario::take_shared`).
    // SAFETY: as above.
    unsafe { crate::store::put(None, d, p) };
}

/// # Safety
/// `p` points to an initialized object of type `d`; `name` to `name_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_freeze(
    d: &TypeDesc,
    name: *const u8,
    name_len: u64,
    p: *const u8,
) {
    // SAFETY: guaranteed by the caller.
    unsafe { record(FREEZE, Value::new(d, p), name, name_len, None) };
}

/// Platform hook called by the entry glue after the results: commit the effects, then the
/// state (`state.rs`).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_finish() {
    let mut out = Vec::new();
    LOG.with(|(n, log)| {
        bcs::uleb128(*n, &mut out);
        out.extend_from_slice(log);
    });
    sp1_zkvm::io::commit_slice(&out);
    crate::state::commit();
}
