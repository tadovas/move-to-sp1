#!/usr/bin/env python3
"""Reference model of examples/amm.move, for cross-checking the compiled program.

Usage: amm_model.py '<pool json>' '<actions json>'   -> prints the expected JSON result
       amm_model.py --gen N                          -> prints a pool and N deterministic
                                                        pseudo-random actions (two JSON args)
"""
import hashlib, json, math, struct, sys

class Abort(Exception):
    pass

def mul_div(a, b, c):
    return a * b // c

def quote(amount_in, r_in, r_out, fee_bps):
    if amount_in == 0: raise Abort(1)
    if r_in == 0 or r_out == 0: raise Abort(2)
    in_after_fee = amount_in * (10_000 - fee_bps)
    out = in_after_fee * r_out // (r_in * 10_000 + in_after_fee)
    return out, mul_div(amount_in, fee_bps, 10_000)

def apply(pool, actions):
    if pool["fee_bps"] >= 10_000: raise Abort(4)
    pre = hashlib.sha256(struct.pack("<4Q", pool["reserve_a"], pool["reserve_b"],
                                     pool["lp_supply"], pool["fee_bps"])).hexdigest()
    p = dict(pool)
    receipts = []
    for action in actions:
        (kind, a), = action.items()
        if kind in ("SwapAForB", "SwapBForA"):
            r_in, r_out = ("reserve_a", "reserve_b") if kind == "SwapAForB" else ("reserve_b", "reserve_a")
            out, fee = quote(a["amount_in"], p[r_in], p[r_out], p["fee_bps"])
            if out < a["min_out"]: raise Abort(3)
            p[r_in] += a["amount_in"]; p[r_out] -= out
            receipts.append({"Swapped": {"amount_in": a["amount_in"], "amount_out": out, "fee": fee}})
        elif kind == "AddLiquidity":
            x, y = a["amount_a"], a["amount_b"]
            if x == 0 or y == 0: raise Abort(1)
            if p["lp_supply"] == 0:
                ux, uy, minted = x, y, math.isqrt(x * y)
            else:
                minted = min(mul_div(x, p["lp_supply"], p["reserve_a"]), mul_div(y, p["lp_supply"], p["reserve_b"]))
                ux = mul_div(minted, p["reserve_a"], p["lp_supply"])
                uy = mul_div(minted, p["reserve_b"], p["lp_supply"])
            if minted == 0: raise Abort(2)
            p["reserve_a"] += ux; p["reserve_b"] += uy; p["lp_supply"] += minted
            receipts.append({"LiquidityAdded": {"amount_a": ux, "amount_b": uy, "lp_minted": minted}})
        elif kind == "RemoveLiquidity":
            lp = a["lp"]
            if not (0 < lp <= p["lp_supply"]): raise Abort(2)
            ox, oy = mul_div(lp, p["reserve_a"], p["lp_supply"]), mul_div(lp, p["reserve_b"], p["lp_supply"])
            p["reserve_a"] -= ox; p["reserve_b"] -= oy; p["lp_supply"] -= lp
            receipts.append({"LiquidityRemoved": {"amount_a": ox, "amount_b": oy}})
    return ["0x" + pre, p, receipts]

def gen(n):
    """A pool and `n` mixed actions that never abort (seeded, reproducible)."""
    import random
    rng = random.Random(n)
    pool = {"reserve_a": 50_000_000, "reserve_b": 20_000_000, "lp_supply": 31_622_776, "fee_bps": 30}
    acts = []
    for _ in range(n):
        r = rng.random()
        if r < 0.4:
            acts.append({"SwapAForB": {"amount_in": rng.randint(1_000, 100_000), "min_out": 0}})
        elif r < 0.8:
            acts.append({"SwapBForA": {"amount_in": rng.randint(1_000, 40_000), "min_out": 0}})
        elif r < 0.9:
            acts.append({"AddLiquidity": {"amount_a": rng.randint(10_000, 500_000), "amount_b": rng.randint(4_000, 200_000)}})
        else:
            acts.append({"RemoveLiquidity": {"lp": rng.randint(1_000, 50_000)}})
    return pool, acts

if __name__ == "__main__":
    if sys.argv[1] == "--gen":
        pool, acts = gen(int(sys.argv[2]))
        print(json.dumps(pool, separators=(",", ":")), json.dumps(acts, separators=(",", ":")))
        sys.exit()
    try:
        print(json.dumps(apply(json.loads(sys.argv[1]), json.loads(sys.argv[2])), separators=(",", ":")))
    except Abort as e:
        print(f"abort:Move abort with code {e.args[0]}")
