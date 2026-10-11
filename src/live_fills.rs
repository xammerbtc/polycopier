//! Exact CTF Exchange V2 settlement accounting from finalized Polygon receipts.
//! Fees come from OrderFilled, never from current market rates or a limit price.
use crate::{execution_journal::Record, models::TradeSide};
use alloy::primitives::{keccak256, U256};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, str::FromStr};

const EXCHANGES: [&str; 2] = [
    "0xe111180000d2663c0091e4f400237545b87b996b",
    "0xe2222d279d744050d28e00520010520000310f59",
];
const SIGNATURE: &str =
    "OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub transaction_index: u64,
    pub log_index: u64,
    pub order_id: String,
    pub token_id: String,
    pub side: TradeSide,
    pub shares: Decimal,
    pub collateral: Decimal,
    pub fee: Decimal,
}

/// Read-only RPC proof. A height comparison alone is insufficient: verify the
/// receipt block hash against the canonical header at a finalized height.
pub async fn finalized_receipt(
    client: &reqwest::Client,
    rpc: &str,
    tx: &str,
) -> anyhow::Result<Option<Value>> {
    async fn call(
        client: &reqwest::Client,
        rpc: &str,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        let response: Value = client
            .post(rpc)
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::ensure!(
            response["id"] == 1 && response.get("error").is_none(),
            "RPC response failed"
        );
        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("RPC result missing"))
    }
    hash(&Value::String(tx.into()))?;
    let chain = call(client, rpc, "eth_chainId", serde_json::json!([])).await?;
    anyhow::ensure!(hex_number(&chain)? == 137, "RPC is not Polygon mainnet");
    let finalized = call(
        client,
        rpc,
        "eth_getBlockByNumber",
        serde_json::json!(["finalized", false]),
    )
    .await?;
    let height = hex_number(&finalized["number"])?;
    let receipt = call(
        client,
        rpc,
        "eth_getTransactionReceipt",
        serde_json::json!([tx]),
    )
    .await?;
    if receipt.is_null() || hex_number(&receipt["blockNumber"])? > height {
        return Ok(None);
    }
    anyhow::ensure!(
        hash(&receipt["transactionHash"])?.eq_ignore_ascii_case(tx),
        "RPC returned another transaction"
    );
    let header = call(
        client,
        rpc,
        "eth_getBlockByNumber",
        serde_json::json!([receipt["blockNumber"].clone(), false]),
    )
    .await?;
    anyhow::ensure!(
        hex_number(&header["number"])? == hex_number(&receipt["blockNumber"])?
            && hash(&header["hash"])? == hash(&receipt["blockHash"])?,
        "Receipt is not in the canonical finalized chain"
    );
    Ok(Some(receipt))
}

fn hex_number(v: &Value) -> anyhow::Result<u64> {
    Ok(u64::from_str_radix(
        v.as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing hex number"))?
            .strip_prefix("0x")
            .ok_or_else(|| anyhow::anyhow!("Invalid hex number"))?,
        16,
    )?)
}
fn hash(v: &Value) -> anyhow::Result<String> {
    let s = v.as_str().ok_or_else(|| anyhow::anyhow!("Missing hash"))?;
    anyhow::ensure!(
        s.len() == 66 && s.starts_with("0x") && s[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid hash"
    );
    Ok(s.to_lowercase())
}
fn units(v: U256) -> anyhow::Result<Decimal> {
    Ok(Decimal::from_str(&v.to_string())? / Decimal::from(1_000_000))
}

/// Only finalized, successful receipts from the two documented CTF V2 exchanges
/// are supported. V1/V3 events must use their own decoder; they are not guessed.
pub fn decode_receipt(
    receipt: &Value,
    expected_tx: &str,
    finalized_height: u64,
    funder: &str,
    journal: &BTreeMap<String, Record>,
) -> anyhow::Result<Vec<Fill>> {
    anyhow::ensure!(hex_number(&receipt["status"])? == 1, "Transaction failed");
    let transaction_hash = hash(&receipt["transactionHash"])?;
    anyhow::ensure!(
        transaction_hash.eq_ignore_ascii_case(expected_tx),
        "Receipt transaction mismatch"
    );
    let block_hash = hash(&receipt["blockHash"])?;
    let block_number = hex_number(&receipt["blockNumber"])?;
    anyhow::ensure!(block_number <= finalized_height, "Receipt is not finalized");
    let transaction_index = hex_number(&receipt["transactionIndex"])?;
    let funder = alloy::primitives::Address::from_str(funder)?;
    let mut wallet_topic = [0u8; 32];
    wallet_topic[12..].copy_from_slice(funder.as_slice());
    let wallet_topic = format!("{:#x}", alloy::primitives::B256::from(wallet_topic));
    let signature = format!("{:#x}", keccak256(SIGNATURE));
    let owned: BTreeMap<_, _> = journal
        .values()
        .filter(|r| r.status == "accepted")
        .filter_map(|r| r.order_id.as_ref().map(|id| (id.to_lowercase(), r)))
        .collect();
    let logs = receipt["logs"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Receipt logs missing"))?;
    let mut fills = BTreeMap::new();
    for log in logs {
        let address = log["address"].as_str().unwrap_or_default().to_lowercase();
        if !EXCHANGES.contains(&address.as_str()) {
            continue;
        }
        let Some(topics) = log["topics"].as_array() else {
            continue;
        };
        if topics
            .first()
            .and_then(Value::as_str)
            .is_none_or(|t| !t.eq_ignore_ascii_case(&signature))
        {
            continue;
        }
        anyhow::ensure!(topics.len() == 4, "Invalid OrderFilled topic count");
        let order_id = hash(&topics[1])?;
        let Some(record) = owned.get(&order_id) else {
            continue;
        };
        anyhow::ensure!(
            topics[2]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&wallet_topic)),
            "Owned order has different maker wallet"
        );
        anyhow::ensure!(log["removed"].as_bool() != Some(true), "Removed log");
        let data = log["data"]
            .as_str()
            .and_then(|s| s.strip_prefix("0x"))
            .ok_or_else(|| anyhow::anyhow!("Event data missing"))?;
        anyhow::ensure!(
            data.len() == 7 * 64 && data.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid OrderFilled data"
        );
        let mut words = Vec::new();
        for i in 0..7 {
            words.push(U256::from_str_radix(&data[i * 64..(i + 1) * 64], 16)?);
        }
        let side = if words[0] == U256::ZERO {
            TradeSide::BUY
        } else if words[0] == U256::from(1) {
            TradeSide::SELL
        } else {
            anyhow::bail!("Invalid event side")
        };
        anyhow::ensure!(
            side == record.order.side && words[1].to_string() == record.order.token_id,
            "Event contradicts owned order"
        );
        let (shares, collateral) = if side == TradeSide::BUY {
            (units(words[3])?, units(words[2])?)
        } else {
            (units(words[2])?, units(words[3])?)
        };
        let fee = units(words[4])?;
        anyhow::ensure!(
            shares > Decimal::ZERO && collateral > Decimal::ZERO && collateral <= shares,
            "Invalid settlement amounts"
        );
        anyhow::ensure!(
            side != TradeSide::SELL || fee <= collateral,
            "Fee exceeds sale proceeds"
        );
        let log_index = hex_number(&log["logIndex"])?;
        let fill = Fill {
            transaction_hash: transaction_hash.clone(),
            block_hash: block_hash.clone(),
            block_number,
            transaction_index,
            log_index,
            order_id,
            token_id: record.order.token_id.clone(),
            side,
            shares,
            collateral,
            fee,
        };
        if let Some(previous) = fills.insert(log_index, fill.clone()) {
            anyhow::ensure!(previous == fill, "Conflicting receipt log");
        }
    }
    Ok(fills.into_values().collect())
}

#[derive(Default, Debug, Serialize)]
pub struct Inventory {
    pub shares: Decimal,
    pub cost_basis: Decimal,
    pub cycle_pnl: Decimal,
}
#[derive(Default, Debug, Serialize)]
pub struct Projection {
    pub inventory: BTreeMap<String, Inventory>,
    pub realized_pnl: Decimal,
    pub fees: Decimal,
    /// One outcome for a completely closed copied position, not per partial fill.
    pub closed_outcomes: Vec<(String, Decimal)>,
}
pub fn project(fills: &[Fill], journal: &BTreeMap<String, Record>) -> anyhow::Result<Projection> {
    let owned: BTreeMap<_, _> = journal
        .values()
        .filter(|r| r.status == "accepted")
        .filter_map(|r| r.order_id.as_ref().map(|id| (id.to_lowercase(), r)))
        .collect();
    let mut events = BTreeMap::new();
    for fill in fills {
        anyhow::ensure!(
            owned.contains_key(&fill.order_id.to_lowercase()),
            "Fill is outside execution journal"
        );
        let key = (fill.block_number, fill.transaction_index, fill.log_index);
        if let Some(old) = events.insert(key, fill) {
            anyhow::ensure!(old == fill, "Conflicting chain order");
        }
    }
    let mut result = Projection::default();
    let mut order_totals = BTreeMap::<String, Decimal>::new();
    for fill in events.values() {
        let record = owned[&fill.order_id.to_lowercase()];
        anyhow::ensure!(
            fill.token_id == record.order.token_id
                && fill.side == record.order.side
                && fill.shares > Decimal::ZERO
                && fill.fee >= Decimal::ZERO
                && fill.collateral > Decimal::ZERO
                && fill.collateral <= fill.shares,
            "Invalid owned fill"
        );
        anyhow::ensure!(
            fill.side != TradeSide::SELL || fill.fee <= fill.collateral,
            "Fee exceeds sale proceeds"
        );
        let total = order_totals.entry(fill.order_id.clone()).or_default();
        *total += fill.shares;
        anyhow::ensure!(
            *total <= record.order.size,
            "Settled shares exceed order size"
        );
        let position = result.inventory.entry(fill.token_id.clone()).or_default();
        result.fees += fill.fee;
        match fill.side {
            TradeSide::BUY => {
                position.shares += fill.shares;
                position.cost_basis += fill.collateral + fill.fee;
            }
            TradeSide::SELL => {
                anyhow::ensure!(
                    fill.shares <= position.shares,
                    "Sale exceeds copier-owned inventory"
                );
                let cost = position.cost_basis * fill.shares / position.shares;
                let pnl = fill.collateral - fill.fee - cost;
                position.shares -= fill.shares;
                position.cost_basis -= cost;
                position.cycle_pnl += pnl;
                result.realized_pnl += pnl;
                if position.shares == Decimal::ZERO {
                    position.cost_basis = Decimal::ZERO;
                    result.closed_outcomes.push((
                        format!("{}:{}", fill.transaction_hash, fill.log_index),
                        position.cycle_pnl,
                    ));
                    position.cycle_pnl = Decimal::ZERO;
                }
            }
        }
    }
    Ok(result)
}
