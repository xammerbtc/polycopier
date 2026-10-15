use crate::state::BotState;
use futures_util::{stream, StreamExt};
use rust_decimal::Decimal;
use std::{str::FromStr, sync::Arc};
use tokio::sync::RwLock;

async fn fetch_midpoints(
    client: &reqwest::Client,
    tokens: &[String],
) -> std::collections::HashMap<String, Decimal> {
    let chunks: Vec<Vec<String>> = tokens.chunks(50).map(|chunk| chunk.to_vec()).collect();
    let batches = stream::iter(chunks)
        .map(|chunk| {
            let client = client.clone();
            async move {
                let body: Vec<_> = chunk
                    .iter()
                    .map(|token| serde_json::json!({ "token_id": token }))
                    .collect();
                let response = client
                    .post("https://clob.polymarket.com/midpoints")
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?;
                response
                    .json::<std::collections::HashMap<String, String>>()
                    .await
            }
        })
        .buffer_unordered(3)
        .collect::<Vec<_>>()
        .await;
    let mut midpoints = std::collections::HashMap::new();
    for result in batches {
        match result {
            Ok(batch) => {
                for (token, raw) in batch {
                    if let Ok(price) = Decimal::from_str(&raw) {
                        if price >= Decimal::ZERO && price <= Decimal::ONE {
                            midpoints.insert(token, price);
                        }
                    }
                }
            }
            Err(error) => tracing::warn!(%error, "CLOB midpoint batch failed"),
        }
    }
    midpoints
}

/// Independently price our holdings, even after targets remove their positions.
pub fn start(state: Arc<RwLock<BotState>>) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .tcp_nodelay(true)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(60))
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("price client");
        let mut market_cache = std::collections::HashMap::<
            String,
            (crate::markets::Details, std::time::Instant),
        >::new();
        loop {
            let tokens: Vec<_> = state
                .read()
                .await
                .reported_positions()
                .keys()
                .cloned()
                .collect();
            {
                let held: std::collections::HashSet<&str> =
                    tokens.iter().map(String::as_str).collect();
                market_cache.retain(|token, _| held.contains(token.as_str()));
            }
            let refresh_tokens: Vec<_> = tokens
                .iter()
                .filter(|token| {
                    market_cache.get(*token).is_none_or(|(details, fetched)| {
                        let ttl = if !details.closed && details.price_source == "Gamma reference" {
                            30
                        } else {
                            300
                        };
                        fetched.elapsed().as_secs() >= ttl
                    })
                })
                .cloned()
                .collect();
            let (batch_details, midpoints) = tokio::join!(
                crate::markets::fetch_batch(&client, &refresh_tokens),
                fetch_midpoints(&client, &tokens)
            );
            let quotes: Vec<_> = tokens
                .into_iter()
                .map(|token| {
                    let cached = market_cache
                        .get(&token)
                        .filter(|(_, fetched)| fetched.elapsed().as_secs() < 300)
                        .map(|(details, _)| details.clone());
                    let fetched = batch_details.get(&token).cloned();
                    let mid = midpoints.get(&token).copied();
                    let refreshed = fetched.is_some();
                    let details = fetched.or(cached);
                    let price = mid.or_else(|| details.as_ref().and_then(|m| m.reference_price));
                    let source = if mid.is_some() {
                        "CLOB midpoint"
                    } else {
                        "Gamma reference"
                    };
                    let details = details.map(|mut details| {
                        details.price_source = source.into();
                        details
                    });
                    (token, price, details, refreshed, mid.is_some() || refreshed)
                })
                .collect();
            let mut guard = state.write().await;
            let held: std::collections::HashSet<_> =
                guard.reported_positions().keys().cloned().collect();
            guard
                .position_prices
                .retain(|token, _| held.contains(token));
            guard
                .price_updated_at
                .retain(|token, _| held.contains(token));
            guard
                .position_details
                .retain(|token, _| held.contains(token));
            for (token, price, details, refreshed, quote_refreshed) in quotes {
                if !held.contains(&token) {
                    continue;
                }
                if let Some(price) = price {
                    guard.position_prices.insert(token.clone(), price);
                    if quote_refreshed {
                        guard
                            .price_updated_at
                            .insert(token.clone(), chrono::Utc::now().timestamp() as u64);
                    }
                }
                if let Some(details) = details {
                    if refreshed {
                        market_cache
                            .insert(token.clone(), (details.clone(), std::time::Instant::now()));
                    }
                    guard.position_details.insert(token, details);
                }
            }
            guard.unrealized_pnl = guard
                .reported_positions()
                .values()
                .filter_map(|p| {
                    guard
                        .position_prices
                        .get(&p.token_id)
                        .map(|price| (*price - p.average_entry_price) * p.size)
                })
                .sum();
            guard.prices_updated_at = chrono::Utc::now().timestamp() as u64;
            drop(guard);
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });
}
