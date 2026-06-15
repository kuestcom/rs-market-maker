<h1 align="center">
  <img src="https://github.com/user-attachments/assets/0cc687fb-89c4-43fa-a056-d89c307215ad" alt="Kuest" height="96" /><br/>
  Kuest Rust Market Maker Example
</h1>

## What It Does

- Finds active, tradable markets from the fork site and records newly seen
  market ids in `state/seen-markets.json`.
- Computes configurable buy/sell quotes per selected outcome token. It defaults
  to buy-only because sell orders require existing outcome-token inventory.
- Posts GTC limit orders only when `--live` is set. Dry-run is the default.

The quoting strategy is intentionally simple: estimate fair value from the book
midpoint, improve the visible top of book by one tick when possible, and keep a
configurable edge away from fair value so it does not cross just to trade.

## Project Layout

```text
src/
  main.rs        CLI bootstrap
  config.rs      args, env vars, validation
  bot.rs         discovery/quote cycle orchestration
  discovery.rs   market pagination, filtering, seen/new selection
  orders.rs      auth, quote planning, cancel/create/post order flow
  pricing.rs     fair price and quote math
  state.rs       seen-market persistence
tests/
  unit/          external unit tests
```

## Dry Run

```bash
cargo run
```

## Live Trading

Live mode requires explicit signing configuration:

```bash
KUEST_PRIVATE_KEY=0x... \
KUEST_DEPOSIT_WALLET=0x... \
KUEST_CHAIN_ID=137 \
cargo run -- --live
```

Use `KUEST_CHAIN_ID=80002` for Amoy when that is the target chain.

By default live mode only posts buy orders:

```bash
MARKET_MAKER_QUOTE_SIDES=buy cargo run -- --live
```

Use sell-side quoting only when the deposit wallet already owns outcome tokens
for the market:

```bash
MARKET_MAKER_QUOTE_SIDES=both cargo run -- --live
```

If a sell order returns `position balance 0 below required 5000000`, the wallet
has zero balance for that outcome token and the order size is 5 shares
(`5 * 10^6` base units).

## CLI args / env vars

```md

  --clob-host / KUEST_CLOB_HOST
  Default: https://clob.kuest.com
  The CLOB API endpoint. Necessary because market discovery, order books,
  auth, signing metadata, and order posting all go through the CLOB API.
  Keep configurable for prod/staging/forks.

  --live / MARKET_MAKER_LIVE
  Default: false
  Safety switch. Without it, the bot only prints intended quotes. Necessary
  because this bot can place real orders.

  --private-key / KUEST_PRIVATE_KEY
  Required only with --live.
  Wallet private key used by the SDK to authenticate and sign orders.
  Necessary because CLOB orders still need client-side signatures.

  --deposit-wallet / KUEST_DEPOSIT_WALLET
  Required only with --live.
  The deposit wallet/funder address whose balances are used. Necessary
  because Kuest’s order flow uses deposit-wallet signature type, and the
  exchange checks this wallet’s USDC/outcome-token balances.

  --chain-id / KUEST_CHAIN_ID
  Required only with --live.
  Allowed: 137 Polygon, 80002 Amoy. Necessary so signatures are made for
  the correct chain and verifying contracts.

  --discovery / MARKET_MAKER_DISCOVERY
  Default: auto. Values: auto, sampling, site.
  Controls where markets come from. sampling prefers reward/sampling
  markets, site uses broader fork-site active markets, auto tries sampling
  first then falls back. Necessary because “new markets from the fork site”
  and “markets worth quoting” are not always the same set.

  --max-markets / MARKET_MAKER_MAX_MARKETS
  Default: 3.
  Maximum markets to quote per cycle. Necessary risk control: every market
  can produce multiple token quotes and real capital exposure.

  --max-pages / MARKET_MAKER_MAX_PAGES
  Default: 5.
  How many paginated market pages to scan. Necessary to cap API work and
  avoid sweeping the whole venue every cycle.

  --order-size / MARKET_MAKER_ORDER_SIZE
  Default: 5.
  Share size per order. Necessary because every order needs a size. For
  buys, this controls exposure; for sells, it requires that many shares of
  that outcome token.

  --edge-ticks / MARKET_MAKER_EDGE_TICKS
  Default: 1.
  Minimum distance from estimated fair value, in ticks. Necessary so the
  bot does not quote exactly at fair or cross into negative edge just to
  get filled.

  --min-spread-ticks / MARKET_MAKER_MIN_SPREAD_TICKS
  Default: 2.
  Minimum spread between the bot’s buy and sell quotes, in ticks. Necessary
  to avoid placing a too-tight two-sided market.

  --quote-sides / MARKET_MAKER_QUOTE_SIDES
  Default: buy. Values: buy, sell, both.
  Controls whether the bot places bids, asks, or both. Necessary because
  buys need USDC, while sells need existing outcome-token inventory. Fresh
  wallets should use buy.

  --allow-single-sided / MARKET_MAKER_ALLOW_SINGLE_SIDED
  Default: true.
  Allows quoting only one side if the other side is unsafe or disabled.
  Necessary because many books/edge settings produce only one valid side.

  --respect-reward-min-size / MARKET_MAKER_RESPECT_REWARD_MIN_SIZE
  Default: false.
  If true, order size is raised to the market reward minimum size.
  Necessary only if you are trying to satisfy reward/scoring constraints;
  otherwise it can unexpectedly increase exposure.

  --cancel-before-quote / MARKET_MAKER_CANCEL_BEFORE_QUOTE
  Default: true.
  Cancels your existing orders for the token before posting fresh quotes.
  Necessary to avoid stacking duplicate stale orders on the same token.

  --post-only / MARKET_MAKER_POST_ONLY
  Default: true.
  Tells the CLOB to reject orders that would immediately take liquidity.
  Necessary for a market-maker posture: rest orders, do not cross.

  --discover-only / MARKET_MAKER_DISCOVER_ONLY
  Default: false.
  Only prints discovered markets, no book reads or quotes. Necessary for
  debugging market selection safely.

  --cycles / MARKET_MAKER_CYCLES
  Default: 1.
  Number of discovery/quote loops to run. Necessary to choose between one-
  shot testing and repeated quoting.

  --refresh-secs / MARKET_MAKER_REFRESH_SECS
  Default: 30.
  Sleep between cycles. Necessary when --cycles > 1, so the bot does not
  hammer APIs or churn orders too fast.
```
