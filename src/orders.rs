use std::collections::BTreeSet;
use std::str::FromStr as _;

use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::{Context as _, Result};
use kuest_client_sdk::auth::Signer as _;
use kuest_client_sdk::clob::types::request::{
    BalanceAllowanceRequest, OrderBookSummaryRequest, OrdersRequest,
};
use kuest_client_sdk::clob::types::response::{
    MarketResponse, OpenOrderResponse, OrderBookSummaryResponse, PostOrderResponse, Token,
};
use kuest_client_sdk::clob::types::{AssetType, OrderStatusType, OrderType, Side, SignatureType};
use kuest_client_sdk::clob::{Client, Config};
use kuest_client_sdk::types::{Address, Decimal, U256};

use crate::config::Cli;
use crate::discovery::market_key;
use crate::pricing::{best_ask, best_bid, fair_price, max_decimal, min_decimal, quote_prices};
use crate::{AuthClient, PublicClient};

const TERMINAL_CURSOR: &str = "LTE=";

pub(crate) struct LiveTrading {
    client: AuthClient,
    signer: PrivateKeySigner,
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
    buy_price: Option<Decimal>,
    sell_price: Option<Decimal>,
    size: Decimal,
}

#[derive(Debug, PartialEq)]
enum BuyTopUpDecision {
    Place { size: Decimal },
    Skip { affordable_size: Decimal },
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

    for token in &market.tokens {
        let request = OrderBookSummaryRequest::builder()
            .token_id(token.token_id)
            .build();
        let book = public_client
            .order_book(&request)
            .await
            .with_context(|| format!("failed to fetch order book for token {}", token.token_id))?;

        let Some(plan) = build_quote_plan(market, token, &book, cli) else {
            println!(
                "skip {} {}: no safe quote at configured edge/sides",
                market.market_slug, token.outcome
            );
            continue;
        };

        print_plan(&plan, cli.live);

        if let Some(live) = live {
            reconcile_quote_plan(live, &plan, cli, global_budget, &mut market_budget).await?;
        }
    }

    Ok(())
}

fn build_quote_plan(
    market: &MarketResponse,
    token: &Token,
    book: &OrderBookSummaryResponse,
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
    let (mut buy_price, mut sell_price) = quote_prices(
        fair_price,
        best_bid,
        best_ask,
        tick,
        cli.edge_ticks,
        cli.min_spread_ticks,
    );

    if !cli.quote_sides.includes_buy() {
        buy_price = None;
    }
    if !cli.quote_sides.includes_sell() {
        sell_price = None;
    }

    buy_price = buy_price.filter(|price| price_in_configured_range(*price, cli));
    sell_price = sell_price.filter(|price| price_in_configured_range(*price, cli));

    if !cli.allow_single_sided && (buy_price.is_none() || sell_price.is_none()) {
        return None;
    }

    if let (Some(buy), Some(sell)) = (buy_price, sell_price)
        && buy >= sell
    {
        buy_price = None;
        sell_price = None;
    }

    if buy_price.is_none() && sell_price.is_none() {
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
        buy_price,
        sell_price,
        size: order_size(market, cli),
    })
}

fn order_size(market: &MarketResponse, cli: &Cli) -> Decimal {
    let mut size = max_decimal(cli.order_size, market.minimum_order_size);
    if cli.respect_reward_min_size {
        size = max_decimal(size, market.rewards.min_size);
    }
    size
}

fn price_in_configured_range(price: Decimal, cli: &Cli) -> bool {
    price >= cli.min_price && price <= cli.max_price
}

fn print_plan(plan: &QuotePlan, live: bool) {
    let mode = if live { "live" } else { "dry-run" };
    println!(
        "{mode}: {} :: {} :: {} :: {} ({}) fair={} bid={:?} ask={:?} buy={:?} sell={:?} size={}",
        plan.market_key,
        plan.market_slug,
        plan.question,
        plan.outcome,
        plan.token_id,
        plan.fair_price,
        plan.best_bid,
        plan.best_ask,
        plan.buy_price,
        plan.sell_price,
        plan.size
    );
}

async fn reconcile_quote_plan(
    live: &LiveTrading,
    plan: &QuotePlan,
    cli: &Cli,
    global_budget: &mut RiskBudget,
    market_budget: &mut RiskBudget,
) -> Result<()> {
    let open_orders = open_orders_for_token(live, plan.token_id).await?;
    let orders_to_cancel = cancellable_orders(&open_orders, plan, cli);
    if cli.cancel_before_quote && !orders_to_cancel.is_empty() {
        let order_ids = orders_to_cancel
            .iter()
            .map(|order| order.id.as_str())
            .collect::<Vec<_>>();
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
    }

    let canceled_ids = orders_to_cancel
        .iter()
        .map(|order| order.id.as_str())
        .collect::<BTreeSet<_>>();
    let remaining_orders = open_orders
        .iter()
        .filter(|order| !canceled_ids.contains(order.id.as_str()))
        .collect::<Vec<_>>();

    for order in &remaining_orders {
        global_budget.reserve_open_buy_order(order);
        market_budget.reserve_open_buy_order(order);
    }

    let collateral_balance = collateral_balance(live).await?;
    let token_balance = conditional_balance(live, plan.token_id).await?;
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

    let mut orders = Vec::new();
    if let Some(price) = plan.buy_price
        && new_order_slots > 0
    {
        let open_size = matching_open_size(&remaining_orders, Side::Buy, price);
        if open_size < plan.size {
            let missing_size = plan.size - open_size;
            match reserve_buy_top_up(
                missing_size,
                price,
                free_collateral,
                global_budget,
                market_budget,
            ) {
                BuyTopUpDecision::Place { size } => {
                    let order = live
                        .client
                        .limit_order()
                        .token_id(plan.token_id)
                        .side(Side::Buy)
                        .price(price)
                        .size(size)
                        .order_type(OrderType::GTC)
                        .post_only(cli.post_only)
                        .build()
                        .await
                        .with_context(|| {
                            format!("failed to build buy order for token {}", plan.token_id)
                        })?;
                    orders.push((Side::Buy, live.client.sign(&live.signer, order).await?));
                    new_order_slots -= 1;
                }
                BuyTopUpDecision::Skip { affordable_size } => {
                    println!(
                        "skip {} {} buy: risk budget/free collateral leaves size {} below required {}",
                        plan.market_slug, plan.outcome, affordable_size, missing_size
                    );
                }
            }
        }
    }

    if let Some(price) = plan.sell_price
        && new_order_slots > 0
    {
        let open_size = matching_open_size(&remaining_orders, Side::Sell, price);
        if open_size < plan.size {
            let missing_size = plan.size - open_size;
            let size = min_decimal(missing_size, free_tokens);
            if size >= missing_size {
                let order = live
                    .client
                    .limit_order()
                    .token_id(plan.token_id)
                    .side(Side::Sell)
                    .price(price)
                    .size(size)
                    .order_type(OrderType::GTC)
                    .post_only(cli.post_only)
                    .build()
                    .await
                    .with_context(|| {
                        format!("failed to build sell order for token {}", plan.token_id)
                    })?;
                orders.push((Side::Sell, live.client.sign(&live.signer, order).await?));
            } else {
                println!(
                    "skip {} {} sell: free token balance leaves size {} below required {}",
                    plan.market_slug, plan.outcome, size, missing_size
                );
            }
        }
    }

    if orders.is_empty() {
        return Ok(());
    }

    let sides = orders.iter().map(|(side, _)| *side).collect::<Vec<_>>();
    let signed_orders = orders
        .into_iter()
        .map(|(_, order)| order)
        .collect::<Vec<_>>();
    let responses = live.client.post_orders(signed_orders).await?;
    let responses = sides.into_iter().zip(responses).collect::<Vec<_>>();
    print_post_responses(plan, &responses);

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

fn reserve_buy_top_up(
    missing_size: Decimal,
    price: Decimal,
    free_collateral: Decimal,
    global_budget: &mut RiskBudget,
    market_budget: &mut RiskBudget,
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
    let global_reserved = global_budget.reserve_new_collateral(collateral);
    let market_reserved = market_budget.reserve_new_collateral(global_reserved);
    debug_assert_eq!(global_reserved, collateral);
    debug_assert_eq!(market_reserved, collateral);

    BuyTopUpDecision::Place { size: missing_size }
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

    for (side, target_price) in [(Side::Buy, plan.buy_price), (Side::Sell, plan.sell_price)] {
        let Some(target_price) = target_price else {
            continue;
        };
        let matching_orders = open_orders
            .iter()
            .filter(|order| {
                !cancellable_ids.contains(order.id.as_str())
                    && order.side == side
                    && order.price == target_price
            })
            .collect::<Vec<_>>();
        let matching_size = matching_orders
            .iter()
            .map(|order| open_order_remaining_size(order))
            .sum::<Decimal>();
        if matching_size > plan.size {
            for order in matching_orders {
                if cancellable_ids.insert(order.id.as_str()) {
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
        Side::Buy => plan.buy_price != Some(order.price),
        Side::Sell => plan.sell_price != Some(order.price),
        Side::Unknown => true,
        _ => true,
    }
}

fn matching_open_size(orders: &[&OpenOrderResponse], side: Side, price: Decimal) -> Decimal {
    orders
        .iter()
        .filter(|order| order.side == side && order.price == price)
        .map(|order| open_order_remaining_size(order))
        .sum()
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

fn print_post_responses(plan: &QuotePlan, responses: &[(Side, PostOrderResponse)]) {
    for (side, response) in responses {
        println!(
            "posted {} {} side={side:?} order_id={} success={} status={:?} error={:?}",
            plan.market_slug,
            plan.outcome,
            response.order_id,
            response.success,
            response.status,
            response.error_msg
        );
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::*;

    #[test]
    fn buy_top_up_skip_does_not_reserve_collateral() {
        let mut global_budget = RiskBudget::new(dec!(10));
        let mut market_budget = RiskBudget::new(dec!(10));

        let decision = reserve_buy_top_up(
            dec!(5),
            dec!(0.50),
            dec!(1),
            &mut global_budget,
            &mut market_budget,
        );

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

        let decision = reserve_buy_top_up(
            dec!(2),
            dec!(0.50),
            dec!(10),
            &mut global_budget,
            &mut market_budget,
        );

        assert_eq!(decision, BuyTopUpDecision::Place { size: dec!(2) });
        assert_eq!(global_budget.remaining_collateral(), dec!(9));
        assert_eq!(market_budget.remaining_collateral(), dec!(9));
    }
}
