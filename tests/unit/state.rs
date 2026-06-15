use std::fs;

use rs_market_maker::state::SeenMarkets;

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

fn unique_state_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "rs-market-maker-{name}-{}-{}.json",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}
