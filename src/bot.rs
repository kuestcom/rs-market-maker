use std::time::Duration;

use anyhow::{Context as _, Result};
use kuest_client_sdk::clob::{Client, Config};
use kuest_client_sdk::gamma::Client as GammaClient;
use tokio::time::sleep;

use crate::config::Cli;
use crate::discovery::{
    discover_event_markets, discover_markets, market_key, select_candidates,
    select_event_candidates,
};
use crate::orders::{RiskBudget, authenticate, quote_market};
use crate::state::SeenMarkets;

pub async fn run(cli: Cli) -> Result<()> {
    let public_client = Client::new(&cli.clob_host, Config::default())
        .with_context(|| format!("failed to create CLOB client for {}", cli.clob_host))?;
    let gamma_client = if cli.event_slug.is_some() {
        Some(
            GammaClient::new(&cli.gamma_host)
                .with_context(|| format!("failed to create Gamma client for {}", cli.gamma_host))?,
        )
    } else {
        None
    };
    let live = if cli.live {
        Some(authenticate(&cli).await?)
    } else {
        None
    };

    let mut seen = SeenMarkets::load(&cli.state_path)?;

    for cycle in 1..=cli.cycles {
        let candidates = if let Some(event_slug) = cli.event_slug.as_deref() {
            let event_slug = event_slug.trim();
            println!(
                "cycle {cycle}/{}: discovering markets for event {event_slug}",
                cli.cycles
            );
            let gamma_client = gamma_client
                .as_ref()
                .expect("gamma client exists when event slug is configured");
            let markets =
                discover_event_markets(&public_client, gamma_client, event_slug, cli.max_pages)
                    .await?;
            select_event_candidates(markets, cli.max_markets)
        } else {
            println!("cycle {cycle}/{}: discovering markets", cli.cycles);
            let markets = discover_markets(&public_client, cli.discovery, cli.max_pages).await?;
            let candidates = select_candidates(markets, &mut seen, cli.max_markets);
            seen.save(&cli.state_path)?;
            candidates
        };

        let new_count = candidates
            .iter()
            .filter(|candidate| candidate.is_new)
            .count();
        if let Some(event_slug) = cli.event_slug.as_deref() {
            println!(
                "event {}: found {} tradable markets",
                event_slug.trim(),
                candidates.len()
            );
        } else {
            println!(
                "found {} tradable fork-scoped markets ({} new)",
                candidates.len(),
                new_count
            );
        }

        for candidate in &candidates {
            let marker = if candidate.is_new { "new" } else { "seen" };
            println!(
                "- [{marker}] {} :: {}",
                market_key(&candidate.market),
                candidate.market.question
            );
        }

        if cli.discover_only {
            continue;
        }

        let mut risk_budget = RiskBudget::new(cli.max_total_collateral);
        for candidate in candidates {
            quote_market(
                &public_client,
                live.as_ref(),
                &candidate.market,
                &cli,
                &mut risk_budget,
            )
            .await?;
        }

        if cycle < cli.cycles {
            sleep(Duration::from_secs(cli.refresh_secs)).await;
        }
    }

    Ok(())
}
