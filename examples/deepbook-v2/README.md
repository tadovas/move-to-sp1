DeepBook v2 — Sui's on-chain central limit order book — as it ran on Sui mainnet:
`sources/` is copied verbatim from Sui `crates/sui-framework/packages/deepbook/sources` at
tag `mainnet-v1.35.2` (Apache-2.0), the last release before its matching engine was removed
in favour of DeepBook v3.

The demo in `../deepbook-v2-demo` is ours: it drives the order book through one transaction (create pool,
maker posts asks/bids, taker sweeps with a market buy) and checks conservation of funds.
