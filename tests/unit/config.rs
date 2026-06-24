use std::path::PathBuf;

use kuest_client_sdk::types::Decimal;
use rs_market_maker::config::{Cli, DiscoveryMode, QuoteSides, validate_cli};
use rust_decimal_macros::dec;

#[test]
fn empty_event_slug_is_rejected() {
    let mut cli = valid_cli();
    cli.event_slug = Some("  ".to_owned());

    let error = validate_cli(&cli).expect_err("empty event slug should fail");

    assert!(error.to_string().contains("MARKET_MAKER_EVENT_SLUG"));
}

#[test]
fn invalid_price_band_is_rejected() {
    let mut cli = valid_cli();
    cli.min_price = dec!(0.60);
    cli.max_price = dec!(0.40);

    let error = validate_cli(&cli).expect_err("invalid price band should fail");

    assert!(error.to_string().contains("MARKET_MAKER_MIN_PRICE"));
}

#[test]
fn zero_open_order_limit_is_rejected() {
    let mut cli = valid_cli();
    cli.max_open_orders_per_token = 0;

    let error = validate_cli(&cli).expect_err("zero open order limit should fail");

    assert!(
        error
            .to_string()
            .contains("MARKET_MAKER_MAX_OPEN_ORDERS_PER_TOKEN")
    );
}

fn valid_cli() -> Cli {
    Cli {
        clob_host: "https://clob.kuest.com".to_owned(),
        gamma_host: "https://gamma-api.polymarket.com".to_owned(),
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
        quote_sides: QuoteSides::Buy,
        allow_single_sided: true,
        respect_reward_min_size: false,
        cancel_before_quote: true,
        post_only: true,
        require_two_sided_live: true,
        min_price: dec!(0.05),
        max_price: dec!(0.95),
        max_collateral_per_market: dec!(25),
        max_total_collateral: dec!(50),
        min_free_collateral: Decimal::ONE,
        max_open_orders_per_token: 2,
        discover_only: false,
        cycles: 1,
        refresh_secs: 30,
        state_path: PathBuf::from("state/seen-markets.json"),
    }
}
