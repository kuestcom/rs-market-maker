use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use kuest_client_sdk::clob::types::response::MarketResponse;
use kuest_client_sdk::clob::{Client, Config};
use tokio::time::sleep;

use crate::PublicClient;
use crate::config::{Cli, validate_cli};
use crate::discovery::{
    MarketCandidate, discover_event_markets, discover_markets, market_key, select_candidates,
    select_event_candidates,
};
use crate::orders::{
    CancelOpenOrdersSummary, PreflightRiskAuditResult, RiskBudget, authenticate,
    cancel_open_orders_for_markets, preflight_risk_audit, quote_market,
};
use crate::state::{PauseState, SeenMarkets};

type ManagedScope = Arc<Mutex<Vec<MarketResponse>>>;

pub async fn run(cli: Cli) -> Result<()> {
    validate_cli(&cli)?;

    if cli.clear_pause {
        let cleared = PauseState::clear(&cli.pause_path)?;
        if cleared {
            println!("cleared pause file {}", cli.pause_path.display());
        } else {
            println!("no pause file at {}", cli.pause_path.display());
        }
        return Ok(());
    }

    let public_client = Client::new(&cli.clob_host, Config::default())
        .with_context(|| format!("failed to create CLOB client for {}", cli.clob_host))?;
    let live = if cli.live {
        Some(authenticate(&cli).await?)
    } else {
        None
    };
    let live_ref = live.as_ref();

    if cli.cancel_all {
        let live = live_ref.context("--cancel-all requires --live")?;
        return cancel_scope_orders(&public_client, live, &cli, None).await;
    }

    if cli.cancel_all_on_exit {
        let live = live_ref.context("--cancel-all-on-exit requires --live")?;
        let managed_scope = Arc::new(Mutex::new(Vec::new()));
        tokio::select! {
            result = run_cycles(&public_client, live_ref, &cli, Some(managed_scope.clone())) => result,
            signal = shutdown_signal() => {
                signal?;
                println!("shutdown signal received; canceling scoped open orders");
                cancel_scope_orders(&public_client, live, &cli, Some(managed_scope)).await
            }
        }
    } else {
        run_cycles(&public_client, live_ref, &cli, None).await
    }
}

async fn run_cycles(
    public_client: &PublicClient,
    live: Option<&crate::orders::LiveTrading>,
    cli: &Cli,
    managed_scope: Option<ManagedScope>,
) -> Result<()> {
    let mut seen = SeenMarkets::load(&cli.state_path)?;

    for cycle in 1..=cli.cycles {
        if stop_if_paused(cli)? {
            return Ok(());
        }

        let candidates = discover_cycle_candidates(public_client, cli, &mut seen, cycle).await?;
        replace_managed_scope(managed_scope.as_ref(), &candidates)?;

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

        let mut risk_budget = if let Some(live) = live {
            let markets = candidates
                .iter()
                .map(|candidate| candidate.market.clone())
                .collect::<Vec<_>>();
            match preflight_risk_audit(public_client, live, &markets, cli).await? {
                PreflightRiskAuditResult::Continue(risk_budget) => risk_budget,
                PreflightRiskAuditResult::SkipCycle => {
                    if cycle < cli.cycles {
                        sleep(Duration::from_secs(cli.refresh_secs)).await;
                    }
                    continue;
                }
                PreflightRiskAuditResult::Stop => return Ok(()),
            }
        } else {
            RiskBudget::new(cli.max_total_collateral)
        };

        for candidate in candidates {
            quote_market(
                public_client,
                live,
                &candidate.market,
                cli,
                &mut risk_budget,
            )
            .await?;
            if stop_if_paused(cli)? {
                return Ok(());
            }
        }

        if cycle < cli.cycles {
            sleep(Duration::from_secs(cli.refresh_secs)).await;
        }
    }

    Ok(())
}

fn stop_if_paused(cli: &Cli) -> Result<bool> {
    if let Some(pause) = PauseState::load(&cli.pause_path)? {
        println!(
            "paused by {}: {}",
            cli.pause_path.display(),
            pause.reason.trim()
        );
        return Ok(true);
    }

    Ok(false)
}

async fn discover_cycle_candidates(
    public_client: &PublicClient,
    cli: &Cli,
    seen: &mut SeenMarkets,
    cycle: u32,
) -> Result<Vec<MarketCandidate>> {
    if let Some(event_slug) = cli.event_slug.as_deref() {
        let event_slug = event_slug.trim();
        println!(
            "cycle {cycle}/{}: discovering markets for event {event_slug}",
            cli.cycles
        );
        let markets = discover_event_markets(public_client, event_slug, cli.max_pages).await?;
        Ok(select_event_candidates(markets, cli.max_markets))
    } else {
        println!("cycle {cycle}/{}: discovering markets", cli.cycles);
        let markets = discover_markets(public_client, cli.discovery, cli.max_pages).await?;
        let candidates = select_candidates(markets, seen, cli.max_markets);
        seen.save(&cli.state_path)?;
        Ok(candidates)
    }
}

fn replace_managed_scope(
    managed_scope: Option<&ManagedScope>,
    candidates: &[MarketCandidate],
) -> Result<()> {
    if let Some(managed_scope) = managed_scope {
        let mut managed_markets = managed_scope
            .lock()
            .map_err(|_| anyhow::anyhow!("managed market scope lock poisoned"))?;
        *managed_markets = candidates
            .iter()
            .map(|candidate| candidate.market.clone())
            .collect();
    }

    Ok(())
}

async fn cancel_scope_orders(
    public_client: &PublicClient,
    live: &crate::orders::LiveTrading,
    cli: &Cli,
    managed_scope: Option<ManagedScope>,
) -> Result<()> {
    let markets = scoped_cancel_markets(public_client, cli, managed_scope).await?;
    if markets.is_empty() {
        println!("cancel-all: no scoped markets found");
        return Ok(());
    }

    println!("cancel-all: checking {} scoped markets", markets.len());
    let summary = cancel_open_orders_for_markets(live, &markets).await?;
    print_cancel_summary(&summary);
    if summary.remaining_open > 0 {
        bail!(
            "cancel-all left {} open orders after verification",
            summary.remaining_open
        );
    }

    Ok(())
}

async fn scoped_cancel_markets(
    public_client: &PublicClient,
    cli: &Cli,
    managed_scope: Option<ManagedScope>,
) -> Result<Vec<MarketResponse>> {
    if let Some(managed_scope) = managed_scope {
        return managed_scope_markets(&managed_scope);
    }

    discover_cancel_markets(public_client, cli).await
}

fn managed_scope_markets(managed_scope: &ManagedScope) -> Result<Vec<MarketResponse>> {
    managed_scope
        .lock()
        .map_err(|_| anyhow::anyhow!("managed market scope lock poisoned"))
        .map(|managed_markets| managed_markets.clone())
}

async fn discover_cancel_markets(
    public_client: &PublicClient,
    cli: &Cli,
) -> Result<Vec<MarketResponse>> {
    if let Some(event_slug) = cli.event_slug.as_deref() {
        let event_slug = event_slug.trim();
        println!("cancel-all: discovering markets for event {event_slug}");
        let markets = discover_event_markets(public_client, event_slug, cli.max_pages).await?;
        Ok(select_event_candidates(markets, cli.max_markets)
            .into_iter()
            .map(|candidate| candidate.market)
            .collect())
    } else {
        println!("cancel-all: discovering scoped markets");
        let markets = discover_markets(public_client, cli.discovery, cli.max_pages).await?;
        let mut seen = SeenMarkets::load(&cli.state_path)?;
        Ok(select_candidates(markets, &mut seen, cli.max_markets)
            .into_iter()
            .map(|candidate| candidate.market)
            .collect())
    }
}

fn print_cancel_summary(summary: &CancelOpenOrdersSummary) {
    println!(
        "cancel-all: markets={} tokens={} found={} canceled={} not_canceled={} remaining={}",
        summary.markets_checked,
        summary.tokens_checked,
        summary.orders_found,
        summary.canceled,
        summary.not_canceled,
        summary.remaining_open
    );
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("failed to listen for SIGTERM")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for Ctrl-C")?;
            }
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl-C")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use clap::Parser as _;

    #[test]
    fn managed_scope_snapshot_preserves_empty_scope() {
        let managed_scope = Arc::new(Mutex::new(Vec::new()));

        let markets = managed_scope_markets(&managed_scope).expect("managed scope should lock");

        assert!(markets.is_empty());
    }

    #[tokio::test]
    async fn run_rejects_clear_pause_cancel_all_before_clearing_file() {
        let path = std::env::temp_dir().join(format!(
            "rs-market-maker-bot-clear-pause-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, r#"{"reason":"test","created_at_unix_secs":1}"#)
            .expect("pause file should be created");

        let pause_path = path.to_string_lossy().to_string();
        let cli = Cli::parse_from([
            "rs-market-maker",
            "--clear-pause",
            "--cancel-all",
            "--pause-path",
            pause_path.as_str(),
        ]);

        let error = run(cli)
            .await
            .expect_err("clear pause plus cancel all should fail");

        assert!(error.to_string().contains("MARKET_MAKER_CLEAR_PAUSE"));
        assert!(path.exists(), "pause file should not be cleared");

        let _ = fs::remove_file(path);
    }
}
