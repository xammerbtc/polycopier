use crate::backoff::next_backoff;
use crate::config::Config;
use crate::models::{TradeEvent, TradeSide};
use alloy::primitives::Address;
use anyhow::Result;
use chrono::Utc;
use polymarket_client_sdk::data::types::request::ActivityRequest;
use polymarket_client_sdk::data::types::{ActivitySortBy, ActivityType, SortDirection};
use std::collections::{HashSet, VecDeque};
use std::str::FromStr;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Bounded memory for recently delivered fills.
const SEEN_HASHES_CAP: usize = 20000;
pub static LAST_POLL_SUCCESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static ACTIVE_TARGETS: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

pub async fn start_ws_listener(config: &Config, tx: mpsc::Sender<TradeEvent>) -> Result<()> {
    let mut config = config.clone();
    let data_client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .tcp_nodelay(true)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .build()?;

    tokio::spawn(async move {
        info!("Started polling target wallets");

        // Deduplicate by transaction hash instead of a timestamp watermark.
        // Timestamps are coarse (seconds) and drop burst trades - hashes are exact.
        let mut seen_hashes: HashSet<String> = HashSet::new();
        let mut seen_order: VecDeque<String> = VecDeque::new();
        // Gap 6: track consecutive API errors per target for backoff.
        let mut consecutive_errors: u32 = 0;
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            config.hot_reload();
            if let Ok(mut active) = ACTIVE_TARGETS.write() {
                *active = config.target_wallets.clone();
            }
            interval.tick().await;

            let mut had_error = false;

            for target in &config.target_wallets {
                let target_addr = match Address::from_str(target) {
                    Ok(addr) => addr,
                    Err(_) => continue,
                };

                let poll_started = std::time::Instant::now();
                let mut trades =
                    match fetch_recent(&data_client, target_addr, Utc::now().timestamp() - 300)
                        .await
                    {
                        Ok(t) => {
                            LAST_POLL_SUCCESS.store(
                                Utc::now().timestamp() as u64,
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            // Success Р Р†Р вЂљРІР‚Сњ backoff reset happens after the full loop
                            t
                        }
                        Err(e) => {
                            crate::latency::poll_error();
                            had_error = true;
                            warn!(
                                "Listener: API error for {}: {e} (consecutive_errors={})",
                                &target[..target.len().min(10)],
                                consecutive_errors + 1
                            );
                            continue; // skip this target this cycle
                        }
                    };
                crate::latency::poll_complete(poll_started.elapsed().as_millis() as u64);

                // Data API returns newest first. Preserve BUY/SELL chronology
                // when several fallback fills arrive in the same polling batch.
                trades.sort_by_key(|trade| trade.timestamp);
                for trade in trades {
                    let hash = format!(
                        "{}:{}:{}:{:?}:{}:{}",
                        trade.transaction_hash,
                        trade.proxy_wallet,
                        trade.asset,
                        trade.side,
                        trade.price,
                        trade.size
                    );

                    // Skip already-processed trades
                    if seen_hashes.contains(&hash) {
                        continue;
                    }

                    // Skip trades that are too old (more than 5 minutes)
                    // to avoid replaying history on reconnect / restart
                    let age_secs = Utc::now().timestamp() - trade.timestamp;
                    if age_secs > 300 {
                        // Still mark as seen so we don't re-evaluate on next poll
                        evict_and_insert(&mut seen_hashes, &mut seen_order, hash);
                        continue;
                    }

                    let side = match trade.side {
                        polymarket_client_sdk::data::types::Side::Buy => TradeSide::BUY,
                        _ => TradeSide::SELL,
                    };

                    let event = TradeEvent {
                        transaction_hash: trade.transaction_hash.to_string(),
                        // Normalize to lowercase: alloy Address::to_string() produces
                        // EIP-55 checksum (mixed-case). config.target_wallets stores
                        // lowercase strings from .env. Vec::contains is case-sensitive,
                        // so without normalization every live trade fails the wallet check.
                        maker_address: trade.proxy_wallet.to_string().to_lowercase(),
                        taker_address: trade.proxy_wallet.to_string().to_lowercase(),
                        token_id: trade.asset.to_string(),
                        price: trade.price,
                        size: trade.size,
                        side,
                        timestamp: trade.timestamp,
                    };

                    // Gap 10: use try_send instead of send().await to avoid
                    // blocking the polling loop when the strategy channel is full.
                    crate::latency::receive(&event, "polling");
                    match tx.try_send(event) {
                        Ok(()) => evict_and_insert(&mut seen_hashes, &mut seen_order, hash),
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            warn!("Listener: strategy channel full Р Р†Р вЂљРІР‚Сњ retrying event on next poll");
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            error!("Listener: strategy channel closed Р Р†Р вЂљРІР‚Сњ shutting down listener.");
                            return;
                        }
                    }
                }
            }

            // Gap 6: exponential backoff on sustained API errors.
            if had_error {
                consecutive_errors += 1;
                let backoff_secs = next_backoff(consecutive_errors, 2, 120);
                if consecutive_errors > 1 {
                    warn!(
                        "Listener: backing off {}s after {} consecutive errors.",
                        backoff_secs, consecutive_errors
                    );
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                }
            } else {
                consecutive_errors = 0; // reset on clean cycle
            }
        }
    });

    Ok(())
}

const PAGE_SIZE: i32 = 100;

fn page_request(wallet: Address, offset: i32, start: i64, end: i64) -> Result<ActivityRequest> {
    Ok(ActivityRequest::builder()
        .user(wallet)
        .activity_types(vec![ActivityType::Trade])
        .start(start.max(0) as u64)
        .end(end.max(0) as u64)
        .sort_by(ActivitySortBy::Timestamp)
        .sort_direction(SortDirection::Desc)
        .limit(PAGE_SIZE)?
        .offset(offset)?
        .build())
}

fn more_pages(count: usize, oldest: Option<i64>, cutoff: i64) -> bool {
    count == PAGE_SIZE as usize && oldest.is_some_and(|timestamp| timestamp >= cutoff)
}

// Use recognized time filters: an arbitrary cache-buster on /trades did not
// reliably bypass upstream snapshots. The window stays fixed across pages.
fn fresh_request(client: &reqwest::Client, request: &ActivityRequest) -> reqwest::RequestBuilder {
    client
        .get("https://data-api.polymarket.com/activity")
        .query(request)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
}

async fn fetch_recent(
    client: &reqwest::Client,
    wallet: Address,
    cutoff: i64,
) -> Result<Vec<polymarket_client_sdk::data::types::response::Trade>> {
    let mut trades = Vec::new();
    let end = Utc::now().timestamp();
    for offset in (0..=10000).step_by(PAGE_SIZE as usize) {
        let page: Vec<polymarket_client_sdk::data::types::response::Trade> =
            fresh_request(client, &page_request(wallet, offset, cutoff, end)?)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
        let more = more_pages(
            page.len(),
            page.iter().map(|trade| trade.timestamp).min(),
            cutoff,
        );
        trades.extend(page);
        if !more {
            break;
        }
        if offset == 10000 {
            crate::latency::poll_page_limit();
            warn!("Listener: pagination limit reached; five-minute coverage is incomplete");
        }
    }
    Ok(trades)
}

/// Insert a hash into the dedup set, evicting the oldest entry when capacity is reached.
fn evict_and_insert(seen: &mut HashSet<String>, order: &mut VecDeque<String>, hash: String) {
    if order.len() >= SEEN_HASHES_CAP {
        if let Some(oldest) = order.pop_front() {
            seen.remove(&oldest);
        }
    }
    seen.insert(hash.clone());
    order.push_back(hash);
}

#[cfg(test)]
mod pagination_tests {
    use super::*;

    #[test]
    fn request_includes_all_trade_activity_and_window() {
        let value =
            serde_json::to_value(page_request(Address::ZERO, 100, 700, 1000).unwrap()).unwrap();
        assert_eq!(value["type"], "TRADE");
        assert!(value.get("takerOnly").is_none());
        assert_eq!(value["limit"], 100);
        assert_eq!(value["offset"], 100);
        assert_eq!(value["start"], 700);
        assert_eq!(value["end"], 1000);
        assert_eq!(value["sortBy"], "TIMESTAMP");
        assert_eq!(value["sortDirection"], "DESC");
    }

    #[test]
    fn polling_windows_use_recognized_time_filters() {
        let client = reqwest::Client::new();
        let first = fresh_request(
            &client,
            &page_request(Address::ZERO, 100, 700, 1000).unwrap(),
        )
        .build()
        .unwrap();
        let next = fresh_request(
            &client,
            &page_request(Address::ZERO, 100, 1700, 2000).unwrap(),
        )
        .build()
        .unwrap();
        assert_ne!(first.url(), next.url());
        let params: std::collections::HashMap<_, _> = next.url().query_pairs().collect();
        assert_eq!(params["type"], "TRADE");
        assert_eq!(params["offset"], "100");
        assert_eq!(params["start"], "1700");
        assert_eq!(params["end"], "2000");
        assert_eq!(next.headers()[reqwest::header::CACHE_CONTROL], "no-cache");
    }

    #[test]
    fn full_page_at_cutoff_keeps_same_second_fills() {
        assert!(more_pages(100, Some(100), 100));
        assert!(!more_pages(100, Some(99), 100));
        assert!(!more_pages(99, Some(101), 100));
        assert!(!more_pages(0, None, 100));
    }
}
