use crate::state::BotState;
use futures_util::{stream, StreamExt};
use rust_decimal::Decimal;
use std::{str::FromStr, sync::Arc};
use tokio::sync::RwLock;

/// Independently price our holdings, even after targets remove their positions.
pub fn start(state: Arc<RwLock<BotState>>) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("price client");
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
                        let (mid, market) =
                            tokio::join!(midpoint, crate::markets::fetch(&client, &token));
                        let mut details = market
                            .ok()
                            .and_then(|m| crate::markets::details(&m, &token).ok());
                        let price =
                            mid.or_else(|| details.as_ref().and_then(|m| m.reference_price));
                        if let Some(m) = &mut details {
                            if mid.is_some() {
                                m.price_source = "CLOB midpoint".into();
                            }
                        }
                        (token, price, details)
                    }
                })
                .buffer_unordered(8)
                .collect()
                .await;
            let mut guard = state.write().await;
            guard.position_prices.clear();
            let held: std::collections::HashSet<_> =
                guard.reported_positions().keys().cloned().collect();
            guard
                .position_details
                .retain(|token, _| held.contains(token));
            for (token, price, details) in quotes {
                if !held.contains(&token) {
                    continue;
                }
                if let Some(price) = price {
                    guard.position_prices.insert(token.clone(), price);
                }
                if let Some(details) = details {
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
