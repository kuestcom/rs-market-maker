<h1 align="center">
  <img src="https://github.com/user-attachments/assets/0cc687fb-89c4-43fa-a056-d89c307215ad" alt="Kuest" height="96" /><br/>
  Kuest Rust Market Maker Example
</h1>

## What It Does

- Finds active, tradable markets from the fork site and records newly seen
  market ids in `state/seen-markets.json`.
- Can be pinned to a single event slug so it keeps trading only that event's
  markets.
- Computes configurable buy/sell quotes per selected outcome token. It defaults
  to buy-only because sell orders require existing outcome-token inventory.
- Posts GTC limit orders only when `--live` is set. Dry-run is the default.

The quoting strategy is intentionally simple: estimate fair value from the book
midpoint, then maintain configured quote-size bands around that fair value. The
bot cancels orders outside the active band, trims excess size above the band
maximum, and only tops up when open size falls below the band minimum.

## Dry Run

```bash
cargo run
```

To keep the bot scoped to one event, pass the event slug:

```bash
cargo run -- --event-slug lowest-temperature-in-nyc-on-june-24-2026
```

Event mode reads `.sdk/site-config.json`, uses the configured Kuest site API,
and resolves the event's CLOB condition ids from that fork.

## Live Trading

Start live mode with:

```bash
cargo run -- --live
```

Live mode requires `KUEST_PRIVATE_KEY`, `KUEST_DEPOSIT_WALLET`, and
`KUEST_CHAIN_ID`. You can also pass them as `--private-key`,
`--deposit-wallet`, and `--chain-id`. Use chain id `137` for Polygon or `80002`
for Amoy.

By default live mode only posts buy orders.

Use sell-side quoting only when the deposit wallet already owns outcome tokens
for the market:

```bash
MARKET_MAKER_QUOTE_SIDES=both cargo run -- --live
```

If a sell order returns `position balance 0 below required 5000000`, the wallet
has zero balance for that outcome token and the order size is 5 shares
(`5 * 10^6` base units).

Live mode runs a preflight risk audit on the selected market scope before
quoting. It fetches current books, balances, and open orders; skips the cycle
if those inputs cannot be fetched or are stale; and stops before quoting if
current exposure already breaches configured risk caps. It subtracts collateral
already locked by live buy orders, checks sell orders against available
outcome-token balance, respects configured collateral caps, and blocks quotes
whose simulated fill would exceed the configured market loss cap.
By default, it also requires a two-sided book with acceptable spread and
top-of-book depth before quoting. After cancel requests, live mode refreshes
open orders before posting replacements; after post responses, it only counts
accepted orders as pending local exposure. It also skips live posts when order
books, balances, or open orders are older than the configured data-age limit.
Buy-side sizing is inventory-aware: balances, live open orders, and pending
orders are counted before adding more long exposure to an outcome or market.
When current state already breaches inventory, market-loss, or collateral caps,
the bot skips new quotes and can optionally cancel resting buy orders. It can
also write a pause file so later cycles or restarts stop before discovery.

To cancel scoped live orders without quoting, run:

```bash
cargo run -- --live --cancel-all
```

To cancel scoped live orders when the process receives Ctrl-C or SIGTERM, run:

```bash
cargo run -- --live --cancel-all-on-exit --cycles 1000
```

Both commands only target orders in the currently configured scope. With
`--event-slug`, that scope is the event's selected markets; otherwise it is the
normal discovery selection.

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

  --event-slug / MARKET_MAKER_EVENT_SLUG
  Optional.
  If set, the bot ignores normal discovery and keeps trading only the markets
  under this event slug. It resolves markets from the Kuest fork configured in
  .sdk/site-config.json.

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
  Default share size per order and fallback band size. Necessary because every
  order needs a size. For buys, this controls exposure; for sells, it requires
  that many shares of that outcome token.

  --edge-ticks / MARKET_MAKER_EDGE_TICKS
  Default: 1.
  Minimum distance from estimated fair value, in ticks. Necessary so the
  bot does not quote exactly at fair or cross into negative edge just to
  get filled.

  --min-spread-ticks / MARKET_MAKER_MIN_SPREAD_TICKS
  Default: 2.
  Minimum spread between the bot’s buy and sell quotes, in ticks. Necessary
  to avoid placing a too-tight two-sided market.

  --band-min-margin-ticks / MARKET_MAKER_BAND_MIN_MARGIN_TICKS
  Optional. Default: --edge-ticks.
  Inner band edge, in ticks away from fair. Existing orders closer than this
  are canceled because they no longer have enough edge.

  --band-avg-margin-ticks / MARKET_MAKER_BAND_AVG_MARGIN_TICKS
  Optional. Default: band min margin.
  Price level used for new top-up orders inside the band.

  --band-max-margin-ticks / MARKET_MAKER_BAND_MAX_MARGIN_TICKS
  Optional. Default: band min margin plus --min-spread-ticks.
  Outer band edge, in ticks away from fair. Existing orders beyond this are
  canceled because they are no longer part of the intended quote band.

  --band-min-size / MARKET_MAKER_BAND_MIN_SIZE
  Optional. Default: --order-size.
  Minimum total open size allowed inside the active side band before topping up.

  --band-avg-size / MARKET_MAKER_BAND_AVG_SIZE
  Optional. Default: max(--order-size, band min size).
  Target total open size after a top-up or excess cancellation pass.

  --band-max-size / MARKET_MAKER_BAND_MAX_SIZE
  Optional. Default: max(band avg size, band min size).
  Maximum total open size allowed inside the active side band before trimming.

  --max-book-spread-ticks / MARKET_MAKER_MAX_BOOK_SPREAD_TICKS
  Default: 20.
  In live mode, when --require-two-sided-live is enabled, skip tokens when
  best ask minus best bid is wider than this many ticks. Necessary because
  midpoint fair value is unreliable in wide books.

  --max-pre-post-move-ticks / MARKET_MAKER_MAX_PRE_POST_MOVE_TICKS
  Default: 2.
  Live-mode posting guard. Immediately before posting new orders, refresh the
  token book and skip the post if fair value moved by more than this many
  ticks from the planned fair value.

  --min-top-depth / MARKET_MAKER_MIN_TOP_DEPTH
  Default: 5.
  In live mode, when --require-two-sided-live is enabled, skip tokens unless
  both best bid and best ask have at least this much size at the top level.
  Necessary because tiny top levels can make the visible midpoint too easy
  to manipulate.

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

  --cancel-all / MARKET_MAKER_CANCEL_ALL
  Default: false.
  Live-only one-shot command. Discovers the configured market scope, cancels
  open orders for its outcome tokens, waits briefly for them to clear, then
  exits. Necessary for emergency cleanup without posting new quotes.

  --cancel-all-on-exit / MARKET_MAKER_CANCEL_ALL_ON_EXIT
  Default: false.
  Live-only shutdown guard. On Ctrl-C or SIGTERM, cancels open orders for the
  markets currently managed by this process and verifies whether any remain.
  Necessary when you do not want interrupted runs to leave stale GTC orders.

  --cancel-on-risk-breach / MARKET_MAKER_CANCEL_ON_RISK_BREACH
  Default: false.
  Live-only circuit-breaker action. When current state already breaches
  inventory, market-loss, or market-collateral caps, skip new quotes and cancel
  resting buy orders for the breached token.

  --pause-on-risk-breach / MARKET_MAKER_PAUSE_ON_RISK_BREACH
  Default: false.
  Live-only circuit-breaker action. When current state already breaches a risk
  cap, write the pause file and stop quoting further markets. Necessary when a
  breached run should stay stopped across later cycles or process restarts.

  --clear-pause / MARKET_MAKER_CLEAR_PAUSE
  Default: false.
  One-shot command. Removes the pause file and exits without connecting to the
  CLOB API. Necessary after a manual risk review decides the bot may resume.

  --pause-path / MARKET_MAKER_PAUSE_PATH
  Default: state/paused.json.
  Path to the persisted pause file checked before each cycle and after live
  quote attempts.

  --post-only / MARKET_MAKER_POST_ONLY
  Default: true.
  Tells the CLOB to reject orders that would immediately take liquidity.
  Necessary for a market-maker posture: rest orders, do not cross.

  --require-two-sided-live / MARKET_MAKER_REQUIRE_TWO_SIDED_LIVE
  Default: true.
  In live mode, skip tokens without a reliable bid and ask. Necessary because
  fallback prices like 0.5 are not safe enough for real money.

  --min-price / MARKET_MAKER_MIN_PRICE
  Default: 0.05.
  Lower bound for posted quote prices. Necessary to avoid extreme tail prices
  where one bad fill can dominate the small edge.

  --max-price / MARKET_MAKER_MAX_PRICE
  Default: 0.95.
  Upper bound for posted quote prices. Same risk control as --min-price.

  --max-collateral-per-market / MARKET_MAKER_MAX_COLLATERAL_PER_MARKET
  Default: 25.
  Maximum collateral exposure counted for one market in a cycle.

  --max-loss-per-market / MARKET_MAKER_MAX_LOSS_PER_MARKET
  Default: 25.
  Maximum simulated worst-case market loss allowed after existing balances,
  open orders, and the proposed new order are counted. Necessary because
  collateral caps alone do not account for cross-outcome inventory. Existing
  balances are marked at current fair value because fill history is not tracked.

  --max-inventory-per-token / MARKET_MAKER_MAX_INVENTORY_PER_TOKEN
  Default: 25.
  Maximum long outcome-token inventory allowed after balances, live open
  orders, and pending orders are counted. Buy orders are capped or skipped when
  this limit leaves too little room.

  --max-inventory-per-market / MARKET_MAKER_MAX_INVENTORY_PER_MARKET
  Default: 50.
  Maximum total long inventory across a market's outcome tokens. Necessary to
  stop the bot from accumulating too many shares in one market even when each
  individual token is under its own cap.

  --max-total-collateral / MARKET_MAKER_MAX_TOTAL_COLLATERAL
  Default: 50.
  Maximum collateral exposure counted across all markets in a cycle.

  --min-free-collateral / MARKET_MAKER_MIN_FREE_COLLATERAL
  Default: 1.
  Collateral buffer left unused after subtracting open buy orders.

  --max-data-age-secs / MARKET_MAKER_MAX_DATA_AGE_SECS
  Default: 10.
  Live-mode freshness limit for order books, open orders, token balances, and
  collateral balance. Necessary because stale inputs can produce duplicate or
  mis-sized quotes.

  --max-open-orders-per-token / MARKET_MAKER_MAX_OPEN_ORDERS_PER_TOKEN
  Default: 2.
  Caps live open orders per token after reconciliation.

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
