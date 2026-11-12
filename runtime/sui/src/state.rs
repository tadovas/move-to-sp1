//! State snapshots: the persistent objects (account-owned, shared, dynamic-field children)
//! carried from one run to the next — "state in, state out".
//!
//! A run starts by loading its input snapshot (the first SP1 stdin buffer, possibly empty)
//! and ends by committing, after the results and effects:
//! `sha256(input snapshot) || len (ULEB128) || output snapshot`.
//! So a verifier can chain proofs: run N+1's input hash equals the hash of run N's output.
//!
//! Snapshot encoding (canonical: entries sorted by object ID, so the hash of a state does not
//! depend on which program wrote it): ULEB128 count, then per object:
//! `kind (u8: 0 owned, 1 shared, 2 dynamic-field child) || id (32) || owner or parent (32;
//! zero for shared) || canonical type name (ULEB128 len + bytes) || BCS value (ULEB128 len + bytes)`.
//!
//! An input snapshot may end with `partial (u8) || ULEB128 count || absent IDs (32 each)`:
//! a state fetched from chain, where unknown dynamic-field children are reported rather than
//! treated as absent (see `store::set_partial`). Output snapshots never have it.
//!
//! Objects are rebuilt in memory through the program's type registry (canonical name ->
//! descriptor, emitted by the compiler for every type that can reach the store). Objects of
//! types the program never touches are passed through unchanged.

use alloc::vec::Vec;

use move_rt_core::{
    abort_in, bcs,
    desc::{self, Slot, TypeDesc, Value},
    global::Global,
};
use sha2::{Digest, Sha256};

use crate::{
    Address,
    location::STATE,
    store::{self, Placement},
};

/// Abort codes (location `move2sp1::state`).
const E_BAD_SNAPSHOT: u64 = 1;
const E_TYPE_NOT_IN_REGISTRY: u64 = 2;
pub(crate) const E_OBJECT_NOT_IN_STATE: u64 = 3;
pub(crate) const E_NOT_OWNER: u64 = 4;

#[repr(C)]
struct RegistryEntry {
    name: *const u8,
    name_len: u64,
    desc: *const TypeDesc,
}

unsafe extern "C" {
    /// Emitted by the compiler: `__move_type_registry_len` entries.
    static __move_type_registry: [RegistryEntry; 0];
    static __move_type_registry_len: u64;
}

fn registry() -> &'static [RegistryEntry] {
    // SAFETY: the compiler emits an array of `__move_type_registry_len` entries.
    unsafe {
        core::slice::from_raw_parts(
            __move_type_registry.as_ptr(),
            __move_type_registry_len as usize,
        )
    }
}

impl RegistryEntry {
    fn name(&self) -> &'static [u8] {
        // SAFETY: registry names are constants of `name_len` bytes.
        unsafe { core::slice::from_raw_parts(self.name, self.name_len as usize) }
    }
    fn desc(&self) -> &'static TypeDesc {
        // SAFETY: registry descriptors are the program's constant descriptors.
        unsafe { &*self.desc }
    }
}

/// (hash of the input snapshot, input entries of types this program does not know: ID and
/// encoded entry)
static SNAPSHOT: Global<([u8; 32], Vec<(Address, Vec<u8>)>)> = Global::new(([0; 32], Vec::new()));

/// Platform hook called by the entry glue before reading arguments: load the input state.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_start() {
    let snapshot: Vec<u8> = sp1_zkvm::io::read_vec();
    let hash: [u8; 32] = Sha256::digest(&snapshot).into();
    let mut input = snapshot.as_slice();
    let mut opaque = Vec::new();
    let n = if input.is_empty() {
        0
    } else {
        read_uleb(&mut input)
    };
    for _ in 0..n {
        let entry_start = input;
        let kind = take(&mut input, 1)[0];
        let id: Address = take(&mut input, 32).try_into().unwrap();
        let other: Address = take(&mut input, 32).try_into().unwrap();
        let name_len = read_uleb(&mut input) as usize;
        let name = take(&mut input, name_len);
        let value_len = read_uleb(&mut input) as usize;
        let mut value = take(&mut input, value_len);
        let placement = match kind {
            0 => Placement::Owned(other),
            1 => Placement::Shared,
            2 => Placement::Child(other),
            _ => abort_in(STATE, E_BAD_SNAPSHOT),
        };
        match registry().iter().find(|e| e.name() == name) {
            Some(entry) => {
                let d = entry.desc();
                let data = desc::alloc(d.size() as usize);
                // SAFETY: `data` is a fresh allocation of `d.size` bytes.
                let slot = unsafe { Slot::new(d, data) };
                if bcs::decode(slot, &mut value).is_err() || !value.is_empty() {
                    abort_in(STATE, E_BAD_SNAPSHOT);
                }
                store::insert_stored(placement, id, d, data);
            }
            None => {
                // unknown to this program: pass through unchanged
                let consumed = entry_start.len() - input.len();
                opaque.push((id, entry_start[..consumed].to_vec()));
            }
        }
    }
    if !input.is_empty() && take(&mut input, 1)[0] == 1 {
        let absent = (0..read_uleb(&mut input))
            .map(|_| take(&mut input, 32).try_into().unwrap())
            .collect();
        store::set_partial(absent);
    }
    SNAPSHOT.with(|s| *s = (hash, opaque));
}

/// Commit `sha256(input snapshot) || len || output snapshot` (called by `move_rt_finish`).
pub(crate) fn commit() {
    let (in_hash, mut entries) = SNAPSHOT.with(|s| (s.0, core::mem::take(&mut s.1)));
    store::for_each_stored(|placement, id, d, data| {
        let (kind, other) = match placement {
            Placement::Owned(a) => (0u8, a),
            Placement::Shared => (1, [0; 32]),
            Placement::Child(parent) => (2, parent),
        };
        let name = registry()
            .iter()
            .find(|e| core::ptr::eq(e.desc(), d))
            .map(|e| e.name())
            .unwrap_or_else(|| abort_in(STATE, E_TYPE_NOT_IN_REGISTRY));
        let mut value = Vec::new();
        // SAFETY: stored objects are initialized values of their descriptor's type.
        bcs::encode(unsafe { Value::new(d, data) }, &mut value);
        let mut entry = Vec::new();
        entry.push(kind);
        entry.extend_from_slice(id);
        entry.extend_from_slice(&other);
        bcs::uleb128(name.len() as u64, &mut entry);
        entry.extend_from_slice(name);
        bcs::uleb128(value.len() as u64, &mut entry);
        entry.extend_from_slice(&value);
        entries.push((*id, entry));
    });
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut snapshot = Vec::new();
    bcs::uleb128(entries.len() as u64, &mut snapshot);
    for (_, entry) in &entries {
        snapshot.extend_from_slice(entry);
    }

    let mut out = in_hash.to_vec();
    bcs::uleb128(snapshot.len() as u64, &mut out);
    out.extend_from_slice(&snapshot);
    sp1_zkvm::io::commit_slice(&out);
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> &'a [u8] {
    if input.len() < n {
        abort_in(STATE, E_BAD_SNAPSHOT);
    }
    let (h, t) = input.split_at(n);
    *input = t;
    h
}

fn read_uleb(input: &mut &[u8]) -> u64 {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = take(input, 1)[0];
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return v;
        }
    }
    abort_in(STATE, E_BAD_SNAPSHOT)
}
