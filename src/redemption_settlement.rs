//! Receipt evidence for redemption: exact owned ERC-1155 debits/burns and pUSD payout.
use alloy::primitives::{keccak256, U256};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, str::FromStr};

pub const CTF: &str = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045";
const PUSD: &str = "0xc011a7e12a19f7b1f670d46f03b03f3342e82dfb";
const ZERO: &str = "0x0000000000000000000000000000000000000000";
const ROUTES: [&str; 5] = [
    ZERO,
    "0xada100db00ca00073811820692005400218fce1f",
    "0xada2005600dec949baf300f4c6120000bdb6eaab",
    "0xd91e80cf2e7be2e162c6513ced06f1dd0da35296",
    "0xa1200000d0002264c9a1698e001292d00e1b00af",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub entry_order_id: String,
    pub shares: Decimal,
    pub payout_per_share: Decimal,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proof {
    pub transaction_hash: String,
    pub block_hash: String,
    pub block_number: u64,
    pub transaction_index: u64,
    pub block_timestamp: i64,
    pub payouts: BTreeMap<String, Decimal>,
    pub exit_logs: BTreeMap<String, u64>,
}
fn number(v: &Value) -> anyhow::Result<u64> {
    Ok(u64::from_str_radix(
        v.as_str()
            .and_then(|s| s.strip_prefix("0x"))
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
fn address(v: &Value) -> anyhow::Result<String> {
    let s = hash(v)?;
    anyhow::ensure!(s[2..26].bytes().all(|b| b == b'0'), "Invalid address topic");
    Ok(format!("0x{}", &s[26..]))
}
fn words(v: &Value) -> anyhow::Result<Vec<U256>> {
    let s = v
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .ok_or_else(|| anyhow::anyhow!("Invalid event data"))?;
    anyhow::ensure!(
        s.len() % 64 == 0 && s.len() <= 128_256 && s.bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid ABI data"
    );
    (0..s.len())
        .step_by(64)
        .map(|i| U256::from_str_radix(&s[i..i + 64], 16).map_err(Into::into))
        .collect()
}
fn amount(n: U256) -> anyhow::Result<Decimal> {
    Ok(Decimal::from_str(&n.to_string())? / Decimal::from(1_000_000))
}
fn quantities(single: bool, w: &[U256]) -> anyhow::Result<Vec<(String, Decimal)>> {
    if single {
        anyhow::ensure!(w.len() == 2, "Invalid single transfer ABI");
        return Ok(vec![(w[0].to_string(), amount(w[1])?)]);
    }
    anyhow::ensure!(w.len() >= 4 && w[0] == U256::from(64), "Invalid batch ABI");
    let count = usize::try_from(w[2])?;
    anyhow::ensure!(
        count <= 1000
            && w.len() == 4 + 2 * count
            && w[1] == U256::from((3 + count) * 32)
            && w[3 + count] == w[2],
        "Invalid batch arrays"
    );
    (0..count)
        .map(|i| Ok((w[3 + i].to_string(), amount(w[4 + count + i])?)))
        .collect()
}

pub fn decode(
    receipt: &Value,
    tx: &str,
    funder: &str,
    assets: &BTreeMap<String, Asset>,
) -> anyhow::Result<Proof> {
    anyhow::ensure!(
        !assets.is_empty()
            && receipt["status"] == "0x1"
            && hash(&receipt["transactionHash"])?.eq_ignore_ascii_case(tx),
        "Unsuccessful or unrelated redemption receipt"
    );
    anyhow::ensure!(
        funder.len() == 42
            && funder.starts_with("0x")
            && funder[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid funder"
    );
    let funder = funder.to_lowercase();
    let single = format!(
        "{:#x}",
        keccak256("TransferSingle(address,address,address,uint256,uint256)")
    );
    let batch = format!(
        "{:#x}",
        keccak256("TransferBatch(address,address,address,uint256[],uint256[])")
    );
    let transfer = format!("{:#x}", keccak256("Transfer(address,address,uint256)"));
    let mut ordered = BTreeMap::new();
    for log in receipt["logs"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing redemption logs"))?
    {
        anyhow::ensure!(log["removed"] != true, "Removed redemption log");
        let i = number(&log["logIndex"])?;
        if let Some(previous) = ordered.insert(i, log) {
            anyhow::ensure!(previous == log, "Conflicting receipt logs");
        }
    }
    let mut debits = BTreeMap::<String, Decimal>::new();
    let mut burns = BTreeMap::<String, Decimal>::new();
    let mut pending = BTreeMap::<(String, String), Decimal>::new();
    let mut exits = BTreeMap::new();
    let mut paid = Decimal::ZERO;
    for (index, log) in ordered {
        let contract = log["address"].as_str().unwrap_or_default().to_lowercase();
        let topics = log["topics"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing topics"))?;
        let signature = topics
            .first()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        if contract == PUSD && signature == transfer {
            anyhow::ensure!(topics.len() == 3, "Invalid collateral topics");
            let from = address(&topics[1])?;
            let to = address(&topics[2])?;
            if from == funder || to == funder {
                let w = words(&log["data"])?;
                anyhow::ensure!(w.len() == 1, "Invalid collateral transfer");
                let value = amount(w[0])?;
                if to == funder {
                    paid += value;
                }
                if from == funder {
                    paid -= value;
                }
            }
        }
        if contract != CTF || (signature != single && signature != batch) {
            continue;
        }
        anyhow::ensure!(topics.len() == 4, "Invalid position topics");
        let from = address(&topics[2])?;
        let to = address(&topics[3])?;
        let amounts = quantities(signature == single, &words(&log["data"])?)?;
        for (token, value) in amounts {
            if value == Decimal::ZERO {
                continue;
            }
            if from == funder {
                anyhow::ensure!(
                    assets.contains_key(&token) && ROUTES.contains(&to.as_str()),
                    "Redemption includes foreign inventory or unknown route"
                );
                *debits.entry(token.clone()).or_default() += value;
                exits.entry(token.clone()).or_insert(index);
                if to == ZERO {
                    *burns.entry(token.clone()).or_default() += value;
                } else {
                    *pending.entry((to.clone(), token.clone())).or_default() += value;
                }
            }
            if to == funder {
                anyhow::bail!("Unexpected incoming position during redemption");
            }
            if from != funder {
                let attributed = pending
                    .get(&(from.clone(), token.clone()))
                    .copied()
                    .unwrap_or_default();
                if attributed > Decimal::ZERO {
                    anyhow::ensure!(
                        value <= attributed && ROUTES.contains(&to.as_str()) && from != to,
                        "Ambiguous pooled redemption transfer"
                    );
                    *pending.get_mut(&(from.clone(), token.clone())).unwrap() -= value;
                    if to == ZERO {
                        *burns.entry(token.clone()).or_default() += value;
                    } else {
                        *pending.entry((to.clone(), token.clone())).or_default() += value;
                    }
                } else if to != ZERO
                    && pending
                        .get(&(to.clone(), token.clone()))
                        .is_some_and(|n| *n > Decimal::ZERO)
                {
                    anyhow::bail!("Another wallet mixed into the redemption route");
                }
            }
        }
    }
    let mut payouts = BTreeMap::new();
    for (token, asset) in assets {
        anyhow::ensure!(
            asset.shares > Decimal::ZERO
                && (asset.payout_per_share == Decimal::ZERO
                    || asset.payout_per_share == Decimal::ONE),
            "Invalid redemption assets"
        );
        anyhow::ensure!(
            debits.get(token) == Some(&asset.shares) && burns.get(token) == Some(&asset.shares),
            "Owned redemption quantities differ from receipt"
        );
        payouts.insert(token.clone(), asset.shares * asset.payout_per_share);
    }
    anyhow::ensure!(
        paid == payouts.values().copied().sum::<Decimal>(),
        "Actual collateral payout differs from resolution"
    );
    let timestamp = i64::try_from(number(&receipt["blockTimestamp"])?)?;
    anyhow::ensure!(timestamp > 0, "Missing finalized redemption timestamp");
    Ok(Proof {
        transaction_hash: hash(&receipt["transactionHash"])?,
        block_hash: hash(&receipt["blockHash"])?,
        block_number: number(&receipt["blockNumber"])?,
        transaction_index: number(&receipt["transactionIndex"])?,
        block_timestamp: timestamp,
        payouts,
        exit_logs: exits,
    })
}

pub async fn prepare(
    client: &reqwest::Client,
    market_id: &str,
    funder: &str,
    book: &crate::live_fills::Projection,
) -> anyhow::Result<(BTreeMap<String, Asset>, Vec<String>)> {
    anyhow::ensure!(
        !market_id.is_empty()
            && market_id.len() <= 30
            && market_id.bytes().all(|b| b.is_ascii_digit()),
        "Invalid market ID"
    );
    anyhow::ensure!(
        funder.len() == 42
            && funder.starts_with("0x")
            && funder[2..].bytes().all(|b| b.is_ascii_hexdigit()),
        "Invalid funder"
    );
    let market: Value = client
        .get(format!(
            "https://gamma-api.polymarket.com/markets/{market_id}"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(
        market["id"].as_str() == Some(market_id),
        "Market ID mismatch"
    );
    let tokens: Vec<String> = if let Some(raw) = market["clobTokenIds"].as_str() {
        serde_json::from_str(raw)?
    } else {
        serde_json::from_value(market["clobTokenIds"].clone())?
    };
    anyhow::ensure!(
        tokens.len() == 2 && tokens[0] != tokens[1],
        "Unsupported outcome mapping"
    );
    let condition = hash(&market["conditionId"])?;
    let resolved: Value = client
        .get(format!("https://clob.polymarket.com/markets/{condition}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(
        resolved["closed"] == true && resolved["accepting_orders"] == false,
        "Market trading is not closed"
    );
    let rpc = std::env::var("POLYGON_RPC_URL")
        .unwrap_or_else(|_| "https://polygon-bor-rpc.publicnode.com".into());
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
            "Redemption RPC read failed"
        );
        Ok(response["result"].clone())
    }
    anyhow::ensure!(
        number(&call(client, &rpc, "eth_chainId", serde_json::json!([])).await?)? == 137,
        "Wrong redemption chain"
    );
    let header = call(
        client,
        &rpc,
        "eth_getBlockByNumber",
        serde_json::json!(["latest", false]),
    )
    .await?;
    let height = header["number"].clone();
    let block_hash = hash(&header["hash"])?;
    let operator = if market["negRisk"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("Market protocol metadata missing"))?
    {
        ROUTES[2]
    } else {
        ROUTES[1]
    };
    let approval_selector = format!("{:#x}", keccak256("isApprovedForAll(address,address)"));
    let approval_data = format!(
        "{}{:0>64}{:0>64}",
        &approval_selector[..10],
        &funder[2..],
        &operator[2..]
    );
    let approved = call(
        client,
        &rpc,
        "eth_call",
        serde_json::json!([{"to":CTF,"data":approval_data},height]),
    )
    .await?;
    let approval = words(&approved)?;
    anyhow::ensure!(
        approval.len() == 1 && approval[0] == U256::from(1),
        "Wallet redemption permission missing"
    );
    let selector = format!("{:#x}", keccak256("balanceOf(address,uint256)"));
    let mut assets = BTreeMap::new();
    let all_tokens = tokens.clone();
    for token in tokens {
        let token_word = U256::from_str(&token)?;
        let data = format!(
            "{}{:0>64}{:064x}",
            &selector[..10],
            &funder[2..],
            token_word
        );
        let raw = call(
            client,
            &rpc,
            "eth_call",
            serde_json::json!([{"to":CTF,"data":data},height]),
        )
        .await?;
        let w = words(&raw)?;
        anyhow::ensure!(w.len() == 1, "Invalid position balance response");
        let held = amount(w[0])?;
        let own = book
            .inventory
            .get(&token)
            .map(|p| p.shares)
            .unwrap_or_default();
        anyhow::ensure!(
            held == own,
            "Shared market contains holdings outside copier inventory"
        );
        if own > Decimal::ZERO {
            let payout = crate::redemption::payout(&resolved, &token)
                .ok_or_else(|| anyhow::anyhow!("Ambiguous market resolution"))?;
            assets.insert(
                token.clone(),
                Asset {
                    entry_order_id: book.inventory[&token].entry_order_id.clone(),
                    shares: own,
                    payout_per_share: payout,
                },
            );
        }
    }
    let canonical = call(
        client,
        &rpc,
        "eth_getBlockByNumber",
        serde_json::json!([height, false]),
    )
    .await?;
    anyhow::ensure!(
        hash(&canonical["hash"])? == block_hash && !assets.is_empty(),
        "Redemption balance snapshot invalid or empty"
    );
    Ok((assets, all_tokens))
}

pub fn apply(
    book: &mut crate::live_fills::Projection,
    records: &BTreeMap<String, crate::live_redemption::Record>,
    fills: &[crate::live_fills::Fill],
) -> anyhow::Result<()> {
    let mut records: Vec<_> = records.values().collect();
    records.sort_by_key(|r| {
        r.proof
            .as_ref()
            .map(|p| (p.block_number, p.block_timestamp))
            .unwrap_or_default()
    });
    for record in records {
        anyhow::ensure!(
            record.status == "confirmed",
            "Redemption outcome is pending reconciliation"
        );
        let proof = record
            .proof
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Redemption receipt proof missing"))?;
        anyhow::ensure!(
            record.transaction_hash.as_deref() == Some(&proof.transaction_hash)
                && !record.assets.is_empty()
                && proof.payouts.len() == record.assets.len()
                && proof.exit_logs.len() == record.assets.len(),
            "Redemption journal proof mismatch"
        );
        let mut assets: Vec<_> = record.assets.iter().collect();
        assets.sort_by_key(|(t, _)| proof.exit_logs.get(*t).copied().unwrap_or(u64::MAX));
        for (token, asset) in assets {
            anyhow::ensure!(
                !fills
                    .iter()
                    .any(|f| f.token_id == *token && f.block_number >= proof.block_number),
                "Position traded after its redemption"
            );
            let position = book
                .inventory
                .get_mut(token)
                .ok_or_else(|| anyhow::anyhow!("Redemption inventory missing"))?;
            anyhow::ensure!(
                position
                    .entry_order_id
                    .eq_ignore_ascii_case(&asset.entry_order_id)
                    && position.shares == asset.shares,
                "Redemption snapshot differs from confirmed inventory"
            );
            let paid = proof
                .payouts
                .get(token)
                .ok_or_else(|| anyhow::anyhow!("Redemption payout missing"))?;
            anyhow::ensure!(
                *paid == asset.shares * asset.payout_per_share,
                "Redemption payout allocation changed"
            );
            let pnl = *paid - position.cost_basis;
            book.realized_pnl += pnl;
            let id = format!("redeem:{}:{token}", proof.transaction_hash);
            book.outcome_order.insert(
                id.clone(),
                (
                    proof.block_number,
                    proof.transaction_index,
                    proof.exit_logs[token],
                ),
            );
            book.closed_outcomes
                .push((id, position.cycle_pnl + pnl, proof.block_timestamp));
            position.shares = Decimal::ZERO;
            position.cost_basis = Decimal::ZERO;
            position.cycle_pnl = Decimal::ZERO;
        }
    }
    book.closed_outcomes
        .sort_by_key(|(id, _, _)| book.outcome_order[id]);
    Ok(())
}
