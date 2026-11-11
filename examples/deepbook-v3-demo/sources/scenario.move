/// Drives DeepBook v3 (Sui's current central limit order book) through a small market, using
/// `sui::test_scenario` for shared objects across transactions, like DeepBook's own tests:
/// create a whitelisted pool (no DEEP fees), a maker posts asks and bids, a taker sweeps the
/// asks with a market buy. Funds are checked for conservation before returning.
module demo::scenario_v3;

use deepbook::balance_manager;
use deepbook::pool::{Self, Pool};
use deepbook::registry::{Self, Registry};
use sui::clock;
use sui::coin;
use sui::test_scenario;
use std::unit_test;

public struct BASE has drop {}

public struct QUOTE has drop {}

const OWNER: address = @0xA11CE;
const FLOAT: u64 = 1_000_000_000;
const TICK: u64 = 1_000;
const LOT: u64 = 1_000;
const MIN_SIZE: u64 = 1_000;
const NO_RESTRICTION: u8 = 0;
const SELF_MATCHING_ALLOWED: u8 = 0;
const NEVER: u64 = 18_446_744_073_709_551_615;

const EFundsNotConserved: u64 = 100;

public struct Outcome has drop {
    taker_base_bought: u64,
    taker_quote_paid: u64,
    best_bid: Option<u64>,
    best_ask: Option<u64>,
    mid_price: u64,
    maker_base: u64,
    maker_quote: u64,
    taker_base: u64,
    taker_quote: u64,
}

#[allow(deprecated_usage)] // the clock test helpers keep the demo self-contained
public fun run(timestamp_ms: u64): Outcome {
    let mut sc = test_scenario::begin(OWNER);
    let mut clock = clock::create_for_testing(sc.ctx());
    clock.set_for_testing(timestamp_ms);

    // tx 1: registry + whitelisted pool
    let registry_id = registry::test_registry(sc.ctx());
    let admin = registry::get_admin_cap_for_testing(sc.ctx());
    sc.next_tx(OWNER);
    let mut registry = sc.take_shared_by_id<Registry>(registry_id);
    let pool_id = pool::create_pool_admin<BASE, QUOTE>(
        &mut registry, TICK, LOT, MIN_SIZE, true, false, &admin, sc.ctx(),
    );
    test_scenario::return_shared(registry);

    // tx 2: funded balance managers for maker and taker
    sc.next_tx(OWNER);
    let mut pool = sc.take_shared_by_id<Pool<BASE, QUOTE>>(pool_id);
    let mut maker = balance_manager::new(sc.ctx());
    let mut taker = balance_manager::new(sc.ctx());
    maker.deposit(coin::mint_for_testing<BASE>(10_000_000, sc.ctx()), sc.ctx());
    maker.deposit(coin::mint_for_testing<QUOTE>(10_000_000, sc.ctx()), sc.ctx());
    taker.deposit(coin::mint_for_testing<QUOTE>(10_000_000, sc.ctx()), sc.ctx());

    // Maker: asks at 1.00 / 1.01 / 1.02, bids at 0.99 / 0.98.
    let (prices, sizes, sides) = (
        vector[100, 101, 102, 99, 98], // cents
        vector[2_000_000, 3_000_000, 5_000_000, 4_000_000, 4_000_000],
        vector[false, false, false, true, true], // is_bid
    );
    prices.length().do!(|i| {
        let proof = maker.generate_proof_as_owner(sc.ctx());
        pool.place_limit_order(
            &mut maker, &proof, i + 1, NO_RESTRICTION, SELF_MATCHING_ALLOWED,
            prices[i] * (FLOAT / 100), sizes[i], sides[i], false, NEVER, &clock, sc.ctx(),
        );
    });

    // Taker: market buy 4_000_000 BASE.
    let proof = taker.generate_proof_as_owner(sc.ctx());
    let quote_before = taker.balance<QUOTE>();
    pool.place_market_order(&mut taker, &proof, 42, SELF_MATCHING_ALLOWED, 4_000_000, true, false, &clock, sc.ctx());

    let (maker_base, maker_quote) = (maker.balance<BASE>(), maker.balance<QUOTE>());
    let (taker_base, taker_quote) = (taker.balance<BASE>(), taker.balance<QUOTE>());
    let (vault_base, vault_quote, _) = pool.vault_balances();
    // Conservation: all minted funds are in the managers or locked in the pool's vault.
    assert!(maker_base + taker_base + vault_base == 10_000_000, EFundsNotConserved);
    assert!(maker_quote + taker_quote + vault_quote == 20_000_000, EFundsNotConserved);

    let outcome = Outcome {
        taker_base_bought: taker_base,
        taker_quote_paid: quote_before - taker_quote,
        best_bid: pool.best_bid_price(&clock),
        best_ask: pool.best_ask_price(&clock),
        mid_price: pool.mid_price(&clock),
        maker_base,
        maker_quote,
        taker_base,
        taker_quote,
    };

    test_scenario::return_shared(pool);
    unit_test::destroy(maker);
    unit_test::destroy(taker);
    unit_test::destroy(admin);
    clock.destroy_for_testing();
    sc.end();
    outcome
}
