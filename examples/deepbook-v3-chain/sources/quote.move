/// The QUOTE test coin: created at publish time with its one-time witness, as any Sui coin.
module demo::quote;

use sui::coin;

public struct QUOTE has drop {}

#[allow(deprecated_usage)]
fun init(otw: QUOTE, ctx: &mut TxContext) {
    let (treasury, metadata) = coin::create_currency(otw, 6, b"QUOTE", b"QUOTE", b"", option::none(), ctx);
    transfer::public_transfer(treasury, ctx.sender());
    transfer::public_freeze_object(metadata);
}
