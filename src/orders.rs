use std::str::FromStr as _;

use alloy::signers::local::{LocalSigner, PrivateKeySigner};
use anyhow::{Context as _, Result};
use kuest_client_sdk::auth::Signer as _;
use kuest_client_sdk::clob::types::request::{CancelMarketOrderRequest, OrderBookSummaryRequest};
use kuest_client_sdk::clob::types::response::{
    MarketResponse, OrderBookSummaryResponse, PostOrderResponse, Token,
};
use kuest_client_sdk::clob::types::{OrderType, Side, SignatureType};
use kuest_client_sdk::clob::{Client, Config};
use kuest_client_sdk::types::{Address, Decimal, U256};

use crate::config::Cli;
use crate::discovery::market_key;
use crate::pricing::{best_ask, best_bid, fair_price, max_decimal, quote_prices};
use crate::{AuthClient, PublicClient};

pub(crate) struct LiveTrading {
    client: AuthClient,
    signer: PrivateKeySigner,
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
) -> Result<()> {
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
            post_quote_plan(live, &plan, cli).await?;
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

async fn post_quote_plan(live: &LiveTrading, plan: &QuotePlan, cli: &Cli) -> Result<()> {
    if cli.cancel_before_quote {
        let request = CancelMarketOrderRequest::builder()
            .asset_id(plan.token_id)
            .build();
        let response = live
            .client
            .cancel_market_orders(&request)
            .await
            .with_context(|| {
                format!("failed to cancel stale orders for token {}", plan.token_id)
            })?;
        if !response.canceled.is_empty() || !response.not_canceled.is_empty() {
            println!(
                "canceled stale orders for {}: canceled={} not_canceled={}",
                plan.token_id,
                response.canceled.len(),
                response.not_canceled.len()
            );
        }
    }

    let mut orders = Vec::new();
    if let Some(price) = plan.buy_price {
        let order = live
            .client
            .limit_order()
            .token_id(plan.token_id)
            .side(Side::Buy)
            .price(price)
            .size(plan.size)
            .order_type(OrderType::GTC)
            .post_only(cli.post_only)
            .build()
            .await
            .with_context(|| format!("failed to build buy order for token {}", plan.token_id))?;
        orders.push((Side::Buy, live.client.sign(&live.signer, order).await?));
    }

    if let Some(price) = plan.sell_price {
        let order = live
            .client
            .limit_order()
            .token_id(plan.token_id)
            .side(Side::Sell)
            .price(price)
            .size(plan.size)
            .order_type(OrderType::GTC)
            .post_only(cli.post_only)
            .build()
            .await
            .with_context(|| format!("failed to build sell order for token {}", plan.token_id))?;
        orders.push((Side::Sell, live.client.sign(&live.signer, order).await?));
    }

    let mut responses = Vec::new();
    for (side, order) in orders {
        let response = live.client.post_order(order).await?;
        responses.push((side, response));
    }
    print_post_responses(plan, &responses);

    Ok(())
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
