use std::time::Duration;

use anyhow::{Context as _, Result};
use kuest_client_sdk::clob::{Client, Config};
use tokio::time::sleep;

use crate::config::Cli;
use crate::discovery::{discover_markets, market_key, select_candidates};
use crate::orders::{authenticate, quote_market};
use crate::state::SeenMarkets;

pub async fn run(cli: Cli) -> Result<()> {
    let public_client = Client::new(&cli.clob_host, Config::default())
        .with_context(|| format!("failed to create CLOB client for {}", cli.clob_host))?;
    let live = if cli.live {
        Some(authenticate(&cli).await?)
    } else {
        None
    };

    let mut seen = SeenMarkets::load(&cli.state_path)?;

    for cycle in 1..=cli.cycles {
        println!("cycle {cycle}/{}: discovering markets", cli.cycles);

        let markets = discover_markets(&public_client, cli.discovery, cli.max_pages).await?;
        let candidates = select_candidates(markets, &mut seen, cli.max_markets);
        seen.save(&cli.state_path)?;

        let new_count = candidates
            .iter()
            .filter(|candidate| candidate.is_new)
            .count();
        println!(
            "found {} tradable fork-scoped markets ({} new)",
            candidates.len(),
            new_count
        );

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

        for candidate in candidates {
            quote_market(&public_client, live.as_ref(), &candidate.market, &cli).await?;
        }

        if cycle < cli.cycles {
            sleep(Duration::from_secs(cli.refresh_secs)).await;
        }
    }

    Ok(())
}
