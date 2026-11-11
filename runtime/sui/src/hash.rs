//! Sui's hashing: `DefaultHash` (Blake2b-256) with intent-scope prefixes for object IDs, and
//! the `sui::hash` natives.

use core::mem::MaybeUninit;

use blake2::{Blake2b, Digest, digest::consts::U32};
use move_rt_core::desc::VecHeader;

use crate::Address;

/// `HashingIntentScope::ChildObjectId` (dynamic field IDs).
pub const CHILD_OBJECT_ID: u8 = 0xf0;
/// `HashingIntentScope::RegularObjectId` (`object::new`).
pub const REGULAR_OBJECT_ID: u8 = 0xf1;

/// Blake2b-256 of the concatenation of `parts`.
pub fn blake2b256(parts: &[&[u8]]) -> Address {
    let mut h = Blake2b::<U32>::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// SHA-256 of the concatenation of `parts` (SP1 precompile): for `--fast-ids`, where IDs only
/// need to be unique, not equal to Sui's.
pub fn sha256(parts: &[&[u8]]) -> Address {
    use sha2::Sha256;
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The ID hash: Sui's Blake2b-256, or SHA-256 for `--fast-ids`.
pub fn id_hash(fast: bool, parts: &[&[u8]]) -> Address {
    if fast {
        sha256(parts)
    } else {
        blake2b256(parts)
    }
}

/// `sui::hash::blake2b256(&vector<u8>)` (software: SP1 has no Blake2b precompile).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_blake2b256(data: &VecHeader, out: &mut MaybeUninit<VecHeader>) {
    out.write(VecHeader::from_bytes(&blake2b256(&[data.bytes()])));
}

/// `sui::hash::keccak256(&vector<u8>)` (SP1 Keccak precompile via the patched `tiny-keccak`).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_keccak256(data: &VecHeader, out: &mut MaybeUninit<VecHeader>) {
    use tiny_keccak::{Hasher, Keccak};
    let mut h = Keccak::v256();
    h.update(data.bytes());
    let mut digest = [0u8; 32];
    h.finalize(&mut digest);
    out.write(VecHeader::from_bytes(&digest));
}
