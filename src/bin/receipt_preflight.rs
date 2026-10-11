//! Read-only proof against a public fixture. Does not load signer credentials.
use anyhow::Result;
async fn check() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tests/fixtures/ctf-v2-public-receipt.json".into());
    let fixture: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let tx = fixture["transactionHash"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Fixture transaction missing"))?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("polycopier-receipt-preflight/0.1")
        .build()?;
    let rpc = std::env::var("POLYGON_RPC_URL")
        .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".into());
    let canonical = polycopier::live_fills::finalized_receipt(&client, &rpc, tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Transaction not finalized"))?;
    anyhow::ensure!(
        canonical["blockHash"] == fixture["blockHash"],
        "Public fixture is not canonical"
    );
    println!("PASS: public transaction receipt verified on finalized Polygon chain. No credentials loaded and no transaction submitted.");
    Ok(())
}
#[tokio::main]
async fn main() {
    if check().await.is_err() {
        eprintln!("Receipt preflight failed; inspect RPC availability and public fixture.");
        std::process::exit(1);
    }
}
