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
    #[serde(default)]
    pub block_timestamp: i64,
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
    let mut receipt = receipt;
    anyhow::ensure!(
        hex_number(&header["timestamp"])? > 0,
        "Invalid finalized block timestamp"
    );
    receipt["blockTimestamp"] = header["timestamp"].clone();
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
            block_timestamp: i64::try_from(
                receipt
                    .get("blockTimestamp")
                    .map(hex_number)
                    .transpose()?
                    .unwrap_or(0),
            )?,
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
    pub entry_order_id: String,
    pub shares: Decimal,
    pub cost_basis: Decimal,
    pub cycle_pnl: Decimal,
}
#[derive(Default, Debug, Serialize)]
pub struct Projection {
    #[serde(skip)]
    pub(crate) outcome_order: BTreeMap<String, (u64, u64, u64)>,
    pub inventory: BTreeMap<String, Inventory>,
    pub realized_pnl: Decimal,
    pub fees: Decimal,
    /// One outcome for a completely closed copied position, not per partial fill.
    pub closed_outcomes: Vec<(String, Decimal, i64)>,
    /// Source lots reconstructed from finalized fills in chain order.
    #[serde(skip)]
    pub source_entries: Vec<crate::copy_ledger::CopyEntry>,
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
    let mut source_ledger = crate::copy_ledger::CopyLedger::new_in_memory();
    let mut order_totals = BTreeMap::<String, Decimal>::new();
    let mut order_spend = BTreeMap::<String, Decimal>::new();
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
        if let Some(budget) = record.order.budget_usd {
            anyhow::ensure!(
                fill.side == TradeSide::BUY && budget > Decimal::ZERO,
                "Invalid budget order in execution journal"
            );
            anyhow::ensure!(
                fill.collateral <= fill.shares * record.order.price + Decimal::new(1, 6),
                "Budget fill exceeds the signed price cap"
            );
            let spent = order_spend.entry(fill.order_id.clone()).or_default();
            *spent += fill.collateral + fill.fee;
            anyhow::ensure!(
                *spent <= budget + Decimal::new(1, 6),
                "Settled fill exceeds fee-inclusive USDC budget"
            );
        } else {
            let total = order_totals.entry(fill.order_id.clone()).or_default();
            *total += fill.shares;
            anyhow::ensure!(
                *total <= record.order.size,
                "Settled shares exceed order size"
            );
        }
        let position = result.inventory.entry(fill.token_id.clone()).or_default();
        let source = record
            .source_wallet
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Owned fill has no source wallet"))?
            .to_lowercase();
        result.fees += fill.fee;
        match fill.side {
            TradeSide::BUY => {
                let fill_cost = fill.collateral + fill.fee;
                if let Some(entry) = source_ledger
                    .entries
                    .iter_mut()
                    .find(|entry| entry.entry_order_id.as_deref() == Some(fill.order_id.as_str()))
                {
                    let remaining = entry.filled_size.unwrap_or(entry.size);
                    let next_remaining = remaining + fill.shares;
                    entry.entry_price =
                        (remaining * entry.entry_price + fill_cost) / next_remaining;
                    entry.size += fill.shares;
                    entry.filled_size = Some(next_remaining);
                    entry.closed = false;
                    entry.closed_at = None;
                } else {
                    source_ledger.record_copy(
                        fill.token_id.clone(),
                        source,
                        fill.shares,
                        fill_cost / fill.shares,
                    );
                    let entry = source_ledger.entries.last_mut().unwrap();
                    entry.entry_order_id = Some(fill.order_id.clone());
                    entry.filled_size = Some(fill.shares);
                    entry.copied_at = chrono::DateTime::from_timestamp(fill.block_timestamp, 0)
                        .ok_or_else(|| anyhow::anyhow!("Invalid fill timestamp"))?;
                }
                if position.shares == Decimal::ZERO {
                    position.entry_order_id = fill.order_id.clone();
                }
                position.shares += fill.shares;
                position.cost_basis += fill_cost;
            }
            TradeSide::SELL => {
                anyhow::ensure!(
                    fill.shares <= position.shares,
                    "Sale exceeds copier-owned inventory"
                );
                let cost = source_ledger.record_source_sale(
                    &fill.token_id,
                    &source,
                    fill.shares,
                    fill.collateral / fill.shares,
                    fill.fee,
                )?;
                let pnl = fill.collateral - fill.fee - cost;
                position.shares -= fill.shares;
                position.cost_basis -= cost;
                position.cycle_pnl += pnl;
                result.realized_pnl += pnl;
                if position.shares == Decimal::ZERO {
                    position.cost_basis = Decimal::ZERO;
                    let id = format!("{}:{}", fill.transaction_hash, fill.log_index);
                    result.outcome_order.insert(
                        id.clone(),
                        (fill.block_number, fill.transaction_index, fill.log_index),
                    );
                    result
                        .closed_outcomes
                        .push((id, position.cycle_pnl, fill.block_timestamp));
                    position.cycle_pnl = Decimal::ZERO;
                }
            }
        }
    }
    for (order_id, record) in owned {
        if record.order.side != TradeSide::BUY
            || source_ledger.entries.iter().any(|entry| {
                entry
                    .entry_order_id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(&order_id))
            })
        {
            continue;
        }
        // Legacy journal entries can predate source attribution. They must
        // remain inert while unfilled; a real fill is rejected above.
        let Some(source) = record.source_wallet.as_deref() else {
            continue;
        };
        let at = chrono::DateTime::from_timestamp(record.created_at, 0)
            .ok_or_else(|| anyhow::anyhow!("Invalid order timestamp"))?;
        source_ledger.entries.push(crate::copy_ledger::CopyEntry {
            entry_order_id: Some(order_id),
            token_id: record.order.token_id.clone(),
            source_wallet: source.to_lowercase(),
            size: record.order.size,
            filled_size: Some(Decimal::ZERO),
            realized_shares: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            entry_price: record.order.price,
            source_notional_usd: None,
            copied_at: at,
            closed: record.terminal_unfilled,
            closed_at: record.terminal_unfilled.then_some(at),
        });
    }
    result.source_entries = source_ledger.entries;
    Ok(result)
}

pub fn stored() -> anyhow::Result<Vec<Fill>> {
    match std::fs::read_to_string("settlement-fills.json") {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}
pub fn sync_risk(risk: &mut crate::risk::RiskEngine) -> anyhow::Result<()> {
    let book = accounted(&stored()?, &crate::execution_journal::snapshot()?)?;
    risk.sync_live_snapshot(&book.closed_outcomes)
}
pub fn accounted(fills: &[Fill], journal: &BTreeMap<String, Record>) -> anyhow::Result<Projection> {
    let mut book = project(fills, journal)?;
    let redemptions = crate::live_redemption::records()?;
    crate::redemption_settlement::apply(&mut book, &redemptions, fills)?;
    for record in redemptions.values() {
        let proof = record.proof.as_ref().unwrap(); // verified by apply above
        let settled_at = chrono::DateTime::from_timestamp(proof.block_timestamp, 0)
            .ok_or_else(|| anyhow::anyhow!("Invalid redemption timestamp"))?;
        for (token, asset) in &record.assets {
            let owned: Decimal = book
                .source_entries
                .iter()
                .filter(|entry| !entry.closed && entry.token_id == *token)
                .map(|entry| entry.filled_size.unwrap_or(entry.size))
                .sum();
            anyhow::ensure!(
                owned == asset.shares,
                "Source lots differ from redeemed shares"
            );
            for entry in book
                .source_entries
                .iter_mut()
                .filter(|entry| !entry.closed && entry.token_id == *token)
            {
                let shares = entry.filled_size.unwrap_or(entry.size);
                entry.realized_shares += shares;
                entry.realized_pnl += shares * (asset.payout_per_share - entry.entry_price);
                entry.filled_size = Some(Decimal::ZERO);
                entry.closed = true;
                entry.closed_at = Some(settled_at);
            }
        }
    }
    Ok(book)
}
pub fn persist(fills: &[Fill]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create("settlement-fills.json.tmp")?;
    file.write_all(&serde_json::to_vec_pretty(fills)?)?;
    file.sync_all()?;
    std::fs::rename("settlement-fills.json.tmp", "settlement-fills.json")?;
    #[cfg(unix)]
    std::fs::File::open(".")?.sync_all()?;
    Ok(())
}

pub async fn collect(
    clob: &crate::clients::AuthedClobClient,
    funder: &str,
) -> anyhow::Result<(Vec<Fill>, BTreeMap<String, Record>)> {
    use polymarket_client_sdk::clob::types::{request::TradesRequest, TradeStatusType};
    let journal = crate::execution_journal::snapshot()?;
    let owned: std::collections::HashSet<_> = journal
        .values()
        .filter(|r| r.status == "accepted")
        .filter_map(|r| r.order_id.as_ref().map(|id| id.to_lowercase()))
        .collect();
    let mut fills = stored()?;
    project(&fills, &journal)?;
    if owned.is_empty() {
        return Ok((fills, journal));
    }
    let earliest = journal
        .values()
        .filter(|r| r.status == "accepted")
        .map(|r| r.created_at)
        .min()
        .unwrap();
    let request = TradesRequest::builder()
        .after(earliest.saturating_sub(60))
        .build();
    let mut cursor = None;
    let mut cursors = std::collections::HashSet::new();
    let mut transactions = std::collections::BTreeSet::new();
    let mut complete = false;
    for _ in 0..100 {
        let page = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            clob.trades(&request, cursor),
        )
        .await??;
        for trade in page.data {
            if trade.status == TradeStatusType::Confirmed
                && (owned.contains(&trade.taker_order_id.to_lowercase())
                    || trade
                        .maker_orders
                        .iter()
                        .any(|m| owned.contains(&m.order_id.to_lowercase())))
            {
                transactions.insert(format!("{:#x}", trade.transaction_hash));
            }
        }
        if page.next_cursor.is_empty() || page.next_cursor == "LTE=" {
            complete = true;
            break;
        }
        anyhow::ensure!(
            cursors.insert(page.next_cursor.clone()),
            "Repeated fill cursor"
        );
        cursor = Some(page.next_cursor);
    }
    anyhow::ensure!(complete, "Fill pagination exceeded safety limit");
    let known: std::collections::BTreeSet<_> =
        fills.iter().map(|f| f.transaction_hash.clone()).collect();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("polycopier-settlement/0.1")
        .build()?;
    let rpc = std::env::var("POLYGON_RPC_URL")
        .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".into());
    for tx in transactions.difference(&known) {
        if let Some(receipt) = finalized_receipt(&client, &rpc, tx).await? {
            let decoded = decode_receipt(&receipt, tx, u64::MAX, funder, &journal)?;
            anyhow::ensure!(
                !decoded.is_empty(),
                "Owned transaction has no supported CTF V2 settlement event"
            );
            fills.extend(decoded);
        }
    }
    project(&fills, &journal)?;
    persist(&fills)?;
    // An absent open order is not evidence of cancellation. Query its exact
    // owned ID and release a zero-filled entry only on explicit CANCELED.
    for record in journal.values().filter(|r| {
        r.status == "accepted" && r.order.side == TradeSide::BUY && !r.terminal_unfilled
    }) {
        let id = record.order_id.as_ref().unwrap();
        if fills.iter().any(|f| f.order_id.eq_ignore_ascii_case(id)) {
            continue;
        }
        let order =
            tokio::time::timeout(std::time::Duration::from_secs(5), clob.order(id)).await??;
        anyhow::ensure!(
            order.id.eq_ignore_ascii_case(id)
                && order.asset_id.to_string() == record.order.token_id,
            "Unexpected entry order response"
        );
        if order.status == polymarket_client_sdk::clob::types::OrderStatusType::Canceled
            && order.size_matched == Decimal::ZERO
        {
            crate::execution_journal::cancel_unfilled(id)?;
        }
    }
    Ok((fills, crate::execution_journal::snapshot()?))
}

pub async fn apply(
    fills: &[Fill],
    journal: &BTreeMap<String, Record>,
    state: &std::sync::Arc<tokio::sync::RwLock<crate::state::BotState>>,
    ledger: &std::sync::Arc<tokio::sync::Mutex<crate::copy_ledger::CopyLedger>>,
) -> anyhow::Result<()> {
    let projection = accounted(fills, journal)?;
    let mut durable_ledger = ledger.lock().await;
    let mut ledger = durable_ledger.clone();
    let journal_buy_ids: std::collections::HashSet<String> = journal
        .values()
        .filter(|record| record.status == "accepted" && record.order.side == TradeSide::BUY)
        .filter_map(|record| record.order_id.as_ref().map(|id| id.to_lowercase()))
        .collect();
    ledger.entries.retain(|entry| {
        entry
            .entry_order_id
            .as_ref()
            .is_none_or(|id| !journal_buy_ids.contains(&id.to_lowercase()))
    });
    anyhow::ensure!(
        ledger.entries.iter().all(|entry| entry.closed),
        "Active live source lot has no accepted order in the execution journal"
    );
    ledger
        .entries
        .extend(projection.source_entries.iter().cloned());
    for (token, inventory) in &projection.inventory {
        let active = ledger
            .entries
            .iter()
            .filter(|entry| !entry.closed && entry.token_id == *token)
            .collect::<Vec<_>>();
        let shares: Decimal = active
            .iter()
            .map(|entry| entry.filled_size.unwrap_or(entry.size))
            .sum();
        let basis: Decimal = active
            .iter()
            .map(|entry| entry.filled_size.unwrap_or(entry.size) * entry.entry_price)
            .sum();
        anyhow::ensure!(
            shares == inventory.shares
                && (basis - inventory.cost_basis).abs() <= Decimal::new(1, 6),
            "Source lots differ from confirmed live inventory"
        );
    }
    ledger.try_save()?;
    *durable_ledger = ledger;
    let mut state = state.write().await;
    state.copier_positions = projection
        .inventory
        .iter()
        .filter(|(_, p)| p.shares > Decimal::ZERO)
        .map(|(token, p)| {
            (
                token.clone(),
                crate::models::Position {
                    token_id: token.clone(),
                    size: p.shares,
                    average_entry_price: p.cost_basis / p.shares,
                },
            )
        })
        .collect();
    state.realized_pnl = projection.realized_pnl;
    state.last_live_fill_sync = chrono::Utc::now().timestamp();
    state.live_fills_ready = true;
    Ok(())
}
pub async fn reconcile(
    clob: &crate::clients::AuthedClobClient,
    funder: &str,
    state: &std::sync::Arc<tokio::sync::RwLock<crate::state::BotState>>,
    ledger: &std::sync::Arc<tokio::sync::Mutex<crate::copy_ledger::CopyLedger>>,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("polycopier-redemption-settlement/0.1")
        .build()?;
    crate::live_redemption::reconcile_receipts(&client, funder).await?;
    let (fills, journal) = collect(clob, funder).await?;
    apply(&fills, &journal, state, ledger).await
}
pub fn start(
    clob: crate::clients::AuthedClobClient,
    funder: String,
    state: std::sync::Arc<tokio::sync::RwLock<crate::state::BotState>>,
    ledger: std::sync::Arc<tokio::sync::Mutex<crate::copy_ledger::CopyLedger>>,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            if reconcile(&clob, &funder, &state, &ledger).await.is_err() {
                state.write().await.live_fills_ready = false;
                tracing::error!("Live settlement reconciliation failed; new execution blocked");
            }
        }
    });
}
