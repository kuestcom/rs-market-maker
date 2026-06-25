use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use kuest_client_sdk::types::{ChainId, Decimal};
use kuest_client_sdk::{AMOY, POLYGON};

#[derive(Debug, Parser)]
#[command(
    name = "rs-market-maker",
    about = "Simple Rust market maker example for Kuest based prediction markets"
)]
pub struct Cli {
    #[arg(
        long,
        env = "KUEST_CLOB_HOST",
        default_value = "https://clob.kuest.com"
    )]
    pub clob_host: String,

    #[arg(long, env = "MARKET_MAKER_LIVE", default_value_t = false)]
    pub live: bool,

    #[arg(long, env = "KUEST_PRIVATE_KEY")]
    pub private_key: Option<String>,

    #[arg(long, env = "KUEST_DEPOSIT_WALLET")]
    pub deposit_wallet: Option<String>,

    #[arg(long, env = "KUEST_CHAIN_ID")]
    pub chain_id: Option<ChainId>,

    #[arg(long, env = "MARKET_MAKER_DISCOVERY", value_enum, default_value_t = DiscoveryMode::Auto)]
    pub discovery: DiscoveryMode,

    #[arg(long, env = "MARKET_MAKER_EVENT_SLUG")]
    pub event_slug: Option<String>,

    #[arg(long, env = "MARKET_MAKER_MAX_MARKETS", default_value_t = 3)]
    pub max_markets: usize,

    #[arg(long, env = "MARKET_MAKER_MAX_PAGES", default_value_t = 5)]
    pub max_pages: usize,

    #[arg(long, env = "MARKET_MAKER_ORDER_SIZE", default_value = "5")]
    pub order_size: Decimal,

    #[arg(long, env = "MARKET_MAKER_EDGE_TICKS", default_value_t = 1)]
    pub edge_ticks: u32,

    #[arg(long, env = "MARKET_MAKER_MIN_SPREAD_TICKS", default_value_t = 2)]
    pub min_spread_ticks: u32,

    #[arg(long, env = "MARKET_MAKER_MAX_BOOK_SPREAD_TICKS", default_value_t = 20)]
    pub max_book_spread_ticks: u32,

    #[arg(long, env = "MARKET_MAKER_MIN_TOP_DEPTH", default_value = "5")]
    pub min_top_depth: Decimal,

    #[arg(long, env = "MARKET_MAKER_QUOTE_SIDES", value_enum, default_value_t = QuoteSides::Buy)]
    pub quote_sides: QuoteSides,

    #[arg(long, env = "MARKET_MAKER_ALLOW_SINGLE_SIDED", default_value_t = true)]
    pub allow_single_sided: bool,

    #[arg(
        long,
        env = "MARKET_MAKER_RESPECT_REWARD_MIN_SIZE",
        default_value_t = false
    )]
    pub respect_reward_min_size: bool,

    #[arg(long, env = "MARKET_MAKER_CANCEL_BEFORE_QUOTE", default_value_t = true)]
    pub cancel_before_quote: bool,

    #[arg(long, env = "MARKET_MAKER_POST_ONLY", default_value_t = true)]
    pub post_only: bool,

    #[arg(
        long,
        env = "MARKET_MAKER_REQUIRE_TWO_SIDED_LIVE",
        default_value_t = true
    )]
    pub require_two_sided_live: bool,

    #[arg(long, env = "MARKET_MAKER_MIN_PRICE", default_value = "0.05")]
    pub min_price: Decimal,

    #[arg(long, env = "MARKET_MAKER_MAX_PRICE", default_value = "0.95")]
    pub max_price: Decimal,

    #[arg(
        long,
        env = "MARKET_MAKER_MAX_COLLATERAL_PER_MARKET",
        default_value = "25"
    )]
    pub max_collateral_per_market: Decimal,

    #[arg(long, env = "MARKET_MAKER_MAX_LOSS_PER_MARKET", default_value = "25")]
    pub max_loss_per_market: Decimal,

    #[arg(long, env = "MARKET_MAKER_MAX_TOTAL_COLLATERAL", default_value = "50")]
    pub max_total_collateral: Decimal,

    #[arg(long, env = "MARKET_MAKER_MIN_FREE_COLLATERAL", default_value = "1")]
    pub min_free_collateral: Decimal,

    #[arg(
        long,
        env = "MARKET_MAKER_MAX_OPEN_ORDERS_PER_TOKEN",
        default_value_t = 2
    )]
    pub max_open_orders_per_token: usize,

    #[arg(long, env = "MARKET_MAKER_DISCOVER_ONLY", default_value_t = false)]
    pub discover_only: bool,

    #[arg(long, env = "MARKET_MAKER_CYCLES", default_value_t = 1)]
    pub cycles: u32,

    #[arg(long, env = "MARKET_MAKER_REFRESH_SECS", default_value_t = 30)]
    pub refresh_secs: u64,

    #[arg(
        long,
        env = "MARKET_MAKER_STATE_PATH",
        default_value = "state/seen-markets.json"
    )]
    pub state_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DiscoveryMode {
    Auto,
    Sampling,
    Site,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum QuoteSides {
    Buy,
    Sell,
    Both,
}

impl QuoteSides {
    pub(crate) const fn includes_buy(self) -> bool {
        matches!(self, Self::Buy | Self::Both)
    }

    pub(crate) const fn includes_sell(self) -> bool {
        matches!(self, Self::Sell | Self::Both)
    }
}

pub fn validate_cli(cli: &Cli) -> Result<()> {
    if cli.max_markets == 0 {
        bail!("MARKET_MAKER_MAX_MARKETS must be greater than zero");
    }
    if cli.max_pages == 0 {
        bail!("MARKET_MAKER_MAX_PAGES must be greater than zero");
    }
    if cli.order_size <= Decimal::ZERO {
        bail!("MARKET_MAKER_ORDER_SIZE must be greater than zero");
    }
    if cli.edge_ticks == 0 {
        bail!("MARKET_MAKER_EDGE_TICKS must be greater than zero");
    }
    if cli.min_spread_ticks == 0 {
        bail!("MARKET_MAKER_MIN_SPREAD_TICKS must be greater than zero");
    }
    if cli.max_book_spread_ticks == 0 {
        bail!("MARKET_MAKER_MAX_BOOK_SPREAD_TICKS must be greater than zero");
    }
    if cli.min_top_depth < Decimal::ZERO {
        bail!("MARKET_MAKER_MIN_TOP_DEPTH cannot be negative");
    }
    if cli
        .event_slug
        .as_deref()
        .is_some_and(|slug| slug.trim().is_empty())
    {
        bail!("MARKET_MAKER_EVENT_SLUG cannot be empty");
    }
    if cli.min_price <= Decimal::ZERO || cli.min_price >= Decimal::ONE {
        bail!("MARKET_MAKER_MIN_PRICE must be between 0 and 1");
    }
    if cli.max_price <= Decimal::ZERO || cli.max_price >= Decimal::ONE {
        bail!("MARKET_MAKER_MAX_PRICE must be between 0 and 1");
    }
    if cli.min_price >= cli.max_price {
        bail!("MARKET_MAKER_MIN_PRICE must be less than MARKET_MAKER_MAX_PRICE");
    }
    if cli.max_collateral_per_market <= Decimal::ZERO {
        bail!("MARKET_MAKER_MAX_COLLATERAL_PER_MARKET must be greater than zero");
    }
    if cli.max_loss_per_market <= Decimal::ZERO {
        bail!("MARKET_MAKER_MAX_LOSS_PER_MARKET must be greater than zero");
    }
    if cli.max_total_collateral <= Decimal::ZERO {
        bail!("MARKET_MAKER_MAX_TOTAL_COLLATERAL must be greater than zero");
    }
    if cli.min_free_collateral < Decimal::ZERO {
        bail!("MARKET_MAKER_MIN_FREE_COLLATERAL cannot be negative");
    }
    if cli.max_open_orders_per_token == 0 {
        bail!("MARKET_MAKER_MAX_OPEN_ORDERS_PER_TOKEN must be greater than zero");
    }
    if cli.live {
        if cli.private_key.as_deref().is_none_or(str::is_empty) {
            bail!("--live requires KUEST_PRIVATE_KEY or --private-key");
        }
        if cli.deposit_wallet.as_deref().is_none_or(str::is_empty) {
            bail!("--live requires KUEST_DEPOSIT_WALLET or --deposit-wallet");
        }
        let Some(chain_id) = cli.chain_id else {
            bail!(
                "--live requires KUEST_CHAIN_ID or --chain-id; use 137 for Polygon or 80002 for Amoy"
            );
        };
        if chain_id != POLYGON && chain_id != AMOY {
            bail!("unsupported chain id {chain_id}; SDK supports {POLYGON} and {AMOY}");
        }
    }
    Ok(())
}
