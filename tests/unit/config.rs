use std::path::PathBuf;

use kuest_client_sdk::{AMOY, types::Decimal};
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

#[test]
fn zero_loss_limit_is_rejected() {
    let mut cli = valid_cli();
    cli.max_loss_per_market = Decimal::ZERO;

    let error = validate_cli(&cli).expect_err("zero market loss limit should fail");

    assert!(
        error
            .to_string()
            .contains("MARKET_MAKER_MAX_LOSS_PER_MARKET")
    );
}

#[test]
fn zero_token_inventory_limit_is_rejected() {
    let mut cli = valid_cli();
    cli.max_inventory_per_token = Decimal::ZERO;

    let error = validate_cli(&cli).expect_err("zero token inventory limit should fail");

    assert!(
        error
            .to_string()
            .contains("MARKET_MAKER_MAX_INVENTORY_PER_TOKEN")
    );
}

#[test]
fn zero_market_inventory_limit_is_rejected() {
    let mut cli = valid_cli();
    cli.max_inventory_per_market = Decimal::ZERO;

    let error = validate_cli(&cli).expect_err("zero market inventory limit should fail");

    assert!(
        error
            .to_string()
            .contains("MARKET_MAKER_MAX_INVENTORY_PER_MARKET")
    );
}

#[test]
fn zero_max_book_spread_is_rejected() {
    let mut cli = valid_cli();
    cli.max_book_spread_ticks = 0;

    let error = validate_cli(&cli).expect_err("zero book spread limit should fail");

    assert!(
        error
            .to_string()
            .contains("MARKET_MAKER_MAX_BOOK_SPREAD_TICKS")
    );
}

#[test]
fn negative_top_depth_is_rejected() {
    let mut cli = valid_cli();
    cli.min_top_depth = dec!(-1);

    let error = validate_cli(&cli).expect_err("negative top depth should fail");

    assert!(error.to_string().contains("MARKET_MAKER_MIN_TOP_DEPTH"));
}

#[test]
fn zero_max_data_age_is_rejected() {
    let mut cli = valid_cli();
    cli.max_data_age_secs = 0;

    let error = validate_cli(&cli).expect_err("zero max data age should fail");

    assert!(error.to_string().contains("MARKET_MAKER_MAX_DATA_AGE_SECS"));
}

#[test]
fn invalid_band_margins_are_rejected() {
    let mut cli = valid_cli();
    cli.band_min_margin_ticks = Some(4);
    cli.band_avg_margin_ticks = Some(3);
    cli.band_max_margin_ticks = Some(5);

    let error = validate_cli(&cli).expect_err("invalid band margins should fail");

    assert!(
        error
            .to_string()
            .contains("MARKET_MAKER_BAND_*_MARGIN_TICKS")
    );
}

#[test]
fn invalid_band_sizes_are_rejected() {
    let mut cli = valid_cli();
    cli.band_min_size = Some(dec!(10));
    cli.band_avg_size = Some(dec!(5));
    cli.band_max_size = Some(dec!(10));

    let error = validate_cli(&cli).expect_err("invalid band sizes should fail");

    assert!(error.to_string().contains("MARKET_MAKER_BAND_*_SIZE"));
}

#[test]
fn cancel_all_requires_live_mode() {
    let mut cli = valid_cli();
    cli.cancel_all = true;

    let error = validate_cli(&cli).expect_err("cancel all without live mode should fail");

    assert!(error.to_string().contains("MARKET_MAKER_CANCEL_ALL"));
}

#[test]
fn cancel_all_on_exit_requires_live_mode() {
    let mut cli = valid_cli();
    cli.cancel_all_on_exit = true;

    let error = validate_cli(&cli).expect_err("cancel all on exit without live mode should fail");

    assert!(error.to_string().contains("MARKET_MAKER_CANCEL_ALL"));
}

#[test]
fn cancel_on_risk_breach_requires_live_mode() {
    let mut cli = valid_cli();
    cli.cancel_on_risk_breach = true;

    let error =
        validate_cli(&cli).expect_err("cancel on risk breach without live mode should fail");

    assert!(
        error
            .to_string()
            .contains("MARKET_MAKER_CANCEL_ON_RISK_BREACH")
    );
}

#[test]
fn cancel_all_modes_are_mutually_exclusive() {
    let mut cli = valid_live_cli();
    cli.cancel_all = true;
    cli.cancel_all_on_exit = true;

    let error = validate_cli(&cli).expect_err("conflicting cancel modes should fail");

    assert!(error.to_string().contains("mutually exclusive"));
}

fn valid_cli() -> Cli {
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

fn valid_live_cli() -> Cli {
    let mut cli = valid_cli();
    cli.live = true;
    cli.private_key = Some("private-key".to_owned());
    cli.deposit_wallet = Some("0x0000000000000000000000000000000000000001".to_owned());
    cli.chain_id = Some(AMOY);
    cli
}
