/// A constant-product AMM (x * y = k) as a provable state transition.
///
/// `apply` takes the pool state and a batch of actions (like a rollup block), and returns
/// the hash of the input state, the new state and one receipt per action. Run in SP1, the
/// proof attests: "these actions, applied to the pool with this hash, yield this pool".
module demo::amm;

use std::bcs;
use std::hash;

const BPS: u128 = 10_000;

const EZeroAmount: u64 = 1;
const EInsufficientLiquidity: u64 = 2;
const ESlippage: u64 = 3;
const EInvalidFee: u64 = 4;

public struct Pool has copy, drop {
    reserve_a: u64,
    reserve_b: u64,
    lp_supply: u64,
    fee_bps: u64,
}

public enum Action has copy, drop {
    SwapAForB { amount_in: u64, min_out: u64 },
    SwapBForA { amount_in: u64, min_out: u64 },
    AddLiquidity { amount_a: u64, amount_b: u64 },
    RemoveLiquidity { lp: u64 },
}

public enum Receipt has copy, drop {
    Swapped { amount_in: u64, amount_out: u64, fee: u64 },
    LiquidityAdded { amount_a: u64, amount_b: u64, lp_minted: u64 },
    LiquidityRemoved { amount_a: u64, amount_b: u64 },
}

/// Apply `actions` in order. Returns (sha2-256 of the BCS-encoded input pool, new pool, receipts).
public fun apply(pool: Pool, actions: vector<Action>): (vector<u8>, Pool, vector<Receipt>) {
    assert!(pool.fee_bps < 10_000, EInvalidFee);
    let pre_state = hash::sha2_256(bcs::to_bytes(&pool));
    let mut pool = pool;
    let mut receipts = vector[];
    actions.do!(|action| receipts.push_back(step(&mut pool, action)));
    (pre_state, pool, receipts)
}

fun step(pool: &mut Pool, action: Action): Receipt {
    match (action) {
        Action::SwapAForB { amount_in, min_out } => {
            let (out, fee) = quote(amount_in, pool.reserve_a, pool.reserve_b, pool.fee_bps);
            assert!(out >= min_out, ESlippage);
            pool.reserve_a = pool.reserve_a + amount_in;
            pool.reserve_b = pool.reserve_b - out;
            Receipt::Swapped { amount_in, amount_out: out, fee }
        },
        Action::SwapBForA { amount_in, min_out } => {
            let (out, fee) = quote(amount_in, pool.reserve_b, pool.reserve_a, pool.fee_bps);
            assert!(out >= min_out, ESlippage);
            pool.reserve_b = pool.reserve_b + amount_in;
            pool.reserve_a = pool.reserve_a - out;
            Receipt::Swapped { amount_in, amount_out: out, fee }
        },
        Action::AddLiquidity { amount_a, amount_b } => add_liquidity(pool, amount_a, amount_b),
        Action::RemoveLiquidity { lp } => {
            assert!(lp > 0 && lp <= pool.lp_supply, EInsufficientLiquidity);
            let amount_a = mul_div(lp, pool.reserve_a, pool.lp_supply);
            let amount_b = mul_div(lp, pool.reserve_b, pool.lp_supply);
            pool.reserve_a = pool.reserve_a - amount_a;
            pool.reserve_b = pool.reserve_b - amount_b;
            pool.lp_supply = pool.lp_supply - lp;
            Receipt::LiquidityRemoved { amount_a, amount_b }
        },
    }
}

/// Output amount and fee for swapping `amount_in` against reserves (in, out).
fun quote(amount_in: u64, reserve_in: u64, reserve_out: u64, fee_bps: u64): (u64, u64) {
    assert!(amount_in > 0, EZeroAmount);
    assert!(reserve_in > 0 && reserve_out > 0, EInsufficientLiquidity);
    let in_after_fee = (amount_in as u128) * (BPS - (fee_bps as u128));
    let out = in_after_fee * (reserve_out as u128) / ((reserve_in as u128) * BPS + in_after_fee);
    let fee = mul_div(amount_in, fee_bps, 10_000);
    ((out as u64), fee)
}

/// Deposit at the current price ratio; the first deposit mints sqrt(a * b) LP tokens.
fun add_liquidity(pool: &mut Pool, amount_a: u64, amount_b: u64): Receipt {
    assert!(amount_a > 0 && amount_b > 0, EZeroAmount);
    let (used_a, used_b, minted) = if (pool.lp_supply == 0) {
        (amount_a, amount_b, ((amount_a as u128) * (amount_b as u128)).sqrt() as u64)
    } else {
        let lp_a = mul_div(amount_a, pool.lp_supply, pool.reserve_a);
        let lp_b = mul_div(amount_b, pool.lp_supply, pool.reserve_b);
        let minted = lp_a.min(lp_b);
        (
            mul_div(minted, pool.reserve_a, pool.lp_supply),
            mul_div(minted, pool.reserve_b, pool.lp_supply),
            minted,
        )
    };
    assert!(minted > 0, EInsufficientLiquidity);
    pool.reserve_a = pool.reserve_a + used_a;
    pool.reserve_b = pool.reserve_b + used_b;
    pool.lp_supply = pool.lp_supply + minted;
    Receipt::LiquidityAdded { amount_a: used_a, amount_b: used_b, lp_minted: minted }
}

/// a * b / c without intermediate overflow.
fun mul_div(a: u64, b: u64, c: u64): u64 {
    ((a as u128) * (b as u128) / (c as u128)) as u64
}
