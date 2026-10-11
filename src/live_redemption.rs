//! SDK bridge for manually enabled Safe/Relayer redemption, with a durable no-retry journal.
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Record {
    pub status: String,
    pub created_at: i64,
    pub transaction_hash: Option<String>,
}
pub fn records() -> anyhow::Result<BTreeMap<String, Record>> {
    match std::fs::read_to_string("redemption-journal.json") {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}
fn save(records: &BTreeMap<String, Record>) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create("redemption-journal.json.tmp")?;
    file.write_all(&serde_json::to_vec_pretty(records)?)?;
    file.sync_all()?;
    std::fs::rename("redemption-journal.json.tmp", "redemption-journal.json")?;
    #[cfg(unix)]
    std::fs::File::open(".")?.sync_all()?;
    Ok(())
}
pub fn configured() -> bool {
    std::env::var("POLYCOPIER_MODE").as_deref() == Ok("live")
        && std::env::var("LIVE_REDEEM_ENABLED").as_deref() == Ok("true")
        && [
            "PRIVATE_KEY",
            "FUNDER_ADDRESS",
            "POLYMARKET_RELAYER_API_KEY",
            "POLYMARKET_RELAYER_API_KEY_ADDRESS",
        ]
        .iter()
        .all(|key| std::env::var(key).is_ok_and(|v| !v.is_empty()))
}
pub async fn execute(market_id: &str) -> anyhow::Result<Value> {
    use tokio::io::AsyncWriteExt;
    anyhow::ensure!(
        configured(),
        "Live redemption credentials are not configured"
    );
    anyhow::ensure!(
        !market_id.is_empty()
            && market_id.len() <= 30
            && market_id.bytes().all(|b| b.is_ascii_digit()),
        "Invalid market ID"
    );
    let mut journal = records()?;
    anyhow::ensure!(
        !journal.contains_key(market_id),
        "Existing intent requires reconciliation; no automatic retry"
    );
    let node =
        std::env::var("REDEEM_NODE").unwrap_or_else(|_| "/opt/polycopier/runtime/node24".into());
    let mut child = tokio::process::Command::new(node)
        .arg("deploy/redeemer/runner.mjs")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    // Persist before delivering input; failures after broadcast never lead to an automatic retry.
    journal.insert(
        market_id.into(),
        Record {
            status: "pending_reconciliation".into(),
            created_at: chrono::Utc::now().timestamp(),
            transaction_hash: None,
        },
    );
    save(&journal)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("Missing helper input"))?;
    stdin
        .write_all(
            serde_json::json!({"market_id":market_id})
                .to_string()
                .as_bytes(),
        )
        .await?;
    drop(stdin);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        child.wait_with_output(),
    )
    .await??;
    anyhow::ensure!(
        output.status.success(),
        "Helper failed; reconcile intent before retrying"
    );
    let receipt: Value = serde_json::from_slice(&output.stdout)?;
    let hash = receipt["transaction_hash"].as_str().unwrap_or_default();
    anyhow::ensure!(
        receipt["status"] == "confirmed"
            && receipt["market_id"] == market_id
            && hash.starts_with("0x")
            && hash.len() == 66
            && hash[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid confirmed receipt"
    );
    journal.get_mut(market_id).unwrap().status = "confirmed".into();
    journal.get_mut(market_id).unwrap().transaction_hash = Some(hash.into());
    save(&journal)?;
    Ok(receipt)
}
pub async fn market_id(client: &reqwest::Client, token: &str) -> anyhow::Result<String> {
    let markets: Value = client
        .get("https://gamma-api.polymarket.com/markets")
        .query(&[("clob_token_ids", token)])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let market = markets
        .as_array()
        .and_then(|v| v.first())
        .ok_or_else(|| anyhow::anyhow!("Market not found"))?;
    let tokens: Vec<Value> = match &market["clobTokenIds"] {
        Value::Array(tokens) => tokens.clone(),
        Value::String(raw) => serde_json::from_str(raw)?,
        _ => anyhow::bail!("Market token mapping missing"),
    };
    anyhow::ensure!(
        tokens.iter().any(|id| id.as_str() == Some(token)),
        "Market token mapping mismatch"
    );
    let id = market["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Market ID missing"))?;
    anyhow::ensure!(
        !id.is_empty() && id.len() <= 30 && id.bytes().all(|b| b.is_ascii_digit()),
        "Invalid market ID"
    );
    Ok(id.into())
}
