/// The BASE test coin: created at publish time with its one-time witness, as any Sui coin.
module demo::base;

use sui::coin;

public struct BASE has drop {}

#[allow(deprecated_usage)]
fun init(otw: BASE, ctx: &mut TxContext) {
    let (treasury, metadata) = coin::create_currency(otw, 6, b"BASE", b"BASE", b"", option::none(), ctx);
    transfer::public_transfer(treasury, ctx.sender());
    transfer::public_freeze_object(metadata);
}
