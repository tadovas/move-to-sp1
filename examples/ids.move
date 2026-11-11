/// Object and dynamic-field IDs, derived as on Sui.
module demo::ids;

use sui::derived_object;

/// The IDs of the first `n` objects created in a transaction.
public fun fresh_ids(mut ctx: TxContext, n: u64): vector<address> {
    let mut ids = vector[];
    n.do!(|_| {
        let uid = object::new(&mut ctx);
        ids.push_back(uid.to_address());
        uid.delete();
    });
    ids
}

/// `derived_object::derive_address` = dynamic-field ID of `DerivedObjectKey<u64>(key)` under `parent`.
public fun field_id(parent: ID, key: u64): address {
    derived_object::derive_address(parent, key)
}
