//! Authenticates and reads status/balance only. Never submits/cancels/redeems orders.
//! Usage: live_preflight [credentials_file] [config_file] [minimum_usdc]
use anyhow::Result;

async fn check() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| ".env.credentials".into());
    let config_path = args
        .next()
        .unwrap_or_else(|| "deploy/config.live-1000.toml".into());
    let minimum_usdc: rust_decimal::Decimal = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or_default();
    anyhow::ensure!(args.next().is_none(), "Too many preflight arguments");
    anyhow::ensure!(
        minimum_usdc >= rust_decimal::Decimal::ZERO,
        "Invalid minimum balance"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            std::fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
            "Credential file permissions are too broad"
        );
    }
    dotenvy::from_filename(&path)?;
    let cfg: polycopier::config::BotConfig =
        toml::from_str(&std::fs::read_to_string(&config_path)?)?;
    polycopier::config::validate_config(&cfg)?;
    let config = polycopier::config::Config::from_parts(
        std::env::var("PRIVATE_KEY")?,
        std::env::var("FUNDER_ADDRESS")?,
        cfg,
    );
    let (_, balance, client) = polycopier::clients::build_order_submitter(&config).await?;
    client.ok().await?;
    let available_usdc = balance().await?;
    let geoblock: serde_json::Value = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .get("https://polymarket.com/api/geoblock")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(
        geoblock["blocked"].as_bool() == Some(false),
        "VPS trading availability not confirmed"
    );
    if available_usdc < minimum_usdc {
        println!("Insufficient CLOB USDC: available ${available_usdc}, required ${minimum_usdc}. No orders or on-chain transactions submitted.");
        anyhow::bail!("Insufficient CLOB collateral for staged bank");
    }
    println!("PASS: configured signature authentication, balance API and VPS trading availability. Available CLOB USDC: ${available_usdc}. No orders or on-chain transactions submitted.");
    Ok(())
}
#[tokio::main]
async fn main() {
    if check().await.is_err() {
        // SDK/network error objects can contain credential-bearing request context.
        eprintln!(
            "Preflight failed. Inspect configuration and endpoints without printing credentials."
        );
        std::process::exit(1);
    }
}
