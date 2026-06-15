use anyhow::Result;
use kuest_client_sdk::clob::types::response::MarketResponse;
use kuest_client_sdk::types::Decimal;

use crate::PublicClient;
use crate::config::DiscoveryMode;
use crate::state::SeenMarkets;

const TERMINAL_CURSOR: &str = "LTE=";

#[derive(Clone, Debug)]
pub(crate) struct MarketCandidate {
    pub(crate) market: MarketResponse,
    pub(crate) is_new: bool,
}

pub(crate) async fn discover_markets(
    client: &PublicClient,
    mode: DiscoveryMode,
    max_pages: usize,
) -> Result<Vec<MarketResponse>> {
    match mode {
        DiscoveryMode::Auto => {
            let sampling = fetch_market_pages(client, DiscoveryMode::Sampling, max_pages).await?;
            if sampling.is_empty() {
                fetch_market_pages(client, DiscoveryMode::Site, max_pages).await
            } else {
                Ok(sampling)
            }
        }
        DiscoveryMode::Sampling | DiscoveryMode::Site => {
            fetch_market_pages(client, mode, max_pages).await
        }
    }
}

pub(crate) async fn fetch_market_pages(
    client: &PublicClient,
    mode: DiscoveryMode,
    max_pages: usize,
) -> Result<Vec<MarketResponse>> {
    let mut cursor = None;
    let mut markets = Vec::new();

    for _ in 0..max_pages {
        let previous_cursor = cursor.clone();
        let page = match mode {
            DiscoveryMode::Sampling => client.sampling_markets(cursor).await?,
            DiscoveryMode::Site => client.markets(cursor).await?,
            DiscoveryMode::Auto => unreachable!("auto is expanded by discover_markets"),
        };

        markets.extend(page.data);
        if page.next_cursor == TERMINAL_CURSOR
            || previous_cursor.as_deref() == Some(&page.next_cursor)
        {
            break;
        }

        cursor = Some(page.next_cursor);
    }

    Ok(markets)
}

pub(crate) fn select_candidates(
    markets: Vec<MarketResponse>,
    seen: &mut SeenMarkets,
    max_markets: usize,
) -> Vec<MarketCandidate> {
    let mut candidates = markets
        .into_iter()
        .filter(is_tradable_market)
        .map(|market| {
            let is_new = seen.mark_new(market_key(&market));
            MarketCandidate { market, is_new }
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|a, b| {
        b.is_new
            .cmp(&a.is_new)
            .then_with(|| has_rewards(&b.market).cmp(&has_rewards(&a.market)))
            .then_with(|| {
                b.market
                    .accepting_order_timestamp
                    .cmp(&a.market.accepting_order_timestamp)
            })
            .then_with(|| a.market.market_slug.cmp(&b.market.market_slug))
    });
    candidates.truncate(max_markets);
    candidates
}

fn is_tradable_market(market: &MarketResponse) -> bool {
    market.enable_order_book
        && market.active
        && !market.closed
        && !market.archived
        && market.accepting_orders
        && !market.tokens.is_empty()
}

fn has_rewards(market: &MarketResponse) -> bool {
    !market.rewards.rates.is_empty() || market.rewards.min_size > Decimal::ZERO
}

pub(crate) fn market_key(market: &MarketResponse) -> String {
    market
        .condition_id
        .map(|condition_id| condition_id.to_string())
        .unwrap_or_else(|| market.market_slug.clone())
}
