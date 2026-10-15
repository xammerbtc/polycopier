//! Preview settlement uses authoritative winner flags, never an inferred price.
use futures_util::StreamExt;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

pub fn settle_paper(
    state: &mut crate::state::BotState,
    token: &str,
    payout: Decimal,
) -> Option<(Decimal, Decimal)> {
    if payout != Decimal::ZERO && payout != Decimal::ONE {
        return None;
    }
    let position = state.positions.remove(token)?;
    let proceeds = position.size * payout;
    let pnl = proceeds - position.size * position.average_entry_price;
    state.total_balance += proceeds;
    state.realized_pnl += pnl;
    let cycle_pnl = state.paper_cycle_pnl.remove(token).unwrap_or_default() + pnl;
    crate::simulation::record_outcome(state, token, cycle_pnl);
    Some((proceeds, cycle_pnl))
}

pub fn payout(market: &Value, token: &str) -> Option<Decimal> {
    if market["closed"].as_bool() != Some(true) {
        return None;
    }
    let tokens = market["tokens"].as_array()?;
    // Ambiguous / void / split resolutions need a payout vector, not winner flags.
    if tokens.len() != 2
        || tokens
            .iter()
            .filter(|t| t["winner"].as_bool() == Some(true))
            .count()
            != 1
    {
        return None;
    }
    let outcome = tokens
        .iter()
        .find(|t| t["token_id"].as_str() == Some(token))?;
    match outcome["winner"].as_bool()? {
        true => Some(Decimal::ONE),
        false => Some(Decimal::ZERO),
    }
}

pub async fn resolved_payout(
    client: &reqwest::Client,
    token: &str,
) -> anyhow::Result<Option<Decimal>> {
    Ok(resolved_payouts(client, &[token.to_string()])
        .await?
        .remove(token))
}

/// Fetch Gamma metadata in bounded batches, then query CLOB once per unique
/// closed market instead of once per held outcome token. Gamma expects repeated
/// `clob_token_ids` parameters; comma-joining IDs is not equivalent.
pub async fn resolved_payouts(
    client: &reqwest::Client,
    tokens: &[String],
) -> anyhow::Result<HashMap<String, Decimal>> {
    Ok(resolved_payouts_with_failures(client, tokens).await?.0)
}

async fn resolved_payouts_with_failures(
    client: &reqwest::Client,
    tokens: &[String],
) -> anyhow::Result<(HashMap<String, Decimal>, usize)> {
    const GAMMA_BATCH_SIZE: usize = 20;
    const GAMMA_URL: &str = "https://gamma-api.polymarket.com/markets";

    let requested: HashSet<String> = tokens.iter().cloned().collect();
    let mut gamma_markets = Vec::new();
    for chunk in tokens.chunks(GAMMA_BATCH_SIZE) {
        let mut query: Vec<(&str, &str)> = chunk
            .iter()
            .map(|token| ("clob_token_ids", token.as_str()))
            .collect();
        // Gamma filters out resolved markets unless explicitly requested.
        query.push(("closed", "true"));
        let response = get_json_with_retry(client, GAMMA_URL, &query).await?;
        if let Some(markets) = response.as_array() {
            gamma_markets.extend(markets.iter().cloned());
        }
    }

    let groups = closed_market_groups(&Value::Array(gamma_markets), &requested)?;
    let mut results = HashMap::new();
    let settled = futures_util::stream::iter(groups)
        .map(|(condition, tokens)| {
            let client = client.clone();
            async move {
                let url = format!("https://clob.polymarket.com/markets/{condition}");
                (
                    condition,
                    tokens,
                    get_json_with_retry(&client, &url, &[]).await,
                )
            }
        })
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;

    let mut market_lookup_failures = 0;
    for (condition, market_tokens, response) in settled {
        match response {
            Ok(market) => {
                for token in market_tokens {
                    if let Some(value) = payout(&market, &token) {
                        results.insert(token, value);
                    }
                }
            }
            Err(error) => {
                market_lookup_failures += 1;
                tracing::warn!("Paper settlement lookup failed for market {condition}: {error}")
            }
        }
    }
    Ok((results, market_lookup_failures))
}

fn closed_market_groups(
    markets: &Value,
    requested: &HashSet<String>,
) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    let markets = markets
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Invalid Gamma markets response"))?;
    let mut groups: HashMap<String, HashSet<String>> = HashMap::new();
    for market in markets {
        if market["closed"].as_bool() != Some(true) {
            continue;
        }
        let Some(condition) = market["conditionId"].as_str() else {
            continue;
        };
        if !condition.starts_with("0x")
            || !matches!(condition.len(), 64 | 66)
            || !condition[2..].chars().all(|c| c.is_ascii_hexdigit())
        {
            continue;
        }
        let token_ids = crate::markets::tokens(market)?;
        let matched = token_ids
            .into_iter()
            .filter(|token| requested.contains(token))
            .collect::<Vec<_>>();
        if !matched.is_empty() {
            groups
                .entry(condition.to_string())
                .or_default()
                .extend(matched);
        }
    }
    let mut result = groups
        .into_iter()
        .map(|(condition, tokens)| (condition, tokens.into_iter().collect::<Vec<_>>()))
        .collect::<Vec<_>>();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

async fn get_json_with_retry(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
) -> anyhow::Result<Value> {
    for attempt in 0..4u32 {
        let response = client.get(url).query(query).send().await.map_err(|error| {
            anyhow::anyhow!("Polymarket API transport error: {}", error.without_url())
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let retry_after = retry_after_delay(
                response.headers().get(reqwest::header::RETRY_AFTER),
                chrono::Utc::now(),
            );
            if attempt == 3 {
                // Respect an explicit cooldown even when this lookup has exhausted
                // its retries, so the worker does not immediately renew a 429.
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    if let Some(delay) = retry_after {
                        tokio::time::sleep(delay).await;
                    }
                }
                anyhow::bail!("Polymarket API returned HTTP {status} after retries");
            }
            let backoff = 250u64.saturating_mul(1u64 << attempt).min(4000);
            tokio::time::sleep(retry_after.unwrap_or(std::time::Duration::from_millis(backoff)))
                .await;
            continue;
        }
        if !status.is_success() {
            anyhow::bail!("Polymarket API returned HTTP {status}");
        }
        return response.json().await.map_err(|error| {
            anyhow::anyhow!(
                "Polymarket API response JSON error: {}",
                error.without_url()
            )
        });
    }
    anyhow::bail!("Request retries exhausted")
}

fn retry_after_delay(
    value: Option<&reqwest::header::HeaderValue>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let value = value?.to_str().ok()?.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(std::time::Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        (date.with_timezone(&chrono::Utc) - now)
            .to_std()
            .unwrap_or_default(),
    )
}

/// Automatically settle paper inventory only after Gamma marks a market closed and
/// the CLOB confirms an unambiguous binary payout. Live redemption remains explicit.
pub fn start_paper_settlement(
    state: Arc<RwLock<crate::state::BotState>>,
    ledger: Arc<Mutex<crate::copy_ledger::CopyLedger>>,
) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .user_agent("Polycopier/1.0")
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .tcp_nodelay(true)
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!("Paper settlement worker could not create HTTP client: {error}");
                return;
            }
        };
        let mut next_delay = std::time::Duration::from_secs(15);
        loop {
            // Look quickly after a new closure or settlement, but avoid hammering
            // unchanged, unresolved markets every 15 seconds indefinitely.
            tokio::time::sleep(next_delay).await;
            let candidates: Vec<String> = {
                let guard = state.read().await;
                guard
                    .positions
                    .keys()
                    .filter(|token| {
                        guard
                            .position_details
                            .get(*token)
                            .is_some_and(|details| details.closed)
                    })
                    .cloned()
                    .collect()
            };
            if candidates.is_empty() {
                next_delay = std::time::Duration::from_secs(15);
                let mut guard = state.write().await;
                guard.paper_settlement_market_failures = 0;
                guard.paper_settlement_last_error = None;
                continue;
            }
            state.write().await.paper_settlement_last_attempt =
                Some(chrono::Utc::now().timestamp());
            let resolved = match resolved_payouts_with_failures(&client, &candidates).await {
                Ok((payouts, failures)) => {
                    let mut guard = state.write().await;
                    guard.paper_settlement_last_lookup = Some(chrono::Utc::now().timestamp());
                    guard.paper_settlement_last_error = None;
                    guard.paper_settlement_market_failures = failures;
                    payouts.into_iter().collect::<Vec<_>>()
                }
                Err(error) => {
                    next_delay = std::time::Duration::from_secs(60);
                    tracing::warn!("Paper settlement batch lookup failed: {error}");
                    let mut guard = state.write().await;
                    guard.paper_settlement_last_error =
                        Some(if error.to_string().contains("429") {
                            "Gamma rate limited the lookup".to_string()
                        } else {
                            "Gamma lookup failed; retrying".to_string()
                        });
                    continue;
                }
            };
            if resolved.is_empty() {
                next_delay = std::time::Duration::from_secs(60);
                continue;
            }
            next_delay = std::time::Duration::from_secs(15);

            let mut ledger = ledger.lock().await;
            let mut guard = state.write().await;
            if guard.paper_integrity_fault {
                continue;
            }
            let old_ledger = ledger.clone();
            let old_positions = guard.positions.clone();
            let old_balance = guard.total_balance;
            let old_pnl = guard.realized_pnl;
            let old_cycle = guard.paper_cycle_pnl.clone();
            let old_outcomes = guard.paper_outcomes.clone();
            let mut settled: Vec<(String, Decimal, Decimal)> = Vec::new();
            let mut failed = false;
            for (token, payout) in resolved {
                // A pending order only blocks settlement of its own token. Other
                // resolved markets can safely credit cash while trading continues.
                if guard.pending_orders.contains_key(&token) {
                    continue;
                }
                if let Some((proceeds, pnl)) = settle_paper(&mut guard, &token, payout) {
                    if let Err(error) = ledger.settle_token(&token, payout) {
                        tracing::error!("Paper source settlement failed for {token}: {error}");
                        failed = true;
                        break;
                    }
                    settled.push((token, proceeds, pnl));
                }
            }
            if !failed && !settled.is_empty() {
                if let Err(error) = ledger
                    .try_save()
                    .and_then(|_| crate::simulation::commit(&mut guard))
                {
                    tracing::error!(
                        "Paper settlement persistence failed; restoring state: {error}"
                    );
                    failed = true;
                }
            }
            if failed {
                guard.started = true;
                guard.paper_integrity_fault = true;
                *ledger = old_ledger;
                ledger.save();
                guard.positions = old_positions;
                guard.total_balance = old_balance;
                guard.realized_pnl = old_pnl;
                guard.paper_cycle_pnl = old_cycle;
                guard.paper_outcomes = old_outcomes;
                continue;
            }
            for (token, _, pnl) in &settled {
                guard.position_prices.remove(token);
                guard.price_updated_at.remove(token);
                if let Some(tx) = &guard.risk_outcome_tx {
                    let _ = tx.send(*pnl);
                }
            }
            if !settled.is_empty() {
                tracing::info!(
                    "Automatic paper settlement confirmed {} resolved positions",
                    settled.len()
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_accepts_seconds_and_http_date_without_truncation() {
        let now = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            retry_after_delay(Some(&reqwest::header::HeaderValue::from_static("120")), now),
            Some(std::time::Duration::from_secs(120))
        );
        assert_eq!(
            retry_after_delay(
                Some(&reqwest::header::HeaderValue::from_static(
                    "Wed, 01 Jan 2025 00:00:20 GMT"
                )),
                now
            ),
            Some(std::time::Duration::from_secs(20))
        );
    }

    #[test]
    fn closed_market_groups_deduplicate_outcome_tokens_by_condition() {
        let condition = format!("0x{}", "a".repeat(64));
        let markets = serde_json::json!([
            {
                "closed": true,
                "conditionId": condition,
                "clobTokenIds": "[\"yes\",\"no\"]"
            },
            {
                "closed": false,
                "conditionId": format!("0x{}", "b".repeat(64)),
                "clobTokenIds": "[\"open\",\"other\"]"
            }
        ]);
        let requested = HashSet::from(["yes".to_string(), "no".to_string(), "open".to_string()]);
        let groups = closed_market_groups(&markets, &requested).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 2);
        assert!(groups[0].1.contains(&"yes".to_string()));
        assert!(groups[0].1.contains(&"no".to_string()));
    }

    #[test]
    fn redeem_remaining_shares_reports_one_combined_cycle_without_double_credit() {
        let mut state = crate::state::BotState::new(true, Some(Decimal::from(194)));
        state.positions.insert(
            "yes".into(),
            crate::models::Position {
                token_id: "yes".into(),
                size: Decimal::from(10),
                average_entry_price: Decimal::new(6, 1),
            },
        );
        assert_eq!(
            crate::simulation::sell(
                &mut state,
                "yes",
                Decimal::from(9),
                Decimal::new(5, 1),
                Decimal::ZERO
            )
            .unwrap(),
            None
        );
        assert_eq!(
            settle_paper(&mut state, "yes", Decimal::ONE),
            Some((Decimal::ONE, Decimal::new(-5, 1)))
        );
        assert_eq!(state.realized_pnl, Decimal::new(-5, 1));
        assert_eq!(state.total_balance, Decimal::new(1995, 1));
        assert!(state.paper_cycle_pnl.is_empty());
        assert_eq!(settle_paper(&mut state, "yes", Decimal::ONE), None);
    }
    #[test]
    fn paper_settlement_credits_once_and_realizes_winner_and_loser() {
        use crate::{models::Position, state::BotState};
        let mut state = BotState::new(true, Some(Decimal::from(190)));
        for token in ["yes", "no"] {
            state.positions.insert(
                token.into(),
                Position {
                    token_id: token.into(),
                    size: Decimal::from(10),
                    average_entry_price: Decimal::new(5, 1),
                },
            );
        }
        assert_eq!(settle_paper(&mut state, "yes", Decimal::new(5, 1)), None);
        assert_eq!(state.positions.len(), 2);
        assert_eq!(
            settle_paper(&mut state, "yes", Decimal::ONE),
            Some((Decimal::from(10), Decimal::from(5)))
        );
        assert_eq!(settle_paper(&mut state, "yes", Decimal::ONE), None);
        assert_eq!(
            settle_paper(&mut state, "no", Decimal::ZERO),
            Some((Decimal::ZERO, Decimal::from(-5)))
        );
        assert_eq!(state.total_balance, Decimal::from(200));
        assert_eq!(state.realized_pnl, Decimal::ZERO);
        assert!(state.positions.is_empty());
    }
    #[test]
    fn requires_closed_unambiguous_resolution_and_exact_token() {
        let mut market = serde_json::json!({"closed":true,"tokens":[{"token_id":"yes","winner":true},{"token_id":"no","winner":false}]});
        assert_eq!(payout(&market, "yes"), Some(Decimal::ONE));
        assert_eq!(payout(&market, "no"), Some(Decimal::ZERO));
        assert_eq!(payout(&market, "other"), None);
        market["closed"] = false.into();
        assert_eq!(payout(&market, "yes"), None);
        market["closed"] = true.into();
        market["tokens"][0]["winner"] = false.into();
        assert_eq!(payout(&market, "no"), None);
        market["tokens"][0]["winner"] = true.into();
        market["tokens"][1]["winner"] = true.into();
        assert_eq!(payout(&market, "yes"), None);
    }
}
