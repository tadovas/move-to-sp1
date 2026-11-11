//! Objects that outlive the Move call that created them: child objects of dynamic fields
//! (`sui::dynamic_field` natives) and shared objects (`sui::test_scenario` take/return).
//!
//! On chain these are separate objects owned by their parent; here they live in an
//! in-memory map for the duration of the run, keyed by child ID. Values are moved in and
//! out bitwise (Move semantics); heap data they reference is never freed, so it stays valid.

use alloc::{collections::BTreeMap, vec::Vec};
use core::ptr;

use move_rt_core::{
    bcs,
    desc::{self, TypeDesc, Value},
    global::Global,
    move_rt_abort,
};
use sha2::{Digest, Sha256};

use crate::Address;

// Abort codes of `sui::dynamic_field`.
const E_FIELD_ALREADY_EXISTS: u64 = 0;
const E_FIELD_DOES_NOT_EXIST: u64 = 1;
const E_FIELD_TYPE_MISMATCH: u64 = 2;

struct Child {
    parent: Address,
    /// Descriptors are unique per type, so pointer identity is type identity.
    desc: *const TypeDesc,
    data: *mut u8,
}

static STORE: Global<BTreeMap<Address, Child>> = Global::new(BTreeMap::new());

/// Look up child `id` of `parent`, checking it has type `d`.
fn find(parent: &Address, id: &Address, d: &TypeDesc) -> *mut u8 {
    let found = STORE.with(|s| {
        s.get(id)
            .map(|c| (c.parent == *parent, ptr::eq(c.desc, d), c.data))
    });
    match found {
        Some((true, true, data)) => data,
        Some((true, false, _)) => move_rt_abort(E_FIELD_TYPE_MISMATCH),
        _ => move_rt_abort(E_FIELD_DOES_NOT_EXIST),
    }
}

/// `hash_type_and_key<K>(parent, k)`: the child ID for key `k` of type `K`:
/// `sha256(parent || type name || bcs(k))`.
///
/// # Safety
/// `k` points to an initialized value of type `kd`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_df_hash(
    parent: &Address,
    kd: &TypeDesc,
    k: *const u8,
    out: &mut Address,
) {
    // SAFETY: guaranteed by the caller.
    let key = unsafe { Value::new(kd, k) };
    let mut key_bcs = Vec::new();
    bcs::encode(key, &mut key_bcs);
    let mut h = Sha256::new();
    h.update(parent);
    h.update(kd.name().as_bytes());
    h.update(&key_bcs);
    *out = h.finalize().into();
}

/// `add_child_object<Child>(parent, child)`. The child's ID is its leading `UID`.
///
/// # Safety
/// `child` points to an initialized object of type `d` (which starts with its `UID`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_df_add(parent: &Address, d: &TypeDesc, child: *const u8) {
    let size = d.size() as usize;
    let data = desc::alloc(size);
    // SAFETY: `child` holds `size` initialized bytes; `data` is a fresh allocation of `size`.
    unsafe { ptr::copy_nonoverlapping(child, data, size) };
    // SAFETY: objects start with `id: UID`, i.e. their 32-byte address.
    let id: Address = unsafe { *(data as *const Address) };
    let exists = STORE.with(|s| {
        s.insert(
            id,
            Child {
                parent: *parent,
                desc: d,
                data,
            },
        )
        .is_some()
    });
    if exists {
        move_rt_abort(E_FIELD_ALREADY_EXISTS);
    }
}

/// `borrow_child_object{,_mut}<Child>(&parent_uid, id)`: address of the stored child.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_df_borrow(parent: &Address, id: &Address, d: &TypeDesc) -> *mut u8 {
    find(parent, id, d)
}

/// `remove_child_object<Child>(parent, id)`: moves the child out into `out`.
///
/// # Safety
/// `out` is valid for writes of a value of type `d`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_df_remove(
    parent: &Address,
    id: &Address,
    d: &TypeDesc,
    out: *mut u8,
) {
    let data = find(parent, id, d);
    // SAFETY: the stored child is `d.size` initialized bytes; `out` has room for them.
    unsafe { ptr::copy_nonoverlapping(data, out, d.size() as usize) };
    STORE.with(|s| s.remove(id));
}

/// `has_child_object(parent, id)` (`d` null) / `has_child_object_with_ty<Child>` (`d` set).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_df_has(parent: &Address, id: &Address, d: *const TypeDesc) -> bool {
    STORE.with(|s| {
        s.get(id)
            .is_some_and(|c| c.parent == *parent && (d.is_null() || ptr::eq(c.desc, d)))
    })
}

// ---- shared objects (for `sui::test_scenario`)

// Abort code of `sui::test_scenario` when an object is not available.
const E_OBJECT_NOT_FOUND: u64 = 3;

struct Shared {
    desc: *const TypeDesc,
    data: *mut u8,
}

/// Shared objects not currently taken, and the IDs of those taken.
static SHARED: Global<(BTreeMap<Address, Shared>, BTreeMap<Address, ()>)> =
    Global::new((BTreeMap::new(), BTreeMap::new()));

/// Store a just-shared object (moved bitwise; its ID is its leading `UID`).
///
/// # Safety
/// `p` points to an initialized object of type `d`.
pub(crate) unsafe fn put_shared(d: &TypeDesc, p: *const u8) {
    let size = d.size() as usize;
    let data = desc::alloc(size);
    // SAFETY: `p` holds `size` initialized bytes; `data` is a fresh allocation of `size`.
    unsafe { ptr::copy_nonoverlapping(p, data, size) };
    // SAFETY: objects start with `id: UID`, i.e. their 32-byte address.
    let id: Address = unsafe { *(data as *const Address) };
    SHARED.with(|(available, taken)| {
        taken.remove(&id);
        available.insert(id, Shared { desc: d, data });
    });
}

/// `test_scenario::take_shared_by_id<T>(id)`: moves the shared object into `out`.
///
/// # Safety
/// `out` is valid for writes of a value of type `d`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_take_shared(id: &Address, d: &TypeDesc, out: *mut u8) {
    let obj = SHARED.with(|(available, taken)| {
        let obj = available.remove(id).filter(|o| ptr::eq(o.desc, d))?;
        taken.insert(*id, ());
        Some(obj)
    });
    let Some(obj) = obj else {
        move_rt_abort(E_OBJECT_NOT_FOUND)
    };
    // SAFETY: the stored object is `d.size` initialized bytes; `out` has room for them.
    unsafe { ptr::copy_nonoverlapping(obj.data, out, d.size() as usize) };
}

/// `test_scenario::was_taken_shared(id)`.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_was_taken_shared(id: &Address) -> bool {
    SHARED.with(|(_, taken)| taken.contains_key(id))
}
