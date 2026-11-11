//! Move native functions implemented in the runtime.
//!
//! Vector arguments arrive as `&VecHeader` (generated code passes the address of the
//! header); results are written to an out-parameter `&mut MaybeUninit<VecHeader>`. Natives
//! taking a generic value (`bcs`, `debug`) receive its descriptor and address and are the
//! only `unsafe` ones: they build a [`Value`] view from the raw pointer.

use alloc::{string::String, vec::Vec};
use core::mem::MaybeUninit;

use crate::{
    bcs,
    desc::{TypeDesc, Value, VecHeader},
    print,
};

// ---- std::bcs

/// # Safety
/// `p` points to an initialized value of type `d`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_bcs_to_bytes(
    d: &TypeDesc,
    p: *const u8,
    out: &mut MaybeUninit<VecHeader>,
) {
    // SAFETY: guaranteed by the caller (generated code passes `&T` with T's descriptor).
    let v = unsafe { Value::new(d, p) };
    let mut bytes = Vec::new();
    bcs::encode(v, &mut bytes);
    out.write(VecHeader::from_bytes(&bytes));
}

// ---- std::debug

/// # Safety
/// `p` points to an initialized value of type `d`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_debug_print(d: &TypeDesc, p: *const u8) {
    // SAFETY: guaranteed by the caller (generated code passes `&T` with T's descriptor).
    let v = unsafe { Value::new(d, p) };
    let mut s = String::from("[debug] ");
    print::format(v, &mut s);
    s.push('\n');
    sp1_zkvm::syscalls::syscall_write(1, s.as_ptr(), s.len());
}

// ---- std::hash (SP1 precompiles via the patched `sha2` / `tiny-keccak` crates)

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sha2_256(data: &VecHeader, out: &mut MaybeUninit<VecHeader>) {
    use sha2::Digest;
    out.write(VecHeader::from_bytes(&sha2::Sha256::digest(data.bytes())));
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sha3_256(data: &VecHeader, out: &mut MaybeUninit<VecHeader>) {
    use tiny_keccak::{Hasher, Sha3};
    let mut h = Sha3::v256();
    h.update(data.bytes());
    let mut digest = [0u8; 32];
    h.finalize(&mut digest);
    out.write(VecHeader::from_bytes(&digest));
}

// ---- std::string (UTF-8 helpers; the Move side guarantees `v` holds valid UTF-8)

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_utf8_check(v: &VecHeader) -> bool {
    core::str::from_utf8(v.bytes()).is_ok()
}

/// Same rule as `str::is_char_boundary`, on bytes: not a UTF-8 continuation byte.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_utf8_is_char_boundary(v: &VecHeader, i: u64) -> bool {
    let b = v.bytes();
    match b.get(i as usize) {
        Some(&c) => (c as i8) >= -0x40,
        None => i as usize == b.len(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn move_rt_utf8_sub_string(
    v: &VecHeader,
    i: u64,
    j: u64,
    out: &mut MaybeUninit<VecHeader>,
) {
    out.write(VecHeader::from_bytes(&v.bytes()[i as usize..j as usize]));
}

/// Byte offset of the first occurrence of `r` in `v`, or `v`'s length if none. (A byte
/// search is exact for valid UTF-8.)
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_utf8_index_of(v: &VecHeader, r: &VecHeader) -> u64 {
    let (s, r) = (v.bytes(), r.bytes());
    let found = if r.is_empty() {
        Some(0)
    } else {
        s.windows(r.len()).position(|w| w == r)
    };
    found.unwrap_or(s.len()) as u64
}
