use std::collections::BTreeSet;
use std::str::FromStr as _;
use std::time::{Duration, Instant};

use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::{Context as _, Result};
use kuest_client_sdk::auth::Signer as _;
use kuest_client_sdk::clob::types::request::{
    BalanceAllowanceRequest, OrderBookSummaryRequest, OrdersRequest,
};
use kuest_client_sdk::clob::types::response::{
    MarketResponse, OpenOrderResponse, OrderBookSummaryResponse, OrderSummary, PostOrderResponse,
    Token,
};
use kuest_client_sdk::clob::types::{
    AssetType, OrderStatusType, OrderType, Side, SignatureType, SignedOrder,
};
use kuest_client_sdk::clob::{Client, Config};
use kuest_client_sdk::types::{Address, Decimal, U256};
use tokio::time::sleep;

use crate::config::Cli;
use crate::discovery::market_key;
use crate::pricing::{best_ask, best_bid, fair_price, max_decimal, min_decimal};
use crate::state::PauseState;
use crate::{AuthClient, PublicClient};

const TERMINAL_CURSOR: &str = "LTE=";
const CANCEL_ORDER_BATCH_SIZE: usize = 50;
const CANCEL_VERIFY_ATTEMPTS: usize = 5;
const CANCEL_VERIFY_DELAY_SECS: u64 = 2;

pub(crate) struct LiveTrading {
    client: AuthClient,
    signer: PrivateKeySigner,
}

#[derive(Debug, Default)]
pub(crate) struct CancelOpenOrdersSummary {
    pub(crate) markets_checked: usize,
    pub(crate) tokens_checked: usize,
    pub(crate) orders_found: usize,
    pub(crate) canceled: usize,
    pub(crate) not_canceled: usize,
    pub(crate) remaining_open: usize,
}

pub(crate) struct RiskBudget {
    remaining_collateral: Decimal,
    counted_open_buy_orders: BTreeSet<String>,
}

impl RiskBudget {
    pub(crate) fn new(collateral_limit: Decimal) -> Self {
        Self {
            remaining_collateral: max_decimal(collateral_limit, Decimal::ZERO),
            counted_open_buy_orders: BTreeSet::new(),
        }
    }

    fn remaining_collateral(&self) -> Decimal {
        max_decimal(self.remaining_collateral, Decimal::ZERO)
    }

    fn reserve_open_buy_order(&mut self, order: &OpenOrderResponse) {
        if order.side != Side::Buy || !self.counted_open_buy_orders.insert(order.id.clone()) {
            return;
        }

        let locked = open_order_remaining_size(order) * order.price;
        self.remaining_collateral = max_decimal(self.remaining_collateral - locked, Decimal::ZERO);
    }

    fn reserve_new_collateral(&mut self, requested: Decimal) -> Decimal {
        let reserved = min_decimal(requested, self.remaining_collateral());
        self.remaining_collateral =
            max_decimal(self.remaining_collateral - reserved, Decimal::ZERO);
        reserved
    }
}

#[derive(Clone, Debug)]
struct QuotePlan {
    market_key: String,
    market_slug: String,
    question: String,
    token_id: U256,
    outcome: String,
    fair_price: Decimal,
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
    book_fetched_at: Instant,
    buy_band: Option<QuoteBand>,
    sell_band: Option<QuoteBand>,
}

#[derive(Clone, Debug)]
struct QuoteBand {
    side: Side,
    price: Decimal,
    min_price: Decimal,
    max_price: Decimal,
    minimum_size: Decimal,
    min_size: Decimal,
    avg_size: Decimal,
    max_size: Decimal,
}

#[derive(Clone, Debug)]
struct TokenQuote {
    token_id: U256,
    fair_price: Decimal,
    plan: Option<QuotePlan>,
    skip_reason: Option<String>,
}

#[derive(Debug)]
struct LiveMarketState {
    tokens: Vec<LiveTokenState>,
    pending_orders: Vec<ProposedOrder>,
}

#[derive(Debug)]
struct LiveTokenState {
    token_id: U256,
    fair_price: Decimal,
    balance: Decimal,
    balance_fetched_at: Instant,
    open_orders: Vec<OpenOrderResponse>,
    open_orders_fetched_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct ProposedOrder {
    token_id: U256,
    side: Side,
    price: Decimal,
    size: Decimal,
}

#[derive(Debug)]
struct PlannedOrder {
    side: Side,
    proposed_order: ProposedOrder,
    signed_order: SignedOrder,
    collateral: Decimal,
}

#[derive(Clone, Copy, Debug)]
struct SubmittedOrder {
    side: Side,
    proposed_order: ProposedOrder,
    collateral: Decimal,
}

#[derive(Clone, Debug)]
struct MarketExposure {
    outcomes: Vec<OutcomeExposure>,
}

#[derive(Clone, Debug)]
struct OutcomeExposure {
    token_id: U256,
    position: Decimal,
    cost: Decimal,
    proceeds: Decimal,
}

#[derive(Debug, PartialEq)]
enum BuyTopUpDecision {
    Place { size: Decimal, collateral: Decimal },
    Skip { affordable_size: Decimal },
}

#[derive(Debug, PartialEq)]
struct InventoryBuyRoom {
    token_position: Decimal,
    market_inventory: Decimal,
    room: Decimal,
}

#[derive(Debug, PartialEq)]
enum RiskBreach {
    TokenInventory {
        token_id: U256,
        inventory: Decimal,
        limit: Decimal,
    },
    MarketInventory {
        inventory: Decimal,
        limit: Decimal,
    },
    MarketLoss {
        loss: Decimal,
        limit: Decimal,
    },
    MarketCollateral {
        collateral: Decimal,
        limit: Decimal,
    },
}

#[derive(Debug, PartialEq)]
enum LiquidityRejectReason {
    MissingTwoSidedBook,
    InvalidTick,
    SpreadTooWide {
        spread_ticks: Decimal,
        max_spread_ticks: u32,
    },
    BidDepthTooLow {
        depth: Decimal,
        min_depth: Decimal,
    },
    AskDepthTooLow {
        depth: Decimal,
        min_depth: Decimal,
    },
}

pub(crate) async fn authenticate(cli: &Cli) -> Result<LiveTrading> {
    let private_key = cli
        .private_key
        .as_deref()
        .context("missing private key for live trading")?;
    let deposit_wallet = cli
        .deposit_wallet
        .as_deref()
        .context("missing deposit wallet for live trading")?
        .parse::<Address>()
        .context("invalid deposit wallet address")?;
    let chain_id = cli.chain_id.context("missing chain id for live trading")?;
    let signer = LocalSigner::from_str(private_key)
        .context("invalid private key")?
        .with_chain_id(Some(chain_id));

    let config = Config::builder().use_server_time(true).build();
    let client = Client::new(&cli.clob_host, config)?
        .authentication_builder(&signer)
        .signature_type(SignatureType::DepositWallet)
        .funder(deposit_wallet)
        .authenticate()
        .await
        .context("failed to authenticate CLOB client")?;

    Ok(LiveTrading { client, signer })
}

pub(crate) async fn quote_market(
    public_client: &PublicClient,
    live: Option<&LiveTrading>,
    market: &MarketResponse,
    cli: &Cli,
    global_budget: &mut RiskBudget,
) -> Result<()> {
    let mut market_budget = RiskBudget::new(cli.max_collateral_per_market);
    let mut token_quotes = Vec::new();

    for token in &market.tokens {
        let request = OrderBookSummaryRequest::builder()
            .token_id(token.token_id)
            .build();
        let book = public_client
            .order_book(&request)
            .await
            .with_context(|| format!("failed to fetch order book for token {}", token.token_id))?;
        let book_fetched_at = Instant::now();
        let token_quote = build_token_quote(market, token, &book, book_fetched_at, cli);

        if let Some(plan) = &token_quote.plan {
            print_plan(plan, cli.live);
        } else {
            println!(
                "skip {} {}: {}",
                market.market_slug,
                token.outcome,
                token_quote
                    .skip_reason
                    .as_deref()
                    .unwrap_or("no safe quote at configured edge/sides")
            );
        }

        token_quotes.push(token_quote);
    }

    if let Some(live) = live {
        let mut market_state = LiveMarketState::load(live, &token_quotes).await?;
        for token_quote in &token_quotes {
            if let Some(plan) = &token_quote.plan {
                if PauseState::load(&cli.pause_path)?.is_some() {
                    return Ok(());
                }
                reconcile_quote_plan(
                    live,
                    plan,
                    cli,
                    global_budget,
                    &mut market_budget,
                    &mut market_state,
                )
                .await?;
                if PauseState::load(&cli.pause_path)?.is_some() {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

fn build_token_quote(
    market: &MarketResponse,
    token: &Token,
    book: &OrderBookSummaryResponse,
    book_fetched_at: Instant,
    cli: &Cli,
) -> TokenQuote {
    let fair_price = fair_price(
        best_bid(&book.bids),
        best_ask(&book.asks),
        token.price,
        book.last_trade_price,
    );
    let liquidity_skip = if should_enforce_liquidity_quality(cli) {
        liquidity_quality_reject_reason(book, cli)
            .map(|reason| format!("liquidity quality check failed: {}", reason.message()))
    } else {
        None
    };
    let plan = if liquidity_skip.is_none() {
        build_quote_plan(market, token, book, book_fetched_at, cli)
    } else {
        None
    };
    let skip_reason = if plan.is_none() {
        liquidity_skip.or_else(|| Some("no safe quote at configured edge/sides".to_owned()))
    } else {
        None
    };

    TokenQuote {
        token_id: token.token_id,
        fair_price,
        plan,
        skip_reason,
    }
}

fn build_quote_plan(
    market: &MarketResponse,
    token: &Token,
    book: &OrderBookSummaryResponse,
    book_fetched_at: Instant,
    cli: &Cli,
) -> Option<QuotePlan> {
    let best_bid = best_bid(&book.bids);
    let best_ask = best_ask(&book.asks);
    if cli.live
        && cli.require_two_sided_live
        && !matches!((best_bid, best_ask), (Some(bid), Some(ask)) if bid > Decimal::ZERO && ask > bid)
    {
        return None;
    }

    let fair_price = fair_price(best_bid, best_ask, token.price, book.last_trade_price);
    let tick = book.tick_size.as_decimal();
    let mut buy_band = cli
        .quote_sides
        .includes_buy()
        .then(|| build_quote_band(market, Side::Buy, fair_price, best_bid, best_ask, tick, cli))
        .flatten();
    let mut sell_band = cli
        .quote_sides
        .includes_sell()
        .then(|| {
            build_quote_band(
                market,
                Side::Sell,
                fair_price,
                best_bid,
                best_ask,
                tick,
                cli,
            )
        })
        .flatten();

    if !cli.allow_single_sided && (buy_band.is_none() || sell_band.is_none()) {
        return None;
    }

    if let (Some(buy), Some(sell)) = (&buy_band, &sell_band)
        && sell.price - buy.price < tick * Decimal::from(cli.min_spread_ticks)
    {
        buy_band = None;
        sell_band = None;
    }

    if buy_band.is_none() && sell_band.is_none() {
        return None;
    }

    Some(QuotePlan {
        market_key: market_key(market),
        market_slug: market.market_slug.clone(),
        question: market.question.clone(),
        token_id: token.token_id,
        outcome: token.outcome.clone(),
        fair_price,
        best_bid,
        best_ask,
        book_fetched_at,
        buy_band,
        sell_band,
    })
}

fn build_quote_band(
    market: &MarketResponse,
    side: Side,
    fair_price: Decimal,
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
    tick: Decimal,
    cli: &Cli,
) -> Option<QuoteBand> {
    if tick <= Decimal::ZERO {
        return None;
    }

    let (min_margin_ticks, avg_margin_ticks, max_margin_ticks) = cli.band_margin_ticks();
    let (min_size, avg_size, max_size) = cli.band_sizes();
    let min_margin = tick * Decimal::from(min_margin_ticks);
    let avg_margin = tick * Decimal::from(avg_margin_ticks);
    let max_margin = tick * Decimal::from(max_margin_ticks);

    let (price, min_price, max_price) = match side {
        Side::Buy => (
            floor_to_tick(fair_price - avg_margin, tick),
            floor_to_tick(fair_price - max_margin, tick),
            floor_to_tick(fair_price - min_margin, tick),
        ),
        Side::Sell => (
            ceil_to_tick(fair_price + avg_margin, tick),
            ceil_to_tick(fair_price + min_margin, tick),
            ceil_to_tick(fair_price + max_margin, tick),
        ),
        _ => return None,
    };
    let min_price = max_decimal(max_decimal(min_price, tick), cli.min_price);
    let max_price = min_decimal(min_decimal(max_price, Decimal::ONE - tick), cli.max_price);

    if !is_tradeable_price(price, tick) || !price_in_configured_range(price, cli) {
        return None;
    }
    if min_price > max_price {
        return None;
    }
    if side == Side::Buy && best_ask.is_some_and(|ask| price >= ask) {
        return None;
    }
    if side == Side::Sell && best_bid.is_some_and(|bid| price <= bid) {
        return None;
    }

    Some(QuoteBand {
        side,
        price,
        min_price,
        max_price,
        minimum_size: order_size(market, Decimal::ZERO, cli),
        min_size: order_size(market, min_size, cli),
        avg_size: order_size(market, avg_size, cli),
        max_size: order_size(market, max_size, cli),
    })
}

fn order_size(market: &MarketResponse, requested_size: Decimal, cli: &Cli) -> Decimal {
    let mut size = max_decimal(requested_size, market.minimum_order_size);
    if cli.respect_reward_min_size {
        size = max_decimal(size, market.rewards.min_size);
    }
    size
}

fn price_in_configured_range(price: Decimal, cli: &Cli) -> bool {
    price >= cli.min_price && price <= cli.max_price
}

fn is_tradeable_price(price: Decimal, tick: Decimal) -> bool {
    price >= tick && price <= Decimal::ONE - tick
}

fn floor_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    (price / tick).floor() * tick
}

fn ceil_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    (price / tick).ceil() * tick
}

fn should_enforce_liquidity_quality(cli: &Cli) -> bool {
    cli.live && cli.require_two_sided_live
}

fn liquidity_quality_reject_reason(
    book: &OrderBookSummaryResponse,
    cli: &Cli,
) -> Option<LiquidityRejectReason> {
    liquidity_reject_reason(
        &book.bids,
        &book.asks,
        book.tick_size.as_decimal(),
        cli.max_book_spread_ticks,
        cli.min_top_depth,
    )
}

fn liquidity_reject_reason(
    bids: &[OrderSummary],
    asks: &[OrderSummary],
    tick: Decimal,
    max_spread_ticks: u32,
    min_top_depth: Decimal,
) -> Option<LiquidityRejectReason> {
    if tick <= Decimal::ZERO {
        return Some(LiquidityRejectReason::InvalidTick);
    }

    let (Some(bid), Some(ask)) = (best_bid(bids), best_ask(asks)) else {
        return Some(LiquidityRejectReason::MissingTwoSidedBook);
    };
    if bid <= Decimal::ZERO || ask <= bid {
        return Some(LiquidityRejectReason::MissingTwoSidedBook);
    }

    let spread_ticks = (ask - bid) / tick;
    if spread_ticks > Decimal::from(max_spread_ticks) {
        return Some(LiquidityRejectReason::SpreadTooWide {
            spread_ticks,
            max_spread_ticks,
        });
    }

    let bid_depth = top_depth(bids, bid);
    if bid_depth < min_top_depth {
        return Some(LiquidityRejectReason::BidDepthTooLow {
            depth: bid_depth,
            min_depth: min_top_depth,
        });
    }

    let ask_depth = top_depth(asks, ask);
    if ask_depth < min_top_depth {
        return Some(LiquidityRejectReason::AskDepthTooLow {
            depth: ask_depth,
            min_depth: min_top_depth,
        });
    }

    None
}

fn top_depth(levels: &[OrderSummary], price: Decimal) -> Decimal {
    levels
        .iter()
        .filter(|level| level.price == price)
        .map(|level| level.size)
        .sum()
}

impl LiquidityRejectReason {
    fn message(&self) -> String {
        match self {
            Self::MissingTwoSidedBook => "missing a valid two-sided book".to_owned(),
            Self::InvalidTick => "book tick size is invalid".to_owned(),
            Self::SpreadTooWide {
                spread_ticks,
                max_spread_ticks,
            } => format!("spread is {spread_ticks} ticks above max {max_spread_ticks}"),
            Self::BidDepthTooLow { depth, min_depth } => {
                format!("best bid depth {depth} below minimum {min_depth}")
            }
            Self::AskDepthTooLow { depth, min_depth } => {
                format!("best ask depth {depth} below minimum {min_depth}")
            }
        }
    }
}

fn print_plan(plan: &QuotePlan, live: bool) {
    let mode = if live { "live" } else { "dry-run" };
    println!(
        "{mode}: {} :: {} :: {} :: {} ({}) fair={} bid={:?} ask={:?} buy={} sell={}",
        plan.market_key,
        plan.market_slug,
        plan.question,
        plan.outcome,
        plan.token_id,
        plan.fair_price,
        plan.best_bid,
        plan.best_ask,
        format_band(plan.buy_band.as_ref()),
        format_band(plan.sell_band.as_ref())
    );
}

fn format_band(band: Option<&QuoteBand>) -> String {
    band.map(|band| {
        format!(
            "price={} band=[{}, {}] size={}/{}/{}",
            band.price, band.min_price, band.max_price, band.min_size, band.avg_size, band.max_size
        )
    })
    .unwrap_or_else(|| "none".to_owned())
}

async fn reconcile_quote_plan(
    live: &LiveTrading,
    plan: &QuotePlan,
    cli: &Cli,
    global_budget: &mut RiskBudget,
    market_budget: &mut RiskBudget,
    market_state: &mut LiveMarketState,
) -> Result<()> {
    let mut open_orders = market_state.open_orders(plan.token_id)?.to_vec();
    let orders_to_cancel = cancellable_orders(&open_orders, plan, cli);
    if cli.cancel_before_quote && !orders_to_cancel.is_empty() {
        let order_ids = orders_to_cancel
            .iter()
            .map(|order| order.id.as_str())
            .collect::<Vec<_>>();
        if skip_live_action_if_paused(plan, cli, "canceling stale orders")? {
            return Ok(());
        }
        let response = live
            .client
            .cancel_orders(&order_ids)
            .await
            .with_context(|| {
                format!("failed to cancel stale orders for token {}", plan.token_id)
            })?;
        println!(
            "canceled stale orders for {}: canceled={} not_canceled={}",
            plan.token_id,
            response.canceled.len(),
            response.not_canceled.len()
        );
        if !response.not_canceled.is_empty() {
            println!(
                "skip placing {} {}: some stale orders could not be canceled",
                plan.market_slug, plan.outcome
            );
            return Ok(());
        }

        let canceled_ids = orders_to_cancel
            .iter()
            .map(|order| order.id.clone())
            .collect::<BTreeSet<_>>();
        open_orders = open_orders_for_token(live, plan.token_id).await?;
        let open_orders_fetched_at = Instant::now();
        market_state.replace_open_orders(
            plan.token_id,
            open_orders.clone(),
            open_orders_fetched_at,
        )?;
        if open_orders
            .iter()
            .any(|order| canceled_ids.contains(&order.id))
        {
            println!(
                "skip placing {} {}: canceled order state is still unstable",
                plan.market_slug, plan.outcome
            );
            return Ok(());
        }
    }

    let remaining_orders = open_orders.iter().collect::<Vec<_>>();
    if let Some(reason) = stale_live_data_reason(plan, market_state, None, Instant::now(), cli)? {
        println!(
            "skip placing {} {}: stale live data ({reason})",
            plan.market_slug, plan.outcome
        );
        return Ok(());
    }

    for order in &remaining_orders {
        global_budget.reserve_open_buy_order(order);
        market_budget.reserve_open_buy_order(order);
    }

    let breaches = market_state.risk_breaches(cli);
    if !breaches.is_empty() {
        print_risk_breaches(plan, &breaches);
        if cli.cancel_on_risk_breach
            && let Some(refreshed_orders) =
                cancel_risk_increasing_orders(live, plan, cli, &remaining_orders).await?
        {
            let open_orders_fetched_at = Instant::now();
            market_state.replace_open_orders(
                plan.token_id,
                refreshed_orders,
                open_orders_fetched_at,
            )?;
        }
        if cli.pause_on_risk_breach {
            let reason = risk_breach_pause_reason(plan, &breaches);
            PauseState::save_reason(&cli.pause_path, reason.clone())?;
            println!("wrote pause file {}: {reason}", cli.pause_path.display());
        }
        return Ok(());
    }

    let collateral_balance = collateral_balance(live).await?;
    let collateral_fetched_at = Instant::now();
    if let Some(reason) = stale_live_data_reason(
        plan,
        market_state,
        Some(collateral_fetched_at),
        Instant::now(),
        cli,
    )? {
        println!(
            "skip placing {} {}: stale live data ({reason})",
            plan.market_slug, plan.outcome
        );
        return Ok(());
    }
    let token_balance = market_state.token_balance(plan.token_id)?;
    let locked_collateral = remaining_orders
        .iter()
        .filter(|order| order.side == Side::Buy)
        .map(|order| open_order_remaining_size(order) * order.price)
        .sum::<Decimal>();
    let locked_tokens = remaining_orders
        .iter()
        .filter(|order| order.side == Side::Sell)
        .map(|order| open_order_remaining_size(order))
        .sum::<Decimal>();
    let free_collateral = max_decimal(
        collateral_balance - locked_collateral - cli.min_free_collateral,
        Decimal::ZERO,
    );
    let free_tokens = max_decimal(token_balance - locked_tokens, Decimal::ZERO);

    let kept_order_count = remaining_orders.len();
    let mut new_order_slots = cli
        .max_open_orders_per_token
        .saturating_sub(kept_order_count);

    let mut planned_orders = Vec::new();
    if let Some(band) = &plan.buy_band
        && new_order_slots > 0
    {
        let open_size = band_open_size(&remaining_orders, band);
        if let Some(missing_size) = band_missing_size(band, open_size) {
            let inventory_room =
                market_state.inventory_buy_room(plan.token_id, &planned_orders, cli)?;
            if let Some(inventory_size) =
                inventory_adjusted_buy_size(missing_size, band.minimum_size, inventory_room.room)
            {
                if inventory_size < missing_size {
                    println!(
                        "cap {} {} buy size from {} to {}: token position {} / max {}, market inventory {} / max {}",
                        plan.market_slug,
                        plan.outcome,
                        missing_size,
                        inventory_size,
                        inventory_room.token_position,
                        cli.max_inventory_per_token,
                        inventory_room.market_inventory,
                        cli.max_inventory_per_market
                    );
                }
                match buy_top_up_decision(
                    inventory_size,
                    band.price,
                    free_collateral,
                    global_budget,
                    market_budget,
                ) {
                    BuyTopUpDecision::Place { size, collateral } => {
                        let proposed_order = ProposedOrder {
                            token_id: plan.token_id,
                            side: Side::Buy,
                            price: band.price,
                            size,
                        };
                        if !market_loss_exceeds_cap(
                            plan,
                            proposed_order,
                            market_state,
                            &planned_orders,
                            cli,
                        )? {
                            let order = live
                                .client
                                .limit_order()
                                .token_id(plan.token_id)
                                .side(Side::Buy)
                                .price(band.price)
                                .size(size)
                                .order_type(OrderType::GTC)
                                .post_only(cli.post_only)
                                .build()
                                .await
                                .with_context(|| {
                                    format!("failed to build buy order for token {}", plan.token_id)
                                })?;
                            planned_orders.push(PlannedOrder {
                                side: Side::Buy,
                                proposed_order,
                                signed_order: live.client.sign(&live.signer, order).await?,
                                collateral,
                            });
                            new_order_slots -= 1;
                        }
                    }
                    BuyTopUpDecision::Skip { affordable_size } => {
                        println!(
                            "skip {} {} buy: risk budget/free collateral leaves size {} below band target {}",
                            plan.market_slug, plan.outcome, affordable_size, inventory_size
                        );
                    }
                }
            } else {
                println!(
                    "skip {} {} buy: inventory room {} below minimum order size {} (token position {}, market inventory {})",
                    plan.market_slug,
                    plan.outcome,
                    inventory_room.room,
                    band.minimum_size,
                    inventory_room.token_position,
                    inventory_room.market_inventory
                );
            }
        }
    }

    if let Some(band) = &plan.sell_band
        && new_order_slots > 0
    {
        let open_size = band_open_size(&remaining_orders, band);
        if let Some(missing_size) = band_missing_size(band, open_size) {
            let size = min_decimal(missing_size, free_tokens);
            if size >= missing_size {
                let proposed_order = ProposedOrder {
                    token_id: plan.token_id,
                    side: Side::Sell,
                    price: band.price,
                    size,
                };
                if !market_loss_exceeds_cap(
                    plan,
                    proposed_order,
                    market_state,
                    &planned_orders,
                    cli,
                )? {
                    let order = live
                        .client
                        .limit_order()
                        .token_id(plan.token_id)
                        .side(Side::Sell)
                        .price(band.price)
                        .size(size)
                        .order_type(OrderType::GTC)
                        .post_only(cli.post_only)
                        .build()
                        .await
                        .with_context(|| {
                            format!("failed to build sell order for token {}", plan.token_id)
                        })?;
                    planned_orders.push(PlannedOrder {
                        side: Side::Sell,
                        proposed_order,
                        signed_order: live.client.sign(&live.signer, order).await?,
                        collateral: Decimal::ZERO,
                    });
                }
            } else {
                println!(
                    "skip {} {} sell: free token balance leaves size {} below band target {}",
                    plan.market_slug, plan.outcome, size, missing_size
                );
            }
        }
    }

    if planned_orders.is_empty() {
        return Ok(());
    }

    if let Some(reason) = stale_live_data_reason(
        plan,
        market_state,
        Some(collateral_fetched_at),
        Instant::now(),
        cli,
    )? {
        println!(
            "skip posting {} {}: stale live data ({reason})",
            plan.market_slug, plan.outcome
        );
        return Ok(());
    }

    let mut submitted_orders = Vec::with_capacity(planned_orders.len());
    let mut signed_orders = Vec::with_capacity(planned_orders.len());
    for planned_order in planned_orders {
        submitted_orders.push(SubmittedOrder {
            side: planned_order.side,
            proposed_order: planned_order.proposed_order,
            collateral: planned_order.collateral,
        });
        signed_orders.push(planned_order.signed_order);
    }
    if skip_live_action_if_paused(plan, cli, "posting orders")? {
        return Ok(());
    }
    let responses = live.client.post_orders(signed_orders).await?;
    let responses = submitted_orders
        .into_iter()
        .zip(responses)
        .collect::<Vec<_>>();
    print_post_responses(plan, &responses);
    apply_post_responses(
        live,
        plan,
        responses,
        global_budget,
        market_budget,
        market_state,
    )
    .await?;

    Ok(())
}

async fn open_orders_for_token(
    live: &LiveTrading,
    token_id: U256,
) -> Result<Vec<OpenOrderResponse>> {
    let request = OrdersRequest::builder().asset_id(token_id).build();
    let mut cursor = None;
    let mut orders = Vec::new();

    loop {
        let page = live
            .client
            .orders(&request, cursor.clone())
            .await
            .with_context(|| format!("failed to fetch open orders for token {token_id}"))?;
        let next_cursor = page.next_cursor.clone();
        orders.extend(page.data.into_iter().filter(is_open_order));
        if next_cursor == TERMINAL_CURSOR || cursor.as_deref() == Some(next_cursor.as_str()) {
            break;
        }
        cursor = Some(next_cursor);
    }

    Ok(orders)
}

pub(crate) async fn cancel_open_orders_for_markets(
    live: &LiveTrading,
    markets: &[MarketResponse],
) -> Result<CancelOpenOrdersSummary> {
    let token_ids = managed_token_ids(markets);
    let mut summary = CancelOpenOrdersSummary {
        markets_checked: markets.len(),
        tokens_checked: token_ids.len(),
        ..Default::default()
    };

    for token_id in &token_ids {
        let open_orders = open_orders_for_token(live, *token_id).await?;
        if open_orders.is_empty() {
            continue;
        }

        println!(
            "cancel-all: token {} has {} open orders",
            token_id,
            open_orders.len()
        );
        summary.orders_found += open_orders.len();

        for batch in open_orders.chunks(CANCEL_ORDER_BATCH_SIZE) {
            let order_ids = batch
                .iter()
                .map(|order| order.id.as_str())
                .collect::<Vec<_>>();
            let response = live
                .client
                .cancel_orders(&order_ids)
                .await
                .with_context(|| format!("failed to cancel open orders for token {token_id}"))?;
            summary.canceled += response.canceled.len();
            summary.not_canceled += response.not_canceled.len();

            println!(
                "cancel-all: token {} canceled={} not_canceled={}",
                token_id,
                response.canceled.len(),
                response.not_canceled.len()
            );
        }
    }

    if summary.orders_found > 0 {
        for attempt in 1..=CANCEL_VERIFY_ATTEMPTS {
            summary.remaining_open = remaining_open_orders_for_tokens(live, &token_ids).await?;
            if summary.remaining_open == 0 {
                break;
            }
            if attempt < CANCEL_VERIFY_ATTEMPTS {
                println!(
                    "cancel-all: waiting for {} open orders to clear (attempt {}/{})",
                    summary.remaining_open, attempt, CANCEL_VERIFY_ATTEMPTS
                );
                sleep(Duration::from_secs(CANCEL_VERIFY_DELAY_SECS)).await;
            }
        }
    }

    Ok(summary)
}

fn managed_token_ids(markets: &[MarketResponse]) -> Vec<U256> {
    let mut token_ids = Vec::new();
    for market in markets {
        for token in &market.tokens {
            if !token_ids.contains(&token.token_id) {
                token_ids.push(token.token_id);
            }
        }
    }
    token_ids
}

async fn remaining_open_orders_for_tokens(live: &LiveTrading, token_ids: &[U256]) -> Result<usize> {
    let mut remaining = 0;
    for token_id in token_ids {
        remaining += open_orders_for_token(live, *token_id).await?.len();
    }
    Ok(remaining)
}

impl LiveMarketState {
    async fn load(live: &LiveTrading, token_quotes: &[TokenQuote]) -> Result<Self> {
        let mut tokens = Vec::new();
        for token_quote in token_quotes {
            let balance = conditional_balance(live, token_quote.token_id).await?;
            let balance_fetched_at = Instant::now();
            let open_orders = open_orders_for_token(live, token_quote.token_id).await?;
            let open_orders_fetched_at = Instant::now();
            tokens.push(LiveTokenState {
                token_id: token_quote.token_id,
                fair_price: token_quote.fair_price,
                balance,
                balance_fetched_at,
                open_orders,
                open_orders_fetched_at,
            });
        }

        Ok(Self {
            tokens,
            pending_orders: Vec::new(),
        })
    }

    fn open_orders(&self, token_id: U256) -> Result<&[OpenOrderResponse]> {
        Ok(&self.token_state(token_id)?.open_orders)
    }

    fn token_balance(&self, token_id: U256) -> Result<Decimal> {
        Ok(self.token_state(token_id)?.balance)
    }

    fn replace_open_orders(
        &mut self,
        token_id: U256,
        open_orders: Vec<OpenOrderResponse>,
        fetched_at: Instant,
    ) -> Result<()> {
        let token_state = self.token_state_mut(token_id)?;
        token_state.open_orders = open_orders;
        token_state.open_orders_fetched_at = fetched_at;
        Ok(())
    }

    fn remove_pending_orders_now_open(&mut self, open_orders: &[OpenOrderResponse]) {
        self.pending_orders.retain(|pending_order| {
            !open_orders
                .iter()
                .any(|open_order| open_order_matches_proposed(open_order, *pending_order))
        });
    }

    fn projected_loss_with_staged(
        &self,
        order: ProposedOrder,
        staged_orders: &[PlannedOrder],
    ) -> Result<Decimal> {
        let mut exposure = self.exposure_with_staged(staged_orders)?;
        exposure.apply_order(order)?;
        Ok(exposure.worst_loss())
    }

    fn inventory_buy_room(
        &self,
        token_id: U256,
        staged_orders: &[PlannedOrder],
        cli: &Cli,
    ) -> Result<InventoryBuyRoom> {
        let token_position = self.long_inventory_for_token(token_id, staged_orders)?;
        let market_inventory = self.long_market_inventory(staged_orders);
        let token_room = max_decimal(cli.max_inventory_per_token - token_position, Decimal::ZERO);
        let market_room = max_decimal(
            cli.max_inventory_per_market - market_inventory,
            Decimal::ZERO,
        );

        Ok(InventoryBuyRoom {
            token_position,
            market_inventory,
            room: min_decimal(token_room, market_room),
        })
    }

    fn risk_breaches(&self, cli: &Cli) -> Vec<RiskBreach> {
        let mut breaches = Vec::new();
        for token_state in &self.tokens {
            let inventory = self
                .long_inventory_for_token(token_state.token_id, &[])
                .unwrap_or(Decimal::ZERO);
            if inventory > cli.max_inventory_per_token {
                breaches.push(RiskBreach::TokenInventory {
                    token_id: token_state.token_id,
                    inventory,
                    limit: cli.max_inventory_per_token,
                });
            }
        }

        let market_inventory = self.long_market_inventory(&[]);
        if market_inventory > cli.max_inventory_per_market {
            breaches.push(RiskBreach::MarketInventory {
                inventory: market_inventory,
                limit: cli.max_inventory_per_market,
            });
        }

        let exposure = self.exposure();
        let loss = exposure.worst_loss();
        if loss > cli.max_loss_per_market {
            breaches.push(RiskBreach::MarketLoss {
                loss,
                limit: cli.max_loss_per_market,
            });
        }

        let collateral = exposure.buy_collateral();
        if collateral > cli.max_collateral_per_market {
            breaches.push(RiskBreach::MarketCollateral {
                collateral,
                limit: cli.max_collateral_per_market,
            });
        }

        breaches
    }

    fn record_pending_order(&mut self, order: ProposedOrder) {
        self.pending_orders.push(order);
    }

    fn exposure(&self) -> MarketExposure {
        let mut exposure = MarketExposure {
            outcomes: self
                .tokens
                .iter()
                .map(|token| OutcomeExposure {
                    token_id: token.token_id,
                    position: token.balance,
                    cost: token.balance * token.fair_price,
                    proceeds: Decimal::ZERO,
                })
                .collect(),
        };

        for token in &self.tokens {
            for order in &token.open_orders {
                let order = ProposedOrder {
                    token_id: token.token_id,
                    side: order.side,
                    price: order.price,
                    size: open_order_remaining_size(order),
                };
                exposure.apply_known_order(order);
            }
        }
        for order in &self.pending_orders {
            exposure.apply_known_order(*order);
        }

        exposure
    }

    fn exposure_with_staged(&self, staged_orders: &[PlannedOrder]) -> Result<MarketExposure> {
        let mut exposure = self.exposure();
        for staged_order in staged_orders {
            exposure.apply_order(staged_order.proposed_order)?;
        }
        Ok(exposure)
    }

    fn long_inventory_for_token(
        &self,
        token_id: U256,
        staged_orders: &[PlannedOrder],
    ) -> Result<Decimal> {
        let token_state = self.token_state(token_id)?;
        Ok(token_long_inventory(
            token_state.balance,
            &token_state.open_orders,
            &self.pending_orders,
            staged_orders,
            token_id,
        ))
    }

    fn long_market_inventory(&self, staged_orders: &[PlannedOrder]) -> Decimal {
        self.tokens
            .iter()
            .map(|token_state| {
                token_long_inventory(
                    token_state.balance,
                    &token_state.open_orders,
                    &self.pending_orders,
                    staged_orders,
                    token_state.token_id,
                )
            })
            .sum()
    }

    fn token_state(&self, token_id: U256) -> Result<&LiveTokenState> {
        self.tokens
            .iter()
            .find(|token| token.token_id == token_id)
            .with_context(|| format!("missing live market state for token {token_id}"))
    }

    fn token_state_mut(&mut self, token_id: U256) -> Result<&mut LiveTokenState> {
        self.tokens
            .iter_mut()
            .find(|token| token.token_id == token_id)
            .with_context(|| format!("missing live market state for token {token_id}"))
    }
}

impl MarketExposure {
    fn apply_order(&mut self, order: ProposedOrder) -> Result<()> {
        let outcome = self
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.token_id == order.token_id)
            .with_context(|| format!("missing exposure state for token {}", order.token_id))?;
        apply_order_to_outcome(outcome, order);
        Ok(())
    }

    fn apply_known_order(&mut self, order: ProposedOrder) {
        if let Some(outcome) = self
            .outcomes
            .iter_mut()
            .find(|outcome| outcome.token_id == order.token_id)
        {
            apply_order_to_outcome(outcome, order);
        }
    }

    fn worst_loss(&self) -> Decimal {
        let cost = self
            .outcomes
            .iter()
            .map(|outcome| outcome.cost)
            .sum::<Decimal>();
        let proceeds = self
            .outcomes
            .iter()
            .map(|outcome| outcome.proceeds)
            .sum::<Decimal>();
        let worst_resolution_payout = if self.outcomes.len() > 1 {
            self.outcomes
                .iter()
                .map(|outcome| outcome.position)
                .min_by(|left, right| left.cmp(right))
                .unwrap_or(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };

        max_decimal(cost - proceeds - worst_resolution_payout, Decimal::ZERO)
    }

    fn buy_collateral(&self) -> Decimal {
        self.outcomes
            .iter()
            .map(|outcome| outcome.cost)
            .sum::<Decimal>()
    }
}

fn apply_order_to_outcome(outcome: &mut OutcomeExposure, order: ProposedOrder) {
    match order.side {
        Side::Buy => {
            outcome.position += order.size;
            outcome.cost += order.size * order.price;
        }
        Side::Sell => {
            outcome.position -= order.size;
            outcome.proceeds += order.size * order.price;
        }
        Side::Unknown => {}
        _ => {}
    }
}

fn token_long_inventory(
    balance: Decimal,
    open_orders: &[OpenOrderResponse],
    pending_orders: &[ProposedOrder],
    staged_orders: &[PlannedOrder],
    token_id: U256,
) -> Decimal {
    max_decimal(balance, Decimal::ZERO)
        + buy_order_size_for_token(open_orders, token_id)
        + pending_buy_size_for_token(pending_orders, token_id)
        + staged_buy_size_for_token(staged_orders, token_id)
}

fn buy_order_size_for_token(orders: &[OpenOrderResponse], token_id: U256) -> Decimal {
    orders
        .iter()
        .filter(|order| order.asset_id == token_id && order.side == Side::Buy)
        .map(open_order_remaining_size)
        .sum()
}

fn pending_buy_size_for_token(orders: &[ProposedOrder], token_id: U256) -> Decimal {
    orders
        .iter()
        .filter(|order| order.token_id == token_id && order.side == Side::Buy)
        .map(|order| order.size)
        .sum()
}

fn staged_buy_size_for_token(orders: &[PlannedOrder], token_id: U256) -> Decimal {
    orders
        .iter()
        .map(|order| order.proposed_order)
        .filter(|order| order.token_id == token_id && order.side == Side::Buy)
        .map(|order| order.size)
        .sum()
}

fn print_risk_breaches(plan: &QuotePlan, breaches: &[RiskBreach]) {
    for breach in breaches {
        println!(
            "risk breach {} {}: {}",
            plan.market_slug,
            plan.outcome,
            breach.message()
        );
    }
}

fn risk_breach_pause_reason(plan: &QuotePlan, breaches: &[RiskBreach]) -> String {
    let messages = breaches
        .iter()
        .map(RiskBreach::message)
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "risk breach {} {}: {messages}",
        plan.market_slug, plan.outcome
    )
}

impl RiskBreach {
    fn message(&self) -> String {
        match self {
            Self::TokenInventory {
                token_id,
                inventory,
                limit,
            } => {
                format!("token {token_id} inventory {inventory} exceeds limit {limit}")
            }
            Self::MarketInventory { inventory, limit } => {
                format!("market inventory {inventory} exceeds limit {limit}")
            }
            Self::MarketLoss { loss, limit } => {
                format!("market loss {loss} exceeds limit {limit}")
            }
            Self::MarketCollateral { collateral, limit } => {
                format!("market collateral {collateral} exceeds limit {limit}")
            }
        }
    }
}

async fn cancel_risk_increasing_orders(
    live: &LiveTrading,
    plan: &QuotePlan,
    cli: &Cli,
    open_orders: &[&OpenOrderResponse],
) -> Result<Option<Vec<OpenOrderResponse>>> {
    let order_ids = open_orders
        .iter()
        .filter(|order| order.side == Side::Buy)
        .map(|order| order.id.as_str())
        .collect::<Vec<_>>();
    if order_ids.is_empty() {
        return Ok(None);
    }

    if skip_live_action_if_paused(plan, cli, "canceling risk-increasing orders")? {
        return Ok(None);
    }

    let response = live
        .client
        .cancel_orders(&order_ids)
        .await
        .with_context(|| {
            format!(
                "failed to cancel risk-increasing buy orders for token {}",
                plan.token_id
            )
        })?;
    println!(
        "risk breach cancel {} {}: canceled={} not_canceled={}",
        plan.market_slug,
        plan.outcome,
        response.canceled.len(),
        response.not_canceled.len()
    );

    Ok(Some(open_orders_for_token(live, plan.token_id).await?))
}

fn skip_live_action_if_paused(plan: &QuotePlan, cli: &Cli, action: &str) -> Result<bool> {
    if let Some(pause) = PauseState::load(&cli.pause_path)? {
        println!(
            "skip {action} for {} {}: pause active at {} ({})",
            plan.market_slug,
            plan.outcome,
            cli.pause_path.display(),
            pause.reason.trim()
        );
        return Ok(true);
    }

    Ok(false)
}

fn market_loss_exceeds_cap(
    plan: &QuotePlan,
    proposed_order: ProposedOrder,
    market_state: &LiveMarketState,
    staged_orders: &[PlannedOrder],
    cli: &Cli,
) -> Result<bool> {
    let projected_loss = market_state.projected_loss_with_staged(proposed_order, staged_orders)?;
    if projected_loss <= cli.max_loss_per_market {
        return Ok(false);
    }

    println!(
        "skip {} {} {:?}: projected market loss {} exceeds cap {}",
        plan.market_slug,
        plan.outcome,
        proposed_order.side,
        projected_loss,
        cli.max_loss_per_market
    );
    Ok(true)
}

fn buy_top_up_decision(
    missing_size: Decimal,
    price: Decimal,
    free_collateral: Decimal,
    global_budget: &RiskBudget,
    market_budget: &RiskBudget,
) -> BuyTopUpDecision {
    let affordable_size = affordable_buy_size(
        missing_size,
        price,
        free_collateral,
        global_budget,
        market_budget,
    );
    if affordable_size < missing_size {
        return BuyTopUpDecision::Skip { affordable_size };
    }

    let collateral = missing_size * price;
    BuyTopUpDecision::Place {
        size: missing_size,
        collateral,
    }
}

fn inventory_adjusted_buy_size(
    requested_size: Decimal,
    minimum_size: Decimal,
    inventory_room: Decimal,
) -> Option<Decimal> {
    let size = min_decimal(requested_size, inventory_room);
    (size >= minimum_size).then_some(size)
}

fn reserve_buy_collateral(
    collateral: Decimal,
    global_budget: &mut RiskBudget,
    market_budget: &mut RiskBudget,
) {
    let global_reserved = global_budget.reserve_new_collateral(collateral);
    let market_reserved = market_budget.reserve_new_collateral(global_reserved);
    debug_assert_eq!(global_reserved, collateral);
    debug_assert_eq!(market_reserved, collateral);
}

fn affordable_buy_size(
    missing_size: Decimal,
    price: Decimal,
    free_collateral: Decimal,
    global_budget: &RiskBudget,
    market_budget: &RiskBudget,
) -> Decimal {
    if missing_size <= Decimal::ZERO || price <= Decimal::ZERO {
        return Decimal::ZERO;
    }

    let requested_collateral = missing_size * price;
    let affordable_collateral = [
        requested_collateral,
        free_collateral,
        global_budget.remaining_collateral(),
        market_budget.remaining_collateral(),
    ]
    .into_iter()
    .fold(requested_collateral, min_decimal);

    affordable_collateral / price
}

async fn collateral_balance(live: &LiveTrading) -> Result<Decimal> {
    let request = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();
    let response = live
        .client
        .balance_allowance(request)
        .await
        .context("failed to fetch collateral balance")?;
    Ok(response.balance)
}

async fn conditional_balance(live: &LiveTrading, token_id: U256) -> Result<Decimal> {
    let request = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Conditional)
        .token_id(token_id)
        .build();
    let response = live
        .client
        .balance_allowance(request)
        .await
        .with_context(|| format!("failed to fetch token balance for {token_id}"))?;
    Ok(response.balance)
}

fn cancellable_orders<'a>(
    open_orders: &'a [OpenOrderResponse],
    plan: &QuotePlan,
    cli: &Cli,
) -> Vec<&'a OpenOrderResponse> {
    if !cli.cancel_before_quote {
        return Vec::new();
    }

    let mut cancellable = open_orders
        .iter()
        .filter(|order| order_should_cancel(order, plan))
        .collect::<Vec<_>>();
    let mut cancellable_ids = cancellable
        .iter()
        .map(|order| order.id.as_str())
        .collect::<BTreeSet<_>>();

    for band in plan.bands() {
        let matching_orders = open_orders
            .iter()
            .filter(|order| {
                !cancellable_ids.contains(order.id.as_str()) && band.includes_order(order)
            })
            .collect::<Vec<_>>();
        let matching_size = matching_orders
            .iter()
            .map(|order| open_order_remaining_size(order))
            .sum::<Decimal>();
        if matching_size > band.max_size {
            let mut band_amount = matching_size;
            let mut excessive_orders = matching_orders;
            excessive_orders.sort_by(|left, right| {
                band.cancel_priority(left)
                    .cmp(&band.cancel_priority(right))
                    .then_with(|| left.created_at.cmp(&right.created_at))
            });
            for order in excessive_orders {
                if band_amount <= band.avg_size {
                    break;
                }
                if cancellable_ids.insert(order.id.as_str()) {
                    band_amount = max_decimal(
                        band_amount - open_order_remaining_size(order),
                        Decimal::ZERO,
                    );
                    cancellable.push(order);
                }
            }
        }
    }

    let mut kept = open_orders
        .iter()
        .filter(|order| !cancellable_ids.contains(order.id.as_str()))
        .collect::<Vec<_>>();

    if kept.len() > cli.max_open_orders_per_token {
        kept.sort_by_key(|left| left.created_at);
        for order in kept.into_iter().skip(cli.max_open_orders_per_token) {
            if cancellable_ids.insert(order.id.as_str()) {
                cancellable.push(order);
            }
        }
    }

    cancellable
}

fn order_should_cancel(order: &OpenOrderResponse, plan: &QuotePlan) -> bool {
    if open_order_remaining_size(order) <= Decimal::ZERO {
        return true;
    }

    match order.side {
        Side::Buy => !plan
            .buy_band
            .as_ref()
            .is_some_and(|band| band.includes_order(order)),
        Side::Sell => !plan
            .sell_band
            .as_ref()
            .is_some_and(|band| band.includes_order(order)),
        Side::Unknown => true,
        _ => true,
    }
}

impl QuotePlan {
    fn bands(&self) -> impl Iterator<Item = &QuoteBand> {
        self.buy_band.iter().chain(self.sell_band.iter())
    }
}

impl QuoteBand {
    fn contains_price(&self, price: Decimal) -> bool {
        match self.side {
            Side::Buy | Side::Sell => price >= self.min_price && price <= self.max_price,
            _ => false,
        }
    }

    fn includes_order(&self, order: &OpenOrderResponse) -> bool {
        order.side == self.side && self.contains_price(order.price)
    }

    fn cancel_priority(&self, order: &OpenOrderResponse) -> Decimal {
        match self.side {
            Side::Buy => max_decimal(self.max_price - order.price, Decimal::ZERO),
            Side::Sell => max_decimal(order.price - self.min_price, Decimal::ZERO),
            _ => Decimal::ZERO,
        }
    }
}

fn band_open_size(orders: &[&OpenOrderResponse], band: &QuoteBand) -> Decimal {
    orders
        .iter()
        .filter(|order| band.includes_order(order))
        .map(|order| open_order_remaining_size(order))
        .sum()
}

fn band_missing_size(band: &QuoteBand, open_size: Decimal) -> Option<Decimal> {
    (open_size < band.min_size).then(|| max_decimal(band.avg_size - open_size, Decimal::ZERO))
}

fn open_order_remaining_size(order: &OpenOrderResponse) -> Decimal {
    max_decimal(order.original_size - order.size_matched, Decimal::ZERO)
}

fn is_open_order(order: &OpenOrderResponse) -> bool {
    matches!(
        order.status,
        OrderStatusType::Live | OrderStatusType::Unmatched | OrderStatusType::Delayed
    )
}

async fn apply_post_responses(
    live: &LiveTrading,
    plan: &QuotePlan,
    responses: Vec<(SubmittedOrder, PostOrderResponse)>,
    global_budget: &mut RiskBudget,
    market_budget: &mut RiskBudget,
    market_state: &mut LiveMarketState,
) -> Result<()> {
    let mut has_failed_response = false;
    for (submitted_order, response) in responses {
        has_failed_response |= apply_post_response(
            submitted_order,
            &response,
            global_budget,
            market_budget,
            market_state,
        );
    }

    if has_failed_response {
        let refreshed_orders = open_orders_for_token(live, plan.token_id).await?;
        let open_orders_fetched_at = Instant::now();
        market_state.remove_pending_orders_now_open(&refreshed_orders);
        market_state.replace_open_orders(
            plan.token_id,
            refreshed_orders,
            open_orders_fetched_at,
        )?;
        println!(
            "post state refreshed for {} {} after rejected order response",
            plan.market_slug, plan.outcome
        );
    }

    Ok(())
}

fn apply_post_response(
    submitted_order: SubmittedOrder,
    response: &PostOrderResponse,
    global_budget: &mut RiskBudget,
    market_budget: &mut RiskBudget,
    market_state: &mut LiveMarketState,
) -> bool {
    if !response.success {
        return true;
    }

    if submitted_order.side == Side::Buy {
        reserve_buy_collateral(submitted_order.collateral, global_budget, market_budget);
    }
    market_state.record_pending_order(submitted_order.proposed_order);
    false
}

fn stale_live_data_reason(
    plan: &QuotePlan,
    market_state: &LiveMarketState,
    collateral_fetched_at: Option<Instant>,
    now: Instant,
    cli: &Cli,
) -> Result<Option<String>> {
    let max_age = Duration::from_secs(cli.max_data_age_secs);
    let token_state = market_state.token_state(plan.token_id)?;
    Ok(
        stale_input_reason("order book", plan.book_fetched_at, now, max_age)
            .or_else(|| {
                stale_input_reason(
                    "open orders",
                    token_state.open_orders_fetched_at,
                    now,
                    max_age,
                )
            })
            .or_else(|| {
                stale_input_reason(
                    "token balance",
                    token_state.balance_fetched_at,
                    now,
                    max_age,
                )
            })
            .or_else(|| {
                collateral_fetched_at.and_then(|fetched_at| {
                    stale_input_reason("collateral balance", fetched_at, now, max_age)
                })
            }),
    )
}

fn stale_input_reason(
    input: &str,
    fetched_at: Instant,
    now: Instant,
    max_age: Duration,
) -> Option<String> {
    let age = now.saturating_duration_since(fetched_at);
    (age > max_age).then(|| {
        format!(
            "{input} age {}ms exceeds max {}ms",
            age.as_millis(),
            max_age.as_millis()
        )
    })
}

fn open_order_matches_proposed(order: &OpenOrderResponse, proposed_order: ProposedOrder) -> bool {
    order.asset_id == proposed_order.token_id
        && order.side == proposed_order.side
        && order.price == proposed_order.price
        && open_order_remaining_size(order) == proposed_order.size
}

fn print_post_responses(plan: &QuotePlan, responses: &[(SubmittedOrder, PostOrderResponse)]) {
    for (submitted_order, response) in responses {
        println!(
            "posted {} {} side={:?} order_id={} success={} status={:?} error={:?}",
            plan.market_slug,
            plan.outcome,
            submitted_order.side,
            response.order_id,
            response.success,
            response.status,
            response.error_msg
        );
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rust_decimal_macros::dec;

    use crate::config::{DiscoveryMode, QuoteSides};

    use super::*;

    #[test]
    fn buy_top_up_skip_does_not_reserve_collateral() {
        let global_budget = RiskBudget::new(dec!(10));
        let market_budget = RiskBudget::new(dec!(10));

        let decision =
            buy_top_up_decision(dec!(5), dec!(0.50), dec!(1), &global_budget, &market_budget);

        assert_eq!(
            decision,
            BuyTopUpDecision::Skip {
                affordable_size: dec!(2)
            }
        );
        assert_eq!(global_budget.remaining_collateral(), dec!(10));
        assert_eq!(market_budget.remaining_collateral(), dec!(10));
    }

    #[test]
    fn buy_top_up_reserves_only_missing_size() {
        let mut global_budget = RiskBudget::new(dec!(10));
        let mut market_budget = RiskBudget::new(dec!(10));

        let decision = buy_top_up_decision(
            dec!(2),
            dec!(0.50),
            dec!(10),
            &global_budget,
            &market_budget,
        );

        assert_eq!(
            decision,
            BuyTopUpDecision::Place {
                size: dec!(2),
                collateral: dec!(1)
            }
        );
        reserve_buy_collateral(dec!(1), &mut global_budget, &mut market_budget);
        assert_eq!(global_budget.remaining_collateral(), dec!(9));
        assert_eq!(market_budget.remaining_collateral(), dec!(9));
    }

    #[test]
    fn open_buy_order_reserves_budget_before_risk_breach_return() {
        let mut budget = RiskBudget::new(dec!(10));
        let order = open_order(
            "open-buy",
            ProposedOrder {
                token_id: U256::from(1),
                side: Side::Buy,
                price: dec!(0.40),
                size: dec!(5),
            },
        );

        budget.reserve_open_buy_order(&order);

        assert_eq!(budget.remaining_collateral(), dec!(8));
    }

    #[test]
    fn market_exposure_counts_complete_set_as_hedged() {
        let exposure = MarketExposure {
            outcomes: vec![
                OutcomeExposure {
                    token_id: U256::from(1),
                    position: dec!(5),
                    cost: dec!(2.5),
                    proceeds: Decimal::ZERO,
                },
                OutcomeExposure {
                    token_id: U256::from(2),
                    position: dec!(5),
                    cost: dec!(2.5),
                    proceeds: Decimal::ZERO,
                },
            ],
        };

        assert_eq!(exposure.worst_loss(), Decimal::ZERO);
    }

    #[test]
    fn projected_buy_loss_uses_worst_resolution() {
        let mut exposure = MarketExposure {
            outcomes: vec![
                OutcomeExposure {
                    token_id: U256::from(1),
                    position: Decimal::ZERO,
                    cost: Decimal::ZERO,
                    proceeds: Decimal::ZERO,
                },
                OutcomeExposure {
                    token_id: U256::from(2),
                    position: Decimal::ZERO,
                    cost: Decimal::ZERO,
                    proceeds: Decimal::ZERO,
                },
            ],
        };

        exposure
            .apply_order(ProposedOrder {
                token_id: U256::from(1),
                side: Side::Buy,
                price: dec!(0.40),
                size: dec!(5),
            })
            .expect("known token should accept order");

        assert_eq!(exposure.worst_loss(), dec!(2));
    }

    #[test]
    fn projected_cross_outcome_buys_can_reduce_loss() {
        let mut exposure = MarketExposure {
            outcomes: vec![
                OutcomeExposure {
                    token_id: U256::from(1),
                    position: Decimal::ZERO,
                    cost: Decimal::ZERO,
                    proceeds: Decimal::ZERO,
                },
                OutcomeExposure {
                    token_id: U256::from(2),
                    position: Decimal::ZERO,
                    cost: Decimal::ZERO,
                    proceeds: Decimal::ZERO,
                },
            ],
        };

        for token_id in [U256::from(1), U256::from(2)] {
            exposure
                .apply_order(ProposedOrder {
                    token_id,
                    side: Side::Buy,
                    price: dec!(0.40),
                    size: dec!(5),
                })
                .expect("known token should accept order");
        }

        assert_eq!(exposure.worst_loss(), Decimal::ZERO);
    }

    #[test]
    fn liquidity_guard_follows_two_sided_live_flag() {
        let mut cli = test_cli();
        cli.live = true;
        cli.require_two_sided_live = false;
        assert!(!should_enforce_liquidity_quality(&cli));

        cli.require_two_sided_live = true;
        assert!(should_enforce_liquidity_quality(&cli));

        cli.live = false;
        assert!(!should_enforce_liquidity_quality(&cli));
    }

    #[test]
    fn liquidity_rejects_missing_two_sided_book() {
        let reason =
            liquidity_reject_reason(&[level(dec!(0.49), dec!(10))], &[], dec!(0.01), 20, dec!(5));

        assert_eq!(reason, Some(LiquidityRejectReason::MissingTwoSidedBook));
    }

    #[test]
    fn liquidity_rejects_wide_book() {
        let reason = liquidity_reject_reason(
            &[level(dec!(0.40), dec!(10))],
            &[level(dec!(0.70), dec!(10))],
            dec!(0.01),
            20,
            dec!(5),
        );

        assert_eq!(
            reason,
            Some(LiquidityRejectReason::SpreadTooWide {
                spread_ticks: dec!(30),
                max_spread_ticks: 20
            })
        );
    }

    #[test]
    fn liquidity_rejects_shallow_best_bid() {
        let reason = liquidity_reject_reason(
            &[level(dec!(0.49), dec!(4.9)), level(dec!(0.48), dec!(100))],
            &[level(dec!(0.51), dec!(10))],
            dec!(0.01),
            20,
            dec!(5),
        );

        assert_eq!(
            reason,
            Some(LiquidityRejectReason::BidDepthTooLow {
                depth: dec!(4.9),
                min_depth: dec!(5)
            })
        );
    }

    #[test]
    fn liquidity_accepts_tight_book_with_enough_top_depth() {
        let reason = liquidity_reject_reason(
            &[level(dec!(0.49), dec!(2)), level(dec!(0.49), dec!(3))],
            &[level(dec!(0.51), dec!(5))],
            dec!(0.01),
            20,
            dec!(5),
        );

        assert_eq!(reason, None);
    }

    #[test]
    fn quote_band_contains_configured_price_range() {
        let band = QuoteBand {
            side: Side::Buy,
            price: dec!(0.49),
            min_price: dec!(0.47),
            max_price: dec!(0.49),
            minimum_size: dec!(1),
            min_size: dec!(5),
            avg_size: dec!(10),
            max_size: dec!(15),
        };

        assert!(band.contains_price(dec!(0.47)));
        assert!(band.contains_price(dec!(0.48)));
        assert!(band.contains_price(dec!(0.49)));
        assert!(!band.contains_price(dec!(0.46)));
        assert!(!band.contains_price(dec!(0.50)));
    }

    #[test]
    fn band_missing_size_targets_average_only_below_minimum() {
        let band = QuoteBand {
            side: Side::Buy,
            price: dec!(0.49),
            min_price: dec!(0.47),
            max_price: dec!(0.49),
            minimum_size: dec!(1),
            min_size: dec!(5),
            avg_size: dec!(10),
            max_size: dec!(15),
        };

        assert_eq!(band_missing_size(&band, dec!(4)), Some(dec!(6)));
        assert_eq!(band_missing_size(&band, dec!(5)), None);
        assert_eq!(band_missing_size(&band, dec!(9)), None);
    }

    #[test]
    fn inventory_adjusted_buy_size_caps_to_remaining_room() {
        assert_eq!(
            inventory_adjusted_buy_size(dec!(5), dec!(1), dec!(3)),
            Some(dec!(3))
        );
    }

    #[test]
    fn inventory_adjusted_buy_size_skips_when_room_is_below_minimum() {
        assert_eq!(
            inventory_adjusted_buy_size(dec!(5), dec!(1), dec!(0.5)),
            None
        );
    }

    #[test]
    fn inventory_buy_room_counts_balance_and_open_buys() {
        let mut cli = test_cli();
        cli.max_inventory_per_token = dec!(20);
        cli.max_inventory_per_market = dec!(50);
        let mut market_state = test_market_state();
        market_state.tokens[0].balance = dec!(10);
        market_state
            .replace_open_orders(
                U256::from(1),
                vec![open_order(
                    "open-buy",
                    ProposedOrder {
                        token_id: U256::from(1),
                        side: Side::Buy,
                        price: dec!(0.40),
                        size: dec!(5),
                    },
                )],
                Instant::now(),
            )
            .expect("test token should exist");

        let room = market_state
            .inventory_buy_room(U256::from(1), &[], &cli)
            .expect("inventory room should be computed");

        assert_eq!(
            room,
            InventoryBuyRoom {
                token_position: dec!(15),
                market_inventory: dec!(15),
                room: dec!(5)
            }
        );
    }

    #[test]
    fn inventory_buy_room_ignores_open_sells() {
        let mut cli = test_cli();
        cli.max_inventory_per_token = dec!(20);
        cli.max_inventory_per_market = dec!(50);
        let mut market_state = test_market_state();
        market_state.tokens[0].balance = dec!(20);
        market_state
            .replace_open_orders(
                U256::from(1),
                vec![open_order(
                    "open-sell",
                    ProposedOrder {
                        token_id: U256::from(1),
                        side: Side::Sell,
                        price: dec!(0.60),
                        size: dec!(10),
                    },
                )],
                Instant::now(),
            )
            .expect("test token should exist");

        let room = market_state
            .inventory_buy_room(U256::from(1), &[], &cli)
            .expect("inventory room should be computed");

        assert_eq!(
            room,
            InventoryBuyRoom {
                token_position: dec!(20),
                market_inventory: dec!(20),
                room: Decimal::ZERO
            }
        );
    }

    #[test]
    fn risk_breaches_detect_token_inventory_over_limit() {
        let mut cli = test_cli();
        cli.max_inventory_per_token = dec!(10);
        let mut market_state = test_market_state();
        market_state.tokens[0].balance = dec!(11);

        let breaches = market_state.risk_breaches(&cli);

        assert!(breaches.contains(&RiskBreach::TokenInventory {
            token_id: U256::from(1),
            inventory: dec!(11),
            limit: dec!(10),
        }));
    }

    #[test]
    fn risk_breaches_detect_market_inventory_over_limit() {
        let mut cli = test_cli();
        cli.max_inventory_per_market = dec!(15);
        let mut market_state = test_market_state();
        market_state.tokens[0].balance = dec!(8);
        market_state.tokens[1].balance = dec!(8);

        let breaches = market_state.risk_breaches(&cli);

        assert!(breaches.contains(&RiskBreach::MarketInventory {
            inventory: dec!(16),
            limit: dec!(15),
        }));
    }

    #[test]
    fn accepted_buy_post_reserves_budget_and_records_pending_order() {
        let mut global_budget = RiskBudget::new(dec!(10));
        let mut market_budget = RiskBudget::new(dec!(10));
        let mut market_state = test_market_state();
        let submitted_order = SubmittedOrder {
            side: Side::Buy,
            proposed_order: ProposedOrder {
                token_id: U256::from(1),
                side: Side::Buy,
                price: dec!(0.40),
                size: dec!(5),
            },
            collateral: dec!(2),
        };
        let response = post_response(true);

        let failed = apply_post_response(
            submitted_order,
            &response,
            &mut global_budget,
            &mut market_budget,
            &mut market_state,
        );

        assert!(!failed);
        assert_eq!(global_budget.remaining_collateral(), dec!(8));
        assert_eq!(market_budget.remaining_collateral(), dec!(8));
        assert_eq!(market_state.pending_orders.len(), 1);
    }

    #[test]
    fn rejected_post_does_not_reserve_budget_or_record_pending_order() {
        let mut global_budget = RiskBudget::new(dec!(10));
        let mut market_budget = RiskBudget::new(dec!(10));
        let mut market_state = test_market_state();
        let submitted_order = SubmittedOrder {
            side: Side::Buy,
            proposed_order: ProposedOrder {
                token_id: U256::from(1),
                side: Side::Buy,
                price: dec!(0.40),
                size: dec!(5),
            },
            collateral: dec!(2),
        };
        let response = post_response(false);

        let failed = apply_post_response(
            submitted_order,
            &response,
            &mut global_budget,
            &mut market_budget,
            &mut market_state,
        );

        assert!(failed);
        assert_eq!(global_budget.remaining_collateral(), dec!(10));
        assert_eq!(market_budget.remaining_collateral(), dec!(10));
        assert!(market_state.pending_orders.is_empty());
    }

    #[test]
    fn pending_order_is_removed_when_refreshed_open_order_matches_it() {
        let mut market_state = test_market_state();
        let proposed_order = ProposedOrder {
            token_id: U256::from(1),
            side: Side::Buy,
            price: dec!(0.40),
            size: dec!(5),
        };
        market_state.record_pending_order(proposed_order);
        let refreshed_orders = vec![open_order("posted-order", proposed_order)];

        market_state.remove_pending_orders_now_open(&refreshed_orders);
        market_state
            .replace_open_orders(U256::from(1), refreshed_orders, Instant::now())
            .expect("test token should exist");

        assert!(market_state.pending_orders.is_empty());
        assert_eq!(
            market_state
                .open_orders(U256::from(1))
                .expect("test token should exist")
                .len(),
            1
        );
        assert_eq!(market_state.exposure().worst_loss(), dec!(2));
    }

    #[test]
    fn stale_input_reason_flags_data_older_than_threshold() {
        let now = Instant::now();
        let reason = stale_input_reason(
            "order book",
            now - Duration::from_secs(11),
            now,
            Duration::from_secs(10),
        );

        assert!(
            reason
                .expect("old input should be stale")
                .contains("order book")
        );
    }

    #[test]
    fn stale_input_reason_accepts_fresh_data() {
        let now = Instant::now();
        let reason = stale_input_reason(
            "order book",
            now - Duration::from_secs(9),
            now,
            Duration::from_secs(10),
        );

        assert!(reason.is_none());
    }

    fn level(price: Decimal, size: Decimal) -> OrderSummary {
        OrderSummary::builder().price(price).size(size).build()
    }

    fn open_order(id: &str, proposed_order: ProposedOrder) -> OpenOrderResponse {
        OpenOrderResponse::builder()
            .id(id)
            .status(OrderStatusType::Live)
            .owner("01HAAAAAAAAAAAAAAAAAAAAAAAAA")
            .maker_address(Address::ZERO)
            .market(Default::default())
            .asset_id(proposed_order.token_id)
            .side(proposed_order.side)
            .original_size(proposed_order.size)
            .size_matched(Decimal::ZERO)
            .price(proposed_order.price)
            .associate_trades(Vec::new())
            .outcome("YES")
            .created_at("2024-01-15T12:34:56Z".parse().unwrap())
            .expiration("2024-01-20T00:00:00Z".parse().unwrap())
            .order_type(OrderType::GTC)
            .build()
    }

    fn post_response(success: bool) -> PostOrderResponse {
        PostOrderResponse::builder()
            .making_amount(Decimal::ZERO)
            .taking_amount(Decimal::ZERO)
            .order_id("0x1")
            .status(if success {
                OrderStatusType::Live
            } else {
                OrderStatusType::Unknown("rejected".to_owned())
            })
            .success(success)
            .build()
    }

    fn test_market_state() -> LiveMarketState {
        LiveMarketState {
            tokens: vec![
                LiveTokenState {
                    token_id: U256::from(1),
                    fair_price: dec!(0.50),
                    balance: Decimal::ZERO,
                    balance_fetched_at: Instant::now(),
                    open_orders: Vec::new(),
                    open_orders_fetched_at: Instant::now(),
                },
                LiveTokenState {
                    token_id: U256::from(2),
                    fair_price: dec!(0.50),
                    balance: Decimal::ZERO,
                    balance_fetched_at: Instant::now(),
                    open_orders: Vec::new(),
                    open_orders_fetched_at: Instant::now(),
                },
            ],
            pending_orders: Vec::new(),
        }
    }

    fn test_cli() -> Cli {
        Cli {
            clob_host: "https://clob.kuest.com".to_owned(),
            live: false,
            private_key: None,
            deposit_wallet: None,
            chain_id: None,
            discovery: DiscoveryMode::Auto,
            event_slug: None,
            max_markets: 3,
            max_pages: 5,
            order_size: dec!(5),
            edge_ticks: 1,
            min_spread_ticks: 2,
            band_min_margin_ticks: None,
            band_avg_margin_ticks: None,
            band_max_margin_ticks: None,
            band_min_size: None,
            band_avg_size: None,
            band_max_size: None,
            max_book_spread_ticks: 20,
            min_top_depth: dec!(5),
            quote_sides: QuoteSides::Buy,
            allow_single_sided: true,
            respect_reward_min_size: false,
            cancel_before_quote: true,
            cancel_all: false,
            cancel_all_on_exit: false,
            cancel_on_risk_breach: false,
            pause_on_risk_breach: false,
            clear_pause: false,
            pause_path: PathBuf::from("state/paused.json"),
            post_only: true,
            require_two_sided_live: true,
            min_price: dec!(0.05),
            max_price: dec!(0.95),
            max_collateral_per_market: dec!(25),
            max_loss_per_market: dec!(25),
            max_inventory_per_token: dec!(25),
            max_inventory_per_market: dec!(50),
            max_total_collateral: dec!(50),
            min_free_collateral: Decimal::ONE,
            max_data_age_secs: 10,
            max_open_orders_per_token: 2,
            discover_only: false,
            cycles: 1,
            refresh_secs: 30,
            state_path: PathBuf::from("state/seen-markets.json"),
        }
    }
}
