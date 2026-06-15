use kuest_client_sdk::clob::types::response::OrderSummary;
use kuest_client_sdk::types::Decimal;
use rust_decimal_macros::dec;

pub(crate) fn best_bid(levels: &[OrderSummary]) -> Option<Decimal> {
    levels
        .iter()
        .map(|level| level.price)
        .max_by(|left, right| left.cmp(right))
}

pub(crate) fn best_ask(levels: &[OrderSummary]) -> Option<Decimal> {
    levels
        .iter()
        .map(|level| level.price)
        .min_by(|left, right| left.cmp(right))
}

pub fn fair_price(
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
    token_price: Decimal,
    last_trade_price: Option<Decimal>,
) -> Decimal {
    if let (Some(bid), Some(ask)) = (best_bid, best_ask)
        && bid > Decimal::ZERO
        && ask > bid
    {
        return (bid + ask) / Decimal::from(2);
    }

    if is_valid_probability(token_price) {
        return token_price;
    }

    if let Some(last_trade_price) = last_trade_price
        && is_valid_probability(last_trade_price)
    {
        return last_trade_price;
    }

    dec!(0.5)
}

pub fn quote_prices(
    fair_price: Decimal,
    best_bid: Option<Decimal>,
    best_ask: Option<Decimal>,
    tick: Decimal,
    edge_ticks: u32,
    min_spread_ticks: u32,
) -> (Option<Decimal>, Option<Decimal>) {
    let fair_price = clamp_probability(fair_price, tick);
    let edge = tick * Decimal::from(edge_ticks);
    let min_spread = tick * Decimal::from(min_spread_ticks);

    let buy_cap = fair_price - edge;
    let sell_floor = fair_price + edge;
    let passive_buy = best_bid
        .map(|price| price + tick)
        .unwrap_or(fair_price - min_spread);
    let passive_sell = best_ask
        .map(|price| price - tick)
        .unwrap_or(fair_price + min_spread);

    let buy = floor_to_tick(min_decimal(passive_buy, buy_cap), tick);
    let sell = ceil_to_tick(max_decimal(passive_sell, sell_floor), tick);

    let buy = if valid_buy(buy, best_ask, tick) {
        Some(buy)
    } else {
        None
    };
    let sell = if valid_sell(sell, best_bid, tick) {
        Some(sell)
    } else {
        None
    };

    match (buy, sell) {
        (Some(buy), Some(sell)) if sell - buy < min_spread => (None, None),
        other => other,
    }
}

fn valid_buy(price: Decimal, best_ask: Option<Decimal>, tick: Decimal) -> bool {
    is_tradeable_price(price, tick) && best_ask.is_none_or(|ask| price < ask)
}

fn valid_sell(price: Decimal, best_bid: Option<Decimal>, tick: Decimal) -> bool {
    is_tradeable_price(price, tick) && best_bid.is_none_or(|bid| price > bid)
}

fn is_tradeable_price(price: Decimal, tick: Decimal) -> bool {
    price >= tick && price <= Decimal::ONE - tick
}

fn is_valid_probability(price: Decimal) -> bool {
    price > Decimal::ZERO && price < Decimal::ONE
}

fn clamp_probability(price: Decimal, tick: Decimal) -> Decimal {
    max_decimal(tick, min_decimal(Decimal::ONE - tick, price))
}

fn floor_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    (price / tick).floor() * tick
}

fn ceil_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    (price / tick).ceil() * tick
}

pub(crate) fn min_decimal(left: Decimal, right: Decimal) -> Decimal {
    if left < right { left } else { right }
}

pub(crate) fn max_decimal(left: Decimal, right: Decimal) -> Decimal {
    if left > right { left } else { right }
}
