pub fn format_timestamp(ts: i64) -> String {
    // Basic formatting placeholder
    format!("{}", ts)
}

use alloy::primitives::Address;
use polymarket_client_sdk::data::Client as DataClient;
use rust_decimal::Decimal;
use serde::Deserialize;
#[cfg(test)]
use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone, Deserialize)]
pub struct PositionSnapshot {
    #[serde(alias = "token_id")]
    pub asset: String,
    #[serde(alias = "current_size")]
    pub size: Decimal,
    #[serde(alias = "avg_price")]
    pub avg_price: Decimal,
    #[serde(alias = "current_price")]
    pub cur_price: Decimal,
    #[serde(default)]
    pub percent_pnl: Decimal,
    #[serde(default)]
    pub redeemable: bool,
    #[serde(default, deserialize_with = "deserialize_end_date")]
    pub end_date: Option<chrono::NaiveDate>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub outcome: String,
}

#[derive(Deserialize)]
struct PositionsPage {
    data: Vec<PositionSnapshot>,
    pagination: PositionsPagination,
}

#[derive(Deserialize)]
struct PositionsPagination {
    next_cursor: Option<String>,
}

fn deserialize_end_date<'de, D>(deserializer: D) -> Result<Option<chrono::NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.and_then(|value| chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d").ok()))
}

static DATA_API_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
type CachedPositions = HashMap<Address, (Instant, Vec<PositionSnapshot>)>;
type WalletFetchLocks = HashMap<Address, Arc<tokio::sync::Mutex<()>>>;
static POSITION_CACHE: OnceLock<tokio::sync::RwLock<CachedPositions>> = OnceLock::new();
static POSITION_FETCH_LOCKS: OnceLock<tokio::sync::Mutex<WalletFetchLocks>> = OnceLock::new();

fn data_api_http_client() -> reqwest::Client {
    DATA_API_HTTP_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("Polycopier/1.0")
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .tcp_nodelay(true)
                .pool_idle_timeout(Duration::from_secs(90))
                .build()
                .expect("Data API HTTP client configuration is valid")
        })
        .clone()
}

#[cfg(test)]
async fn fetch_offset_pages<T, F, Fut>(
    user: Address,
    limit: usize,
    max_pages: usize,
    mut fetch_page: F,
) -> anyhow::Result<Vec<T>>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = anyhow::Result<Vec<T>>>,
{
    anyhow::ensure!(limit > 0 && max_pages > 0, "Invalid Data API page limits");

    let mut all_positions = Vec::new();
    for page in 0..max_pages {
        let offset = page * limit;
        let mut batch = fetch_page(offset).await?;
        let count = batch.len();
        all_positions.append(&mut batch);

        if count < limit {
            return Ok(all_positions);
        }
    }

    anyhow::bail!(
        "Data API pagination for {user} reached {max_pages} full pages; refusing to return an incomplete position snapshot"
    )
}

/// Fetches the complete open-position snapshot using the current cursor-based API.
pub async fn fetch_all_positions(
    _client: &DataClient,
    user: Address,
) -> anyhow::Result<Vec<PositionSnapshot>> {
    const LIMIT: usize = 1000;
    const MAX_PAGES: usize = 1000;
    const CACHE_TTL: Duration = Duration::from_secs(10);
    let fetch_lock = {
        let locks = POSITION_FETCH_LOCKS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
        let mut locks = locks.lock().await;
        locks
            .entry(user)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let _singleflight = fetch_lock.lock().await;
    let cache = POSITION_CACHE.get_or_init(|| tokio::sync::RwLock::new(HashMap::new()));
    if let Some((cached_at, positions)) = cache.read().await.get(&user) {
        if cached_at.elapsed() < CACHE_TTL {
            return Ok(positions.clone());
        }
    }

    let reqwest_client = data_api_http_client();
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _page in 0..MAX_PAGES {
        let mut retries = 0;
        let response = loop {
            let mut request = reqwest_client
                .get("https://data-api.polymarket.com/v2/positions")
                .query(&[("user", user.to_string()), ("limit", LIMIT.to_string())]);
            if let Some(value) = cursor.as_deref() {
                request = request.query(&[("cursor", value)]);
            }
            let response = request.send().await?;
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS
                && response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE
            {
                break response;
            }
            anyhow::ensure!(
                retries < 2,
                "Data API rate-limited positions for {user} after retries"
            );
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(2)
                .clamp(1, 10);
            retries += 1;
            tokio::time::sleep(Duration::from_secs(retry_after)).await;
        };
        anyhow::ensure!(
            response.status().is_success(),
            "fetch_all_positions returned HTTP {} for {user}",
            response.status()
        );
        let response: PositionsPage = response.json().await?;
        all.extend(response.data);
        match response
            .pagination
            .next_cursor
            .filter(|value| !value.is_empty())
        {
            Some(next) => cursor = Some(next),
            None => {
                cache
                    .write()
                    .await
                    .insert(user, (Instant::now(), all.clone()));
                return Ok(all);
            }
        }
    }
    anyhow::bail!(
        "Data API pagination for {user} exceeded {MAX_PAGES} cursor pages; refusing incomplete snapshot"
    )
}

#[cfg(test)]
mod pagination_tests {
    use super::{fetch_offset_pages, PositionsPage};
    use alloy::primitives::Address;
    use rust_decimal::Decimal;

    #[tokio::test]
    async fn full_page_budget_returns_error_instead_of_partial_snapshot() {
        let result = fetch_offset_pages(Address::ZERO, 2, 2, |offset| async move {
            Ok(vec![offset, offset + 1])
        })
        .await;

        let error = result.expect_err("a full final page means more results may exist");
        assert!(error.to_string().contains("incomplete position snapshot"));
    }

    #[tokio::test]
    async fn short_page_ends_pagination_and_returns_all_rows() {
        let result = fetch_offset_pages(Address::ZERO, 2, 3, |offset| async move {
            Ok(if offset == 0 { vec![0, 1] } else { vec![2] })
        })
        .await
        .unwrap();

        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn parses_v2_position_envelope_and_maps_api_field_names() {
        let page: PositionsPage = serde_json::from_str(
            r#"{"data":[{"token_id":"123","current_size":2.5,"avg_price":0.4,"current_price":0.6,"percent_pnl":50,"redeemable":false,"end_date":"2026-10-14","title":"Market","outcome":"Yes"}],"pagination":{"next_cursor":"next"}}"#,
        )
        .unwrap();

        assert_eq!(page.data[0].asset, "123");
        assert_eq!(page.data[0].size, Decimal::new(25, 1));
        assert_eq!(page.data[0].cur_price, Decimal::new(6, 1));
        assert_eq!(page.pagination.next_cursor.as_deref(), Some("next"));
    }
}
