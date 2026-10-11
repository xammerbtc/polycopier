//! Verifies a public target-wallet redemption receipt; never loads credentials.
use alloy::primitives::U256;
use polycopier::redemption_settlement::{self, Asset};
use rust_decimal::Decimal;
use std::{collections::BTreeMap, str::FromStr};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let frozen: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/ctf-public-redemption-receipt.json"
    ))?;
    let tx = frozen["transactionHash"].as_str().unwrap();
    let data = frozen["logs"][0]["data"]
        .as_str()
        .unwrap()
        .strip_prefix("0x")
        .unwrap();
    let token = U256::from_str_radix(&data[4 * 64..5 * 64], 16)?.to_string();
    let assets = BTreeMap::from([(
        token,
        Asset {
            entry_order_id: "public-fixture".into(),
            shares: Decimal::from_str("114.980296")?,
            payout_per_share: Decimal::ONE,
        },
    )]);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("polycopier-redemption-receipt-preflight/0.1")
        .build()?;
    let rpc = std::env::var("POLYGON_RPC_URL")
        .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".into());
    let receipt = polycopier::live_fills::finalized_receipt(&client, &rpc, tx)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Public redemption not finalized"))?;
    let proof = redemption_settlement::decode(
        &receipt,
        tx,
        "0x2d2d2b32e5199edf833594a99b9c54e5668ab69d",
        &assets,
    )?;
    anyhow::ensure!(
        proof.block_hash == frozen["blockHash"].as_str().unwrap().to_lowercase(),
        "Public receipt changed"
    );
    println!("Public redemption receipt: canonical finalized burn and pUSD payout verified; transactions submitted: 0");
    Ok(())
}
