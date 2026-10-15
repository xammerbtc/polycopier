use crate::state::BotState;
use futures_util::{stream, StreamExt};
use rust_decimal::Decimal;
use std::{str::FromStr, sync::Arc};
use tokio::sync::RwLock;

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
            let quotes: Vec<_> = stream::iter(tokens)
                .map(|token| {
                    let client = client.clone();
                    let cached = market_cache
                        .get(&token)
                        .filter(|(_, fetched)| fetched.elapsed().as_secs() < 60)
                        .map(|(details, _)| details.clone());
                    async move {
                        let midpoint = async {
                            let response = client
                                .get("https://clob.polymarket.com/midpoint")
                                .query(&[("token_id", &token)])
                                .send()
                                .await
                                .ok()?
                                .error_for_status()
                                .ok()?;
                            let value: serde_json::Value = response.json().await.ok()?;
                            let price = Decimal::from_str(value["mid"].as_str()?).ok()?;
                            (price >= Decimal::ZERO && price <= Decimal::ONE).then_some(price)
                        };
                        let mid = midpoint.await;
                        let (details, refreshed) = if let Some(cached) = cached {
                            (Some(cached), false)
                        } else {
                            let details = crate::markets::fetch(&client, &token)
                                .await
                                .ok()
                                .and_then(|market| crate::markets::details(&market, &token).ok());
                            (details, true)
                        };
                        let price =
                            mid.or_else(|| details.as_ref().and_then(|m| m.reference_price));
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
                    }
                })
                .buffer_unordered(8)
                .collect()
                .await;
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
