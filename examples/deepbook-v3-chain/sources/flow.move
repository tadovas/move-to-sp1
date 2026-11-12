/// DeepBook v3 driven as real transactions, one SP1 run each, chained through state:
///
///   publish      -> Registry (shared), DeepbookAdminCap, TreasuryCap<BASE/QUOTE>   (`init`s)
///   create_pool  -> a whitelisted BASE/QUOTE Pool (shared)
///   make         -> the maker's BalanceManager with asks 1.00/1.01/1.02 and bids 0.99/0.98
///   take         -> the taker's market buy of 4_000_000 BASE
///
/// Production code only: no test helpers, coins are minted through their `TreasuryCap`s.
module demo::flow;

use deepbook::balance_manager::{Self, BalanceManager};
use deepbook::pool::{Self, Pool};
use deepbook::registry::{DeepbookAdminCap, Registry};
use demo::base::BASE;
use demo::quote::QUOTE;
use sui::clock::Clock;
use sui::coin::TreasuryCap;

const FLOAT: u64 = 1_000_000_000;
const NO_RESTRICTION: u8 = 0;
const SELF_MATCHING_ALLOWED: u8 = 0;
const NEVER: u64 = 18_446_744_073_709_551_615;

public struct Outcome has drop {
    taker_base_bought: u64,
    taker_quote_paid: u64,
    best_bid: Option<u64>,
    best_ask: Option<u64>,
    mid_price: u64,
}

/// Admin: create a whitelisted (fee-less) BASE/QUOTE pool; it is shared.
public fun create_pool(registry: &mut Registry, admin: &DeepbookAdminCap, ctx: &mut TxContext) {
    pool::create_pool_admin<BASE, QUOTE>(registry, 1_000, 1_000, 1_000, true, false, admin, ctx);
}

/// Maker: fund a balance manager and post three asks and two bids.
public fun make(
    pool: &mut Pool<BASE, QUOTE>,
    base: &mut TreasuryCap<BASE>,
    quote: &mut TreasuryCap<QUOTE>,
    clock: &Clock,
    ctx: &mut TxContext,
) {
    let mut maker = balance_manager::new(ctx);
    maker.deposit(base.mint(10_000_000, ctx), ctx);
    maker.deposit(quote.mint(10_000_000, ctx), ctx);
    let (prices, sizes, sides) = (
        vector[100, 101, 102, 99, 98], // cents
        vector[2_000_000, 3_000_000, 5_000_000, 4_000_000, 4_000_000],
        vector[false, false, false, true, true], // is_bid
    );
    prices.length().do!(|i| {
        let proof = maker.generate_proof_as_owner(ctx);
        pool.place_limit_order(
            &mut maker, &proof, i + 1, NO_RESTRICTION, SELF_MATCHING_ALLOWED,
            prices[i] * (FLOAT / 100), sizes[i], sides[i], false, NEVER, clock, ctx,
        );
    });
    transfer::public_transfer(maker, ctx.sender());
}

/// Taker: fund a balance manager and market-buy 4_000_000 BASE.
public fun take(
    pool: &mut Pool<BASE, QUOTE>,
    quote: &mut TreasuryCap<QUOTE>,
    clock: &Clock,
    ctx: &mut TxContext,
): Outcome {
    let mut taker = balance_manager::new(ctx);
    taker.deposit(quote.mint(10_000_000, ctx), ctx);
    let proof = taker.generate_proof_as_owner(ctx);
    pool.place_market_order(&mut taker, &proof, 42, SELF_MATCHING_ALLOWED, 4_000_000, true, false, clock, ctx);
    let outcome = Outcome {
        taker_base_bought: taker.balance<BASE>(),
        taker_quote_paid: 10_000_000 - taker.balance<QUOTE>(),
        best_bid: pool.best_bid_price(clock),
        best_ask: pool.best_ask_price(clock),
        mid_price: pool.mid_price(clock),
    };
    transfer::public_transfer(taker, ctx.sender());
    outcome
}
