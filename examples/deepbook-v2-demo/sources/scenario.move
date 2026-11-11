/// Drives DeepBook v2 (Sui's on-chain central limit order book, mainnet-v1.35.2) through a
/// small market inside one transaction: create a pool, a maker posts asks and bids, a taker
/// sweeps the asks with a market buy. Funds are checked for conservation before returning.
module demo::scenario;

use deepbook::clob_v2;
use deepbook::custodian_v2::AccountCap;
use sui::balance::{Self, Supply};
use sui::clock::Clock;
use sui::coin::{Self, Coin};
use sui::sui::SUI;

/// Test assets (any `drop` type can back a `Supply`).
public struct BASE has drop {}

public struct QUOTE has drop {}

const FLOAT: u64 = 1_000_000_000; // DeepBook's fixed-point scaling
const TICK: u64 = 10_000_000; // 0.01
const LOT: u64 = 1_000;
const NO_RESTRICTION: u8 = 0;
const CANCEL_OLDEST: u8 = 0;
const NEVER: u64 = 18_446_744_073_709_551_615;

const EFundsNotConserved: u64 = 100;

public struct Outcome has drop {
    /// What the taker's market buy got and paid.
    taker_base_bought: u64,
    taker_quote_paid: u64,
    /// Book after the trade.
    best_bid: Option<u64>,
    best_ask: Option<u64>,
    ask_prices: vector<u64>,
    ask_depths: vector<u64>,
    /// Maker's custodian balances (available, locked).
    maker_base: vector<u64>,
    maker_quote: vector<u64>,
}

public fun run(
    creation_fee: Coin<SUI>,
    clock: Clock,
    mut ctx: TxContext,
): (Outcome, Supply<BASE>, Supply<QUOTE>, Clock) {
    let ctx = &mut ctx;
    let mut base = balance::create_supply(BASE {});
    let mut quote = balance::create_supply(QUOTE {});

    let mut pool = clob_v2::create_pool_with_return<BASE, QUOTE>(TICK, LOT, creation_fee, ctx);
    let maker = clob_v2::create_account(ctx);
    let taker = clob_v2::create_account(ctx);

    // Maker: asks at 1.00 / 1.01 / 1.02, bids at 0.99 / 0.98.
    clob_v2::deposit_base(&mut pool, mint(&mut base, 10_000, ctx), &maker);
    clob_v2::deposit_quote(&mut pool, mint(&mut quote, 10_000, ctx), &maker);
    let (prices, sizes, sides) = (
        vector[100, 101, 102, 99, 98], // in cents
        vector[2_000, 3_000, 5_000, 4_000, 4_000],
        vector[false, false, false, true, true], // is_bid
    );
    prices.length().do!(|i| place(&mut pool, i + 1, prices[i], sizes[i], sides[i], &clock, &maker, ctx));

    // Taker: market buy 4_000 BASE, paying with 10_000 QUOTE.
    let quote_in = 10_000;
    let (base_out, quote_left) = clob_v2::place_market_order(
        &mut pool, &taker, 42, 4_000, true,
        coin::zero(ctx), mint(&mut quote, quote_in, ctx), &clock, ctx,
    );
    let taker_base_bought = base_out.value();
    let taker_quote_paid = quote_in - quote_left.value();

    let (bid, ask) = clob_v2::get_market_price(&pool);
    let (ask_prices, ask_depths) = clob_v2::get_level2_book_status_ask_side(&pool, 0, NEVER, &clock);
    let (mb_avail, mb_locked, mq_avail, mq_locked) = clob_v2::account_balance(&pool, &maker);

    // Conservation: every minted unit is with the taker, in the maker's account or in the
    // pool's fee vault.
    let fees = clob_v2::quote_asset_trading_fees_value(&pool);
    assert!(taker_base_bought + mb_avail + mb_locked == base.supply_value(), EFundsNotConserved);
    assert!(quote_left.value() + mq_avail + mq_locked + fees == quote.supply_value(), EFundsNotConserved);

    let me = ctx.sender();
    transfer::public_transfer(base_out, me);
    transfer::public_transfer(quote_left, me);
    transfer::public_transfer(maker, me);
    transfer::public_transfer(taker, me);
    transfer::public_share_object(pool);

    let outcome = Outcome {
        taker_base_bought,
        taker_quote_paid,
        best_bid: bid,
        best_ask: ask,
        ask_prices,
        ask_depths,
        maker_base: vector[mb_avail, mb_locked],
        maker_quote: vector[mq_avail, mq_locked],
    };
    (outcome, base, quote, clock)
}

fun mint<T>(supply: &mut Supply<T>, amount: u64, ctx: &mut TxContext): Coin<T> {
    coin::from_balance(supply.increase_supply(amount), ctx)
}

fun place(
    pool: &mut clob_v2::Pool<BASE, QUOTE>,
    id: u64,
    cents: u64,
    qty: u64,
    is_bid: bool,
    clock: &Clock,
    account: &AccountCap,
    ctx: &mut TxContext,
) {
    let price = cents * (FLOAT / 100);
    clob_v2::place_limit_order(pool, id, price, qty, CANCEL_OLDEST, is_bid, NEVER, NO_RESTRICTION, clock, account, ctx);
}
