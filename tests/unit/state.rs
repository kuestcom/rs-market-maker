use std::fs;

use rs_market_maker::state::{FillLedger, FillRecord, PauseState, SeenMarkets};
use rust_decimal_macros::dec;

#[test]
fn missing_state_file_loads_as_empty() {
    let path = unique_state_path("missing");
    let _ = fs::remove_file(&path);

    let seen = SeenMarkets::load(&path).expect("missing state should load");

    assert!(seen.markets.is_empty());
}

#[test]
fn state_round_trips_seen_markets() {
    let path = unique_state_path("round-trip");
    let _ = fs::remove_file(&path);

    let mut seen = SeenMarkets::default();
    assert!(seen.mark_new("market-a".to_string()));
    assert!(!seen.mark_new("market-a".to_string()));
    assert!(seen.mark_new("market-b".to_string()));
    seen.save(&path).expect("state should save");

    let loaded = SeenMarkets::load(&path).expect("state should load");

    assert_eq!(loaded.markets.len(), 2);
    assert!(loaded.markets.contains("market-a"));
    assert!(loaded.markets.contains("market-b"));

    let _ = fs::remove_file(&path);
}

#[test]
fn pause_state_round_trips_and_clears() {
    let path = unique_state_path("pause");
    let _ = fs::remove_file(&path);

    let saved =
        PauseState::save_reason(&path, "risk breach test").expect("pause state should save");
    let loaded = PauseState::load(&path)
        .expect("pause state should load")
        .expect("pause state should exist");

    assert_eq!(loaded.reason, "risk breach test");
    assert_eq!(loaded.created_at_unix_secs, saved.created_at_unix_secs);
    assert!(PauseState::clear(&path).expect("pause state should clear"));
    assert!(!PauseState::clear(&path).expect("missing pause state should be ok"));
    assert!(
        PauseState::load(&path)
            .expect("missing pause state should load")
            .is_none()
    );
}

#[test]
fn fill_ledger_round_trips_and_filters_by_token() {
    let path = unique_state_path("fills");
    let _ = fs::remove_file(&path);

    let mut ledger = FillLedger::default();
    assert!(ledger.upsert(FillRecord {
        id: "trade-a".to_owned(),
        token_id: "1".to_owned(),
        market: "market-a".to_owned(),
        side: "BUY".to_owned(),
        size: dec!(5),
        price: dec!(0.40),
        status: "Matched".to_owned(),
        matched_at_unix_secs: 2,
    }));
    assert!(ledger.upsert(FillRecord {
        id: "trade-b".to_owned(),
        token_id: "2".to_owned(),
        market: "market-a".to_owned(),
        side: "BUY".to_owned(),
        size: dec!(1),
        price: dec!(0.50),
        status: "Matched".to_owned(),
        matched_at_unix_secs: 1,
    }));
    ledger.save(&path).expect("fill ledger should save");

    let loaded = FillLedger::load(&path).expect("fill ledger should load");
    let token_records = loaded.records_for_token("1");

    assert_eq!(token_records.len(), 1);
    assert_eq!(token_records[0].id, "trade-a");

    let _ = fs::remove_file(path);
}

fn unique_state_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "rs-market-maker-{name}-{}-{}.json",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}
