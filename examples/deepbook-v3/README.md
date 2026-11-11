DeepBook v3 — Sui's current central limit order book. `deepbook/` and `token/` are copied
verbatim from `MystenLabs/deepbookv3` (`packages/{deepbook,token}`) at the commit in
`SOURCE_COMMIT` (Apache-2.0).

The demo in `../deepbook-v3-demo` is ours. It is built with `--test-mode` because it uses
test-only *setup* helpers — `coin::mint_for_testing`, `registry::test_registry`, and
`sui::test_scenario` to take shared objects between transactions — while the trading path
(balance managers, order placement, matching, settlement) is DeepBook's production code.
