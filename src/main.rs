use anyhow::Result;
use clap::Parser as _;
use rs_market_maker::bot::run;
use rs_market_maker::config::{Cli, validate_cli};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    validate_cli(&cli)?;
    run(cli).await
}
