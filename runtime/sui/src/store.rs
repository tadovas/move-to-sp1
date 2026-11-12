//! Objects that outlive the Move call that created them: child objects of dynamic fields
//! (`sui::dynamic_field` natives) and shared objects (`sui::test_scenario` take/return).
//!
//! On chain these are separate objects owned by their parent; here they live in an
//! in-memory map for the duration of the run, keyed by child ID. Values are moved in and
//! out bitwise (Move semantics); heap data they reference is never freed, so it stays valid.

use alloc::{collections::BTreeMap, vec::Vec};
use core::ptr;

use move_rt_core::{
    abort_in, bcs,
    desc::{self, Kind, Slot, TypeDesc, Value},
    global::Global,
};

use crate::{
    Address,
    location::{DYNAMIC_FIELD, TEST_SCENARIO},
};

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
        Some((true, false, _)) => abort_in(DYNAMIC_FIELD, E_FIELD_TYPE_MISMATCH),
        _ => abort_in(DYNAMIC_FIELD, E_FIELD_DOES_NOT_EXIST),
    }
}

/// `hash_type_and_key<K>(parent, k)`: the child ID for key `k`, derived exactly as Sui does
/// (`derive_dynamic_field_id`): `Blake2b256(0xF0 || parent || len(bcs(k)) as u64 LE || bcs(k) ||
/// bcs(TypeTag of K))`. The compiler supplies `bcs(TypeTag)` as a constant.
///
/// # Safety
/// `k` points to an initialized value of type `kd`; `tag` points to `tag_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_df_hash(
    parent: &Address,
    kd: &TypeDesc,
    k: *const u8,
    tag: *const u8,
    tag_len: u64,
    out: &mut Address,
) {
    // SAFETY: guaranteed by the caller.
    *out = unsafe { field_id(false, parent, kd, k, tag, tag_len) };
}

/// `--fast`: the same derivation with SHA-256 (SP1 precompile) instead of Blake2b.
///
/// # Safety
/// As for `move_rt_sui_df_hash`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_df_hash_fast(
    parent: &Address,
    kd: &TypeDesc,
    k: *const u8,
    tag: *const u8,
    tag_len: u64,
    out: &mut Address,
) {
    // SAFETY: guaranteed by the caller.
    *out = unsafe { field_id(true, parent, kd, k, tag, tag_len) };
}

/// # Safety
/// `k` points to an initialized value of type `kd`; `tag` points to `tag_len` bytes.
unsafe fn field_id(
    fast: bool,
    parent: &Address,
    kd: &TypeDesc,
    k: *const u8,
    tag: *const u8,
    tag_len: u64,
) -> Address {
    // SAFETY: guaranteed by the caller.
    let (key, tag) = unsafe {
        (
            Value::new(kd, k),
            core::slice::from_raw_parts(tag, tag_len as usize),
        )
    };
    let mut key_bcs = Vec::new();
    bcs::encode(key, &mut key_bcs);
    let len = (key_bcs.len() as u64).to_le_bytes();
    crate::hash::id_hash(
        fast,
        &[&[crate::hash::CHILD_OBJECT_ID], parent, &len, &key_bcs, tag],
    )
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
        abort_in(DYNAMIC_FIELD, E_FIELD_ALREADY_EXISTS);
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

// ---- object inventory (for `sui::test_scenario`)
//
// Objects transferred to an account or shared during the run are kept here so later
// "transactions" of a test scenario can take them back. As on Sui, objects placed during a
// transaction are pending until it ends (`test_scenario::end_transaction`, called by
// `next_tx` / `end`); only then can they be taken.

// Abort code of `sui::test_scenario` when an object is not available.
const E_OBJECT_NOT_FOUND: u64 = 3;

/// Owner of an inventory object: an account, or `None` for shared.
type Owner = Option<Address>;

struct Owned {
    owner: Owner,
    desc: *const TypeDesc,
    data: *mut u8,
    /// Insertion order, for "most recent".
    seq: u64,
    /// Placed during the current transaction: not takeable until it ends.
    pending: bool,
}

struct Inventory {
    objects: BTreeMap<Address, Owned>,
    /// Objects taken out, by ID, with the owner they were taken from.
    taken: BTreeMap<Address, Owner>,
    next_seq: u64,
}

static INVENTORY: Global<Inventory> = Global::new(Inventory {
    objects: BTreeMap::new(),
    taken: BTreeMap::new(),
    next_seq: 0,
});

/// `owner` argument of the natives below: null for shared objects, else the account.
fn owner_of(account: *const Address) -> Owner {
    // SAFETY: non-null `account` points to a 32-byte address (generated code passes `&address`).
    (!account.is_null()).then(|| unsafe { *account })
}

/// Store a transferred / shared object (moved bitwise; its ID is its leading `UID`).
///
/// # Safety
/// `p` points to an initialized object of type `d`.
pub(crate) unsafe fn put(owner: Owner, d: &TypeDesc, p: *const u8) {
    let size = d.size() as usize;
    let data = desc::alloc(size);
    // SAFETY: `p` holds `size` initialized bytes; `data` is a fresh allocation of `size`.
    unsafe { ptr::copy_nonoverlapping(p, data, size) };
    // SAFETY: objects start with `id: UID`, i.e. their 32-byte address.
    let id: Address = unsafe { *(data as *const Address) };
    INVENTORY.with(|inv| {
        inv.taken.remove(&id);
        let seq = inv.next_seq;
        inv.next_seq += 1;
        inv.objects.insert(
            id,
            Owned {
                owner,
                desc: d,
                data,
                seq,
                pending: true,
            },
        );
    });
}

/// `take_shared_by_id<T>(id)` (`account` null) / `take_from_address_by_id<T>(account, id)`:
/// moves the object into `out`.
///
/// # Safety
/// `out` is valid for writes of a value of type `d`; `account` is null or a valid address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_take(
    account: *const Address,
    id: &Address,
    d: &TypeDesc,
    out: *mut u8,
) {
    let owner = owner_of(account);
    let obj = INVENTORY.with(|inv| {
        let matches = inv
            .objects
            .get(id)
            .is_some_and(|o| !o.pending && o.owner == owner && ptr::eq(o.desc, d));
        let obj = if matches {
            inv.objects.remove(id)
        } else {
            None
        }?;
        inv.taken.insert(*id, owner);
        Some(obj)
    });
    let Some(obj) = obj else {
        abort_in(TEST_SCENARIO, E_OBJECT_NOT_FOUND)
    };
    // SAFETY: the stored object is `d.size` initialized bytes; `out` has room for them.
    unsafe { ptr::copy_nonoverlapping(obj.data, out, d.size() as usize) };
}

/// `was_taken_shared(id)` (`account` null) / `was_taken_from_address(account, id)`.
///
/// # Safety
/// `account` is null or a valid address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_was_taken(account: *const Address, id: &Address) -> bool {
    let owner = owner_of(account);
    INVENTORY.with(|inv| inv.taken.get(id) == Some(&owner))
}

/// `most_recent_id_shared<T>()` / `most_recent_id_for_address<T>(account)`: writes an
/// `Option<ID>` (described by `opt`) to `out`.
///
/// # Safety
/// `out` is valid for writes of an `Option<ID>`; `account` is null or a valid address.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn move_rt_sui_most_recent(
    account: *const Address,
    d: &TypeDesc,
    opt: &TypeDesc,
    out: *mut u8,
) {
    let owner = owner_of(account);
    let found = INVENTORY.with(|inv| {
        inv.objects
            .iter()
            .filter(|(_, o)| !o.pending && o.owner == owner && ptr::eq(o.desc, d))
            .max_by_key(|(_, o)| o.seq)
            .map(|(id, _)| *id)
    });
    // Option<ID> = { vec: vector<ID> }, ID = { bytes: address }
    // SAFETY: guaranteed by the caller.
    let slot = unsafe { Slot::new(opt, out) };
    let vec = slot.fields().next().expect("Option.vec");
    for mut id_slot in vec.init_vector(found.is_some() as usize) {
        while id_slot.desc().kind() == Kind::Struct {
            id_slot = id_slot.fields().next().expect("single-field wrapper");
        }
        id_slot.write_prim(&found.unwrap());
    }
}

/// `test_scenario::end_transaction`: objects placed during the transaction become available,
/// and the record of objects taken in it is cleared.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_end_transaction() {
    INVENTORY.with(|inv| {
        inv.objects.values_mut().for_each(|o| o.pending = false);
        inv.taken.clear();
    });
}

// ---- state snapshots (see `state.rs`)

/// Where a persistent object lives.
#[derive(Clone, Copy)]
pub(crate) enum Placement {
    /// Owned by an account.
    Owned(Address),
    Shared,
    /// A dynamic-field child of this parent.
    Child(Address),
}

/// Visit every persistent object: placement, ID, type and value.
pub(crate) fn for_each_stored(
    mut f: impl FnMut(Placement, &Address, &'static TypeDesc, *const u8),
) {
    // SAFETY (both loops): stored descriptors point at the program's constant descriptors.
    INVENTORY.with(|inv| {
        for (id, o) in &inv.objects {
            let placement = o.owner.map_or(Placement::Shared, Placement::Owned);
            f(placement, id, unsafe { &*o.desc }, o.data);
        }
    });
    STORE.with(|s| {
        for (id, c) in s {
            f(Placement::Child(c.parent), id, unsafe { &*c.desc }, c.data);
        }
    });
}

/// Insert an object loaded from an input snapshot (`data` holds a value of type `d`).
pub(crate) fn insert_stored(
    placement: Placement,
    id: Address,
    d: &'static TypeDesc,
    data: *mut u8,
) {
    match placement {
        Placement::Child(parent) => STORE.with(|s| {
            s.insert(
                id,
                Child {
                    parent,
                    desc: d,
                    data,
                },
            );
        }),
        Placement::Owned(_) | Placement::Shared => INVENTORY.with(|inv| {
            let owner = match placement {
                Placement::Owned(a) => Some(a),
                _ => None,
            };
            let seq = inv.next_seq;
            inv.next_seq += 1;
            inv.objects.insert(
                id,
                Owned {
                    owner,
                    desc: d,
                    data,
                    seq,
                    pending: false,
                },
            );
        }),
    }
}

/// Owners of the owned objects lent to the entry function (checked against the sender).
static LENT_OWNED: Global<Vec<Address>> = Global::new(Vec::new());

/// An object of the input state, lent to an entry function's `&T` / `&mut T` parameter:
/// mutations happen in place, so they persist into the output state.
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_borrow_object(id: &Address, d: &TypeDesc) -> *mut u8 {
    let found = INVENTORY.with(|inv| {
        inv.objects
            .get(id)
            .filter(|o| ptr::eq(o.desc, d))
            .map(|o| (o.data, o.owner))
    });
    let Some((data, owner)) = found else {
        abort_in(crate::location::STATE, crate::state::E_OBJECT_NOT_IN_STATE)
    };
    if let Some(owner) = owner {
        LENT_OWNED.with(|l| l.push(owner));
    }
    data
}

/// Called by the entry glue once all arguments are read: as on Sui, owned objects can only
/// be used by their owner (shared objects by anyone).
#[unsafe(no_mangle)]
pub extern "C" fn move_rt_sui_check_inputs() {
    let sender = crate::tx::sender();
    if LENT_OWNED.with(|l| l.iter().any(|owner| *owner != sender)) {
        abort_in(crate::location::STATE, crate::state::E_NOT_OWNER);
    }
}
