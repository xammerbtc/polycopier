//! Authenticates and reads status/balance only. Never submits/cancels/redeems orders.
use anyhow::Result;

async fn check() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| ".env.credentials".into());
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
        toml::from_str(&std::fs::read_to_string("config.toml")?)?;
    polycopier::config::validate_config(&cfg)?;
    let config = polycopier::config::Config::from_parts(
        std::env::var("PRIVATE_KEY")?,
        std::env::var("FUNDER_ADDRESS")?,
        cfg,
    );
    let (_, balance, client) = polycopier::clients::build_order_submitter(&config).await?;
    client.ok().await?;
    balance().await?;
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
    println!("PASS: configured signature authentication, balance API and VPS trading availability. No orders or on-chain transactions submitted.");
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
