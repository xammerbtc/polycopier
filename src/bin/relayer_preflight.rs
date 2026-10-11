//! Read-only Relayer authentication check. Never signs or submits transactions.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    dotenvy::from_filename(path).map_err(|_| anyhow::anyhow!("Credential file unavailable"))?;
    let key = std::env::var("POLYMARKET_RELAYER_API_KEY")
        .map_err(|_| anyhow::anyhow!("Relayer key missing"))?;
    let address = std::env::var("POLYMARKET_RELAYER_API_KEY_ADDRESS")
        .map_err(|_| anyhow::anyhow!("Relayer key owner missing"))?;
    anyhow::ensure!(
        !key.trim().is_empty()
            && address.starts_with("0x")
            && address.len() == 42
            && address[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid Relayer credential format"
    );
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("polycopier-relayer-preflight/0.1")
        .build()?
        .get("https://relayer-v2.polymarket.com/transactions")
        .header("RELAYER_API_KEY", key)
        .header("RELAYER_API_KEY_ADDRESS", address)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Relayer read failed"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "Relayer authentication check returned HTTP {}",
        response.status().as_u16()
    );
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid Relayer response"))?;
    anyhow::ensure!(value.is_array(), "Unexpected Relayer transaction list");
    println!("Relayer authentication: passed; transactions submitted: 0");
    Ok(())
}
