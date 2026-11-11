//! Move native functions implemented in the runtime. Vectors are passed as pointers to
//! their headers; results are written through an `out` header pointer.

use alloc::string::String;

use crate::{
    bcs,
    desc::{TypeDesc, VecHeader},
    print,
};

// ---- std::bcs

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_bcs_to_bytes(d: &TypeDesc, p: *const u8, out: *mut VecHeader) {
    unsafe { out.write(bcs::to_bytes(d, p)) };
}

// ---- std::debug

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_debug_print(d: &TypeDesc, p: *const u8) {
    let mut s = String::from("[debug] ");
    unsafe { print::format(d, p, &mut s) };
    s.push('\n');
    sp1_zkvm::syscalls::syscall_write(1, s.as_ptr(), s.len());
}

// ---- std::hash

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sha2_256(data: *const VecHeader, out: *mut VecHeader) {
    use sha2::Digest;
    let h = sha2::Sha256::digest(unsafe { VecHeader::bytes(data) });
    unsafe { out.write(VecHeader::from_bytes(&h)) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sha3_256(data: *const VecHeader, out: *mut VecHeader) {
    use sha3::Digest;
    let h = sha3::Sha3_256::digest(unsafe { VecHeader::bytes(data) });
    unsafe { out.write(VecHeader::from_bytes(&h)) };
}

// ---- std::string (UTF-8 helpers)

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_utf8_check(v: *const VecHeader) -> bool {
    core::str::from_utf8(unsafe { VecHeader::bytes(v) }).is_ok()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_utf8_is_char_boundary(v: *const VecHeader, i: u64) -> bool {
    let s = unsafe { core::str::from_utf8_unchecked(VecHeader::bytes(v)) };
    s.is_char_boundary(i as usize)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_utf8_sub_string(
    v: *const VecHeader,
    i: u64,
    j: u64,
    out: *mut VecHeader,
) {
    let b = unsafe { VecHeader::bytes(v) };
    unsafe { out.write(VecHeader::from_bytes(&b[i as usize..j as usize])) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_utf8_index_of(v: *const VecHeader, r: *const VecHeader) -> u64 {
    let s = unsafe { core::str::from_utf8_unchecked(VecHeader::bytes(v)) };
    let r = unsafe { core::str::from_utf8_unchecked(VecHeader::bytes(r)) };
    s.find(r).unwrap_or(s.len()) as u64
}
