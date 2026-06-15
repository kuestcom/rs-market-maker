use rs_market_maker::pricing::{fair_price, quote_prices};
use rust_decimal_macros::dec;

#[test]
fn quotes_inside_wide_book_without_crossing_fair_edge() {
    let (buy, sell) = quote_prices(
        dec!(0.50),
        Some(dec!(0.40)),
        Some(dec!(0.60)),
        dec!(0.01),
        1,
        2,
    );

    assert_eq!(buy, Some(dec!(0.41)));
    assert_eq!(sell, Some(dec!(0.59)));
}

#[test]
fn quotes_keep_configured_edge_on_tight_book() {
    let (buy, sell) = quote_prices(
        dec!(0.50),
        Some(dec!(0.49)),
        Some(dec!(0.51)),
        dec!(0.01),
        2,
        2,
    );

    assert_eq!(buy, Some(dec!(0.48)));
    assert_eq!(sell, Some(dec!(0.52)));
}

#[test]
fn quotes_refuse_prices_outside_tradeable_bounds() {
    let (buy, sell) = quote_prices(dec!(0.99), None, None, dec!(0.01), 1, 2);

    assert_eq!(buy, Some(dec!(0.97)));
    assert_eq!(sell, None);
}

#[test]
fn midpoint_is_preferred_when_book_is_two_sided() {
    let fair = fair_price(
        Some(dec!(0.44)),
        Some(dec!(0.56)),
        dec!(0.10),
        Some(dec!(0.20)),
    );

    assert_eq!(fair, dec!(0.50));
}
