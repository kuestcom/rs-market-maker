use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};
use kuest_client_sdk::clob::types::response::MarketResponse;
use kuest_client_sdk::types::B256;
use kuest_client_sdk::types::Decimal;
use reqwest::Url;
use serde::Deserialize;

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

pub(crate) async fn discover_event_markets(
    clob_client: &PublicClient,
    event_slug: &str,
    max_pages: usize,
) -> Result<Vec<MarketResponse>> {
    let condition_ids = condition_ids_from_site_config(event_slug, max_pages).await?;

    let mut markets = Vec::new();
    for condition_id in condition_ids {
        let market = clob_client
            .market(&condition_id)
            .await
            .with_context(|| format!("failed to fetch CLOB market {condition_id}"))?;
        markets.push(market);
    }

    Ok(markets)
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

    sort_candidates(&mut candidates);
    candidates.truncate(max_markets);
    candidates
}

pub(crate) fn select_event_candidates(
    markets: Vec<MarketResponse>,
    max_markets: usize,
) -> Vec<MarketCandidate> {
    let mut candidates = markets
        .into_iter()
        .filter(is_tradable_market)
        .map(|market| MarketCandidate {
            market,
            is_new: false,
        })
        .collect::<Vec<_>>();

    sort_candidates(&mut candidates);
    candidates.truncate(max_markets);
    candidates
}

fn sort_candidates(candidates: &mut [MarketCandidate]) {
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

async fn condition_ids_from_site_config(
    event_slug: &str,
    max_pages: usize,
) -> Result<BTreeSet<String>> {
    let site_url = site_url_from_config(Path::new(".sdk/site-config.json"))?;
    let events = fetch_site_events(&site_url, max_pages).await?;
    let condition_ids = events
        .into_iter()
        .find(|event| event.slug.as_deref() == Some(event_slug))
        .map(|event| {
            event
                .markets
                .unwrap_or_default()
                .into_iter()
                .filter_map(|market| {
                    market
                        .condition_id
                        .map(|condition_id| condition_id.to_string())
                })
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();

    Ok(condition_ids)
}

fn site_url_from_config(path: &Path) -> Result<String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let config: SiteConfig = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let site_url = config.site_url.trim();
    if site_url.is_empty() {
        anyhow::bail!("{}.site_url must not be empty", path.display());
    } else if site_url.starts_with("http://") || site_url.starts_with("https://") {
        Ok(site_url.to_owned())
    } else {
        Ok(format!("https://{site_url}"))
    }
}

async fn fetch_site_events(site_url: &str, max_pages: usize) -> Result<Vec<SiteEvent>> {
    const SITE_EVENTS_LIMIT: usize = 100;

    let origin = Url::parse(site_url).with_context(|| format!("invalid site_url {site_url}"))?;
    let client = reqwest::Client::new();
    let mut events = Vec::new();

    for page in 0..max_pages.max(1) {
        let mut url = origin
            .join("api/events")
            .with_context(|| format!("invalid site events URL for {site_url}"))?;
        url.query_pairs_mut()
            .append_pair("status", "active")
            .append_pair("includeBookmarkState", "false")
            .append_pair("limit", &SITE_EVENTS_LIMIT.to_string())
            .append_pair("offset", &(page * SITE_EVENTS_LIMIT).to_string());

        let page_events = client
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("failed to fetch site events from {url}"))?
            .error_for_status()
            .with_context(|| format!("site events request failed for {url}"))?
            .json::<Vec<SiteEvent>>()
            .await
            .with_context(|| format!("failed to parse site events from {url}"))?;
        let is_last_page = page_events.len() < SITE_EVENTS_LIMIT;
        events.extend(page_events);
        if is_last_page {
            break;
        }
    }

    Ok(events)
}

#[derive(Debug, Deserialize)]
struct SiteConfig {
    site_url: String,
}

#[derive(Debug, Deserialize)]
struct SiteEvent {
    slug: Option<String>,
    markets: Option<Vec<SiteMarket>>,
}

#[derive(Debug, Deserialize)]
struct SiteMarket {
    condition_id: Option<B256>,
}
