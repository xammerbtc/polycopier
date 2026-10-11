//! Exact order ownership and fail-closed handling of ambiguous CLOB submissions.
use crate::models::OrderRequest;
use std::{
    collections::{BTreeMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
};
static LOCK: Mutex<()> = Mutex::new(());
static ACTIVE: AtomicBool = AtomicBool::new(false);
static BLOCKED: AtomicBool = AtomicBool::new(false);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
static SOURCES: std::sync::LazyLock<Mutex<BTreeMap<String, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(BTreeMap::new()));
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Record {
    #[serde(default)]
    pub terminal_unfilled: bool,
    #[serde(default)]
    pub source_wallet: Option<String>,
    pub order: OrderRequest,
    pub status: String,
    pub order_id: Option<String>,
    pub created_at: i64,
}
#[derive(Debug, thiserror::Error)]
#[error("CLOB outcome uncertain; copier paused until execution-journal reconciliation")]
pub struct UncertainExecution;
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}
pub fn blocked() -> bool {
    BLOCKED.load(Ordering::Relaxed)
}
fn load() -> anyhow::Result<BTreeMap<String, Record>> {
    match std::fs::read_to_string("execution-journal.json") {
        Ok(raw) => Ok(serde_json::from_str(&raw)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}
fn save(records: &BTreeMap<String, Record>) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create("execution-journal.json.tmp")?;
    file.write_all(&serde_json::to_vec_pretty(records)?)?;
    file.sync_all()?;
    std::fs::rename("execution-journal.json.tmp", "execution-journal.json")?;
    #[cfg(unix)]
    std::fs::File::open(".")?.sync_all()?;
    Ok(())
}
pub fn initialize() -> anyhow::Result<()> {
    let _lock = LOCK.lock().unwrap();
    let records = load()?;
    anyhow::ensure!(
        !records
            .values()
            .any(|r| matches!(r.status.as_str(), "pending" | "uncertain")),
        "Unreconciled execution journal; live startup blocked"
    );
    save(&records)?;
    ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}
pub fn begin(order: &OrderRequest) -> anyhow::Result<String> {
    begin_record(order, None)
}
pub fn remember_source(token: &str, source: &str) {
    SOURCES
        .lock()
        .unwrap()
        .insert(token.into(), source.to_lowercase());
}
pub fn clear_source(token: &str) {
    SOURCES.lock().unwrap().remove(token);
}
pub fn begin_contextual(order: &OrderRequest) -> anyhow::Result<String> {
    let source = SOURCES
        .lock()
        .unwrap()
        .remove(&order.token_id)
        .ok_or_else(|| anyhow::anyhow!("Submission source context missing"))?;
    begin_record(order, Some(source))
}
fn begin_record(order: &OrderRequest, source_wallet: Option<String>) -> anyhow::Result<String> {
    let _lock = LOCK.lock().unwrap();
    anyhow::ensure!(!blocked(), "Uncertain execution requires reconciliation");
    let mut records = load()?;
    let key = format!(
        "{}-{}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    records.insert(
        key.clone(),
        Record {
            terminal_unfilled: false,
            source_wallet,
            order: order.clone(),
            status: "pending".into(),
            order_id: None,
            created_at: chrono::Utc::now().timestamp(),
        },
    );
    save(&records)?;
    Ok(key)
}
pub fn finish(key: &str, status: &str, id: Option<String>) -> anyhow::Result<()> {
    let _lock = LOCK.lock().unwrap();
    let mut records = load()?;
    let record = records
        .get_mut(key)
        .ok_or_else(|| anyhow::anyhow!("Execution intent missing"))?;
    record.status = status.into();
    record.order_id = id;
    save(&records)
}
pub fn uncertain(key: &str) {
    BLOCKED.store(true, Ordering::Relaxed);
    if let Err(e) = finish(key, "uncertain", None) {
        tracing::error!("Execution journal update failed: {e}");
    }
}
pub fn owned_ids() -> anyhow::Result<HashSet<String>> {
    let _lock = LOCK.lock().unwrap();
    Ok(load()?
        .values()
        .filter(|r| r.status == "accepted")
        .filter_map(|r| r.order_id.clone())
        .collect())
}
pub fn snapshot() -> anyhow::Result<BTreeMap<String, Record>> {
    let _lock = LOCK.lock().unwrap();
    load()
}
pub fn cancel_unfilled(id: &str) -> anyhow::Result<()> {
    let _lock = LOCK.lock().unwrap();
    let mut records = load()?;
    let record = records
        .values_mut()
        .find(|r| r.status == "accepted" && r.order_id.as_deref() == Some(id))
        .ok_or_else(|| anyhow::anyhow!("Owned cancellation missing"))?;
    anyhow::ensure!(
        record.order.side == crate::models::TradeSide::BUY,
        "Cancellation is not an entry"
    );
    record.terminal_unfilled = true;
    save(&records)
}

pub async fn open_orders(
    clob: &crate::clients::AuthedClobClient,
) -> anyhow::Result<Vec<polymarket_client_sdk::clob::types::response::OpenOrderResponse>> {
    let mut orders = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..100 {
        let page = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            clob.orders(
                &polymarket_client_sdk::clob::types::request::OrdersRequest::default(),
                cursor.clone(),
            ),
        )
        .await??;
        orders.extend(page.data);
        if page.next_cursor.is_empty() || page.next_cursor == "LTE=" {
            return Ok(orders);
        }
        anyhow::ensure!(
            seen.insert(page.next_cursor.clone()),
            "Repeated CLOB pagination cursor"
        );
        cursor = Some(page.next_cursor);
    }
    anyhow::bail!("CLOB pagination exceeded safety limit")
}
