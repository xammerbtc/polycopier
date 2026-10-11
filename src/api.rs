use crate::config::BotConfig;
use crate::models::{EvaluatedTrade, Position, TargetPosition};
use crate::state::BotState;
use axum::{
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
};
use axum::{
    extract::{Json, State},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use base64::Engine;
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

async fn authenticate(request: Request, next: Next) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }
    if request.method() != axum::http::Method::GET && request.method() != axum::http::Method::HEAD {
        if let Some(origin) = request
            .headers()
            .get("origin")
            .and_then(|v| v.to_str().ok())
        {
            let host = request
                .headers()
                .get("host")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if origin != format!("http://{host}") && origin != format!("https://{host}") {
                return (StatusCode::FORBIDDEN, "Cross-origin mutation blocked").into_response();
            }
        }
    }
    let token = std::env::var("DASHBOARD_TOKEN").unwrap_or_default();
    if token.len() < 24 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Dashboard token is not configured",
        )
            .into_response();
    }
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("polycopier:{token}"))
    );
    let actual = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let different = actual
        .as_bytes()
        .iter()
        .zip(expected.as_bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if actual.len() != expected.len() || different != 0 {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                "www-authenticate",
                "Basic realm=\"Polycopier\", charset=\"UTF-8\"",
            )],
            "Authentication required",
        )
            .into_response();
    }
    next.run(request).await
}

async fn health(State(api): State<ApiState>) -> Json<serde_json::Value> {
    let state = api.bot_state.read().await;
    Json(serde_json::json!({"status":"ok", "paused":state.started,
        "mode":std::env::var("POLYCOPIER_MODE").unwrap_or_else(|_| "unknown".into()),
        "websocket_connected":crate::rtds::CONNECTED.load(std::sync::atomic::Ordering::Relaxed),
        "active_targets":crate::listener::ACTIVE_TARGETS.read().map(|g| g.clone()).unwrap_or_default(),
        "last_poll_success":crate::listener::LAST_POLL_SUCCESS.load(std::sync::atomic::Ordering::Relaxed),
        "last_websocket_message":crate::rtds::LAST_MESSAGE.load(std::sync::atomic::Ordering::Relaxed)}))
}

#[derive(Deserialize)]
struct Control {
    action: String,
}
async fn control(State(api): State<ApiState>, Json(payload): Json<Control>) -> Response {
    let paused = match payload.action.as_str() {
        "pause" | "stop" => true,
        "start" | "resume" => false,
        _ => return (StatusCode::BAD_REQUEST, "Unknown action").into_response(),
    };
    if !paused && crate::execution_journal::blocked() {
        return (
            StatusCode::CONFLICT,
            "Uncertain execution requires reconciliation before resume",
        )
            .into_response();
    }
    if !paused && crate::execution_journal::active() {
        if !crate::live_redemption::records().is_ok_and(|records| {
            records
                .values()
                .all(|r| r.status == "confirmed" && r.proof.is_some())
        }) {
            return (
                StatusCode::CONFLICT,
                "Redemption receipt reconciliation is pending",
            )
                .into_response();
        }
        let guard = api.bot_state.read().await;
        if !guard.live_fills_ready
            || chrono::Utc::now()
                .timestamp()
                .saturating_sub(guard.last_live_fill_sync)
                > 60
        {
            return (
                StatusCode::CONFLICT,
                "Live settlements require reconciliation before resume",
            )
                .into_response();
        }
    }
    api.bot_state.write().await.started = paused;
    Json(serde_json::json!({"success":true,"paused":paused})).into_response()
}

async fn export_ledger(State(api): State<ApiState>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(&*api.copy_ledger.lock().await).unwrap_or_default())
}

async fn logs() -> Json<serde_json::Value> {
    let text = std::fs::read_to_string("polycopier.log").unwrap_or_default();
    let lines: Vec<_> = text
        .lines()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Json(serde_json::json!({"lines":lines}))
}

async fn redeem(State(api): State<ApiState>) -> Response {
    let Ok(_request_guard) = api.redemption_lock.try_lock() else {
        return (
            StatusCode::CONFLICT,
            "A redemption request is already running",
        )
            .into_response();
    };
    if std::env::var("POLYCOPIER_MODE").as_deref() != Ok("simulation") {
        return redeem_live(&api).await;
    }
    let tokens: Vec<_> = api
        .bot_state
        .read()
        .await
        .positions
        .keys()
        .cloned()
        .collect();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut resolved = Vec::new();
    let mut failed = Vec::new();
    for token in tokens {
        match crate::redemption::resolved_payout(&client, &token).await {
            Ok(Some(payout)) => resolved.push((token, payout)),
            Ok(None) => {}
            Err(_) => failed.push(token),
        }
    }
    let mut ledger = api.copy_ledger.lock().await;
    let mut state = api.bot_state.write().await;
    if !state.pending_orders.is_empty() {
        return (
            StatusCode::CONFLICT,
            "Wait for pending paper orders to finish",
        )
            .into_response();
    }
    let old_positions = state.positions.clone();
    let old_balance = state.total_balance;
    let old_pnl = state.realized_pnl;
    let old_cycle_pnl = state.paper_cycle_pnl.clone();
    let mut settled = Vec::new();
    for (token, payout) in resolved {
        if let Some((proceeds, pnl)) = crate::redemption::settle_paper(&mut state, &token, payout) {
            settled.push((token, proceeds, pnl));
        }
    }
    if let Err(e) = crate::simulation::save(&state) {
        state.positions = old_positions;
        state.total_balance = old_balance;
        state.realized_pnl = old_pnl;
        state.paper_cycle_pnl = old_cycle_pnl;
        tracing::error!("Paper redemption persistence failed: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    for (token, _, pnl) in &settled {
        state.position_prices.remove(token);
        if let Some(tx) = &state.risk_outcome_tx {
            let _ = tx.send(*pnl);
        }
        let sources: Vec<_> = ledger
            .entries
            .iter()
            .filter(|e| !e.closed && e.token_id == *token)
            .map(|e| e.source_wallet.clone())
            .collect();
        for source in sources {
            ledger.record_close(token, &source);
        }
    }
    state.unrealized_pnl = state
        .positions
        .values()
        .filter_map(|p| {
            state
                .position_prices
                .get(&p.token_id)
                .map(|price| (*price - p.average_entry_price) * p.size)
        })
        .sum();
    tracing::info!(
        "Force Redeem (paper): settled {} positions, {} lookup failures",
        settled.len(),
        failed.len()
    );
    Json(serde_json::json!({"mode":"simulation","settled":settled,"lookup_failures":failed,"on_chain_transactions":0})).into_response()
}

async fn redeem_live(api: &ApiState) -> Response {
    if std::env::var("POLYCOPIER_MODE").as_deref() != Ok("live")
        || !crate::live_redemption::configured()
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "Live Redeem needs explicit enablement, signer/wallet and Relayer credentials",
        )
            .into_response();
    }
    let Some(clob) = api.live_client.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Live client unavailable").into_response();
    };
    let funder = std::env::var("FUNDER_ADDRESS").unwrap_or_default();
    let journal = match crate::live_redemption::records() {
        Ok(journal) => journal,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Cannot read redemption journal",
            )
                .into_response()
        }
    };
    let tokens: Vec<_> = {
        let ledger = api.copy_ledger.lock().await;
        let mut state = api.bot_state.write().await;
        if !state.pending_orders.is_empty()
            || !state.in_flight_tokens.is_empty()
            || !state.live_fills_ready
        {
            return (
                StatusCode::CONFLICT,
                "Wait for pending copier orders to finish",
            )
                .into_response();
        }
        state.started = true;
        state
            .copier_positions
            .keys()
            .filter(|token| ledger.find_active_for_token(token).is_some())
            .cloned()
            .collect()
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let mut markets = std::collections::BTreeMap::<String, Vec<String>>::new();
    for token in tokens {
        match crate::redemption::resolved_payout(&client, &token).await {
            Ok(Some(_)) => match crate::live_redemption::market_id(&client, &token).await {
                Ok(id) => {
                    markets.entry(id).or_default().push(token);
                }
                Err(_) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        "Market lookup failed; no redemption submitted",
                    )
                        .into_response()
                }
            },
            Ok(None) => {}
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    "Resolution lookup failed; no redemption submitted",
                )
                    .into_response()
            }
        }
    }
    if markets.keys().any(|market| {
        journal
            .get(market)
            .is_some_and(|record| record.status != "confirmed")
    }) {
        return (
            StatusCode::CONFLICT,
            "Uncertain prior redemption requires chain/Relayer reconciliation",
        )
            .into_response();
    }
    let mut receipts = Vec::new();
    for (market_id, _tokens) in markets {
        if journal.contains_key(&market_id) {
            continue;
        }
        let prepared = async {
            let (fills, orders) = crate::live_fills::collect(clob, &funder).await?;
            let book = crate::live_fills::accounted(&fills, &orders)?;
            let (assets, all_tokens) =
                crate::redemption_settlement::prepare(&client, &market_id, &funder, &book).await?;
            let open = crate::execution_journal::open_orders(clob).await?;
            anyhow::ensure!(
                !open
                    .iter()
                    .any(|o| all_tokens.contains(&o.asset_id.to_string())),
                "Market has outstanding orders"
            );
            for record in orders
                .values()
                .filter(|r| r.status == "accepted" && all_tokens.contains(&r.order.token_id))
            {
                let id = record
                    .order_id
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Owned order ID missing"))?;
                let order = tokio::time::timeout(std::time::Duration::from_secs(5), clob.order(id))
                    .await??;
                let settled: Decimal = fills
                    .iter()
                    .filter(|f| f.order_id.eq_ignore_ascii_case(id))
                    .map(|f| f.shares)
                    .sum();
                anyhow::ensure!(
                    order.id.eq_ignore_ascii_case(id) && order.size_matched == settled,
                    "Market has unfinalized copier fills"
                );
            }
            Ok::<_, anyhow::Error>(assets)
        }
        .await;
        let assets=match prepared {Ok(assets)=>assets,Err(_)=>return (StatusCode::CONFLICT,"Redemption preflight failed: market must be resolved, trading closed, and all shares/fills owned and finalized").into_response()};
        match crate::live_redemption::execute(&market_id, assets).await {
            Ok(receipt) => {
                api.bot_state.write().await.live_fills_ready = false;
                receipts.push(receipt);
                if crate::live_fills::reconcile(clob, &funder, &api.bot_state, &api.copy_ledger)
                    .await
                    .is_err()
                {
                    return (StatusCode::ACCEPTED,Json(serde_json::json!({"mode":"live","settlement_pending":true,"on_chain_transactions":receipts.len(),"receipts":receipts}))).into_response();
                }
            }
            Err(e) => {
                tracing::error!("Live redemption of market {market_id} needs reconciliation: {e}");
                return (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error":"Redemption incomplete; inspect protected journal before retrying","confirmed_receipts":receipts}))).into_response();
            }
        }
    }
    Json(serde_json::json!({"mode":"live","on_chain_transactions":receipts.len(),"receipts":receipts})).into_response()
}

async fn redemption_status() -> Response {
    match crate::live_redemption::records() {
        Ok(records) => Json(serde_json::json!({"live_configured":crate::live_redemption::configured(),"records":records})).into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "Cannot read redemption journal").into_response(),
    }
}

#[derive(Clone)]
pub struct ApiState {
    pub live_client: Option<crate::clients::AuthedClobClient>,
    pub redemption_lock: Arc<tokio::sync::Mutex<()>>,
    pub bot_state: Arc<RwLock<BotState>>,
    pub copy_ledger: Arc<tokio::sync::Mutex<crate::copy_ledger::CopyLedger>>,
}

// ── SETUP ROUTER (when .env is missing) ───────────────────────────────────

pub fn create_setup_router() -> Router {
    Router::new()
        .route(
            "/api/state",
            get(|| async { axum::Json(serde_json::json!({ "status": "setup_required" })) }),
        )
        .route("/api/setup", post(handle_setup))
        .fallback_service(
            tower_http::services::ServeDir::new("web/dist").append_index_html_on_directories(true),
        )
        .layer(middleware::from_fn(authenticate))
}

#[derive(serde::Deserialize)]
pub struct SetupPayload {
    pub private_key: String,
    pub funder_address: String,
}

async fn handle_setup(Json(payload): Json<SetupPayload>) -> axum::response::Response {
    use crate::config::{is_valid_private_key_format, BotConfig, TargetsConfig};

    if !is_valid_private_key_format(&payload.private_key) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Invalid Private Key: MUST be exactly 64 hex characters (32 bytes) long.",
        )
            .into_response();
    }

    // 1. Write the genuine secrets to `.env` (preserving any existing unrelated variables)
    let _ = crate::config::write_secrets_env(&payload.private_key, &payload.funder_address);

    // 2. Initialize default config.toml (Target Wallets can be configured later in UI)
    if !std::path::Path::new("config.toml").exists() {
        let default_cfg = BotConfig {
            targets: TargetsConfig { wallets: vec![] },
            ..Default::default()
        };
        let _ = crate::config::write_toml(&default_cfg);
    }

    // Force a semantic wait to ensure file flush
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Trigger seamless hot reboot after a short delay so the HTTP response returns cleanly
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let exe = std::env::current_exe().unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(&exe).arg("--ui-reboot").exec();
            tracing::error!("Seamless setup reboot failed: {}", err);
        }
        #[cfg(not(unix))]
        {
            let _ = std::process::Command::new(&exe).arg("--ui-reboot").spawn();
            std::process::exit(0);
        }
    });

    axum::Json(serde_json::json!({ "success": true })).into_response()
}

// ────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct StateResponse {
    pub position_prices: HashMap<String, Decimal>,
    pub prices_updated_at: u64,
    pub paused: bool,
    pub mode: String,
    pub daily_volume_usd: Decimal,
    pub positions: HashMap<String, Position>,
    pub live_feed: Vec<EvaluatedTrade>,
    pub total_balance: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub target_positions: Vec<TargetPosition>,
    pub copies_executed: u32,
    pub trades_skipped: u32,
    pub copied_count: usize,
    pub next_scan_secs: u64,
    pub pending_orders: std::collections::HashMap<String, crate::models::QueuedOrder>,
    pub active_orders: Vec<crate::models::ActiveApiOrder>,
    pub position_sources: HashMap<String, String>,
}

async fn get_state(State(api_state): State<ApiState>) -> Json<StateResponse> {
    let ledger = api_state.copy_ledger.lock().await;
    let guard = api_state.bot_state.read().await;

    let mut position_sources = HashMap::new();
    let positions = if crate::execution_journal::active() {
        &guard.copier_positions
    } else {
        &guard.positions
    };
    // Map live holdings
    for token_id in positions.keys() {
        if let Some(entry) = ledger.find_active_for_token(token_id) {
            position_sources.insert(token_id.clone(), entry.source_wallet.clone());
        } else if let Some(entry) = ledger
            .entries
            .iter()
            .rev()
            .find(|e| e.token_id == *token_id)
        {
            // Fallback for positions currently in the process of closing
            // (Limit SELL pending but still held in balance)
            position_sources.insert(token_id.clone(), entry.source_wallet.clone());
        }
    }
    // Map active API limit orders (pending opens / pending closes)
    for o in &guard.active_orders {
        if let Some(entry) = ledger.find_active_for_token(&o.token_id) {
            position_sources.insert(o.token_id.clone(), entry.source_wallet.clone());
        } else if let Some(entry) = ledger
            .entries
            .iter()
            .rev()
            .find(|e| e.token_id == o.token_id)
        {
            position_sources.insert(o.token_id.clone(), entry.source_wallet.clone());
        }
    }

    // Convert current state to serializable DTO, stripping Instants
    let response = StateResponse {
        position_prices: guard.position_prices.clone(),
        prices_updated_at: guard.prices_updated_at,
        paused: guard.started,
        mode: std::env::var("POLYCOPIER_MODE").unwrap_or_else(|_| "unknown".into()),
        daily_volume_usd: std::env::var("RISK_STATE_PATH")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .filter(|v| {
                v["date"].as_str() == Some(chrono::Utc::now().date_naive().to_string().as_str())
            })
            .and_then(|v| serde_json::from_value(v["volume"].clone()).ok())
            .unwrap_or(Decimal::ZERO),
        positions: positions.clone(),
        live_feed: guard.live_feed.iter().cloned().collect(),
        total_balance: guard.total_balance,
        unrealized_pnl: guard.unrealized_pnl,
        realized_pnl: guard.realized_pnl,
        target_positions: guard.target_positions.clone(),
        copies_executed: guard.copies_executed,
        trades_skipped: guard.trades_skipped,
        copied_count: guard.copied_count,
        next_scan_secs: guard.next_scan_secs,
        pending_orders: guard.pending_orders.clone(),
        active_orders: guard.active_orders.clone(),
        position_sources,
    };

    Json(response)
}

use serde::Deserialize;

#[derive(Serialize, Deserialize)]
pub struct EnvData {
    pub private_key: String,
    pub funder_address: String,
}

async fn get_config() -> Json<serde_json::Value> {
    let raw = std::fs::read_to_string("config.toml").unwrap_or_default();
    let toml_val: toml::Value = raw
        .parse()
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));
    let json_val = serde_json::to_value(toml_val).unwrap();
    Json(json_val)
}

async fn post_config(Json(payload): Json<BotConfig>) -> Response {
    if let Err(e) = crate::config::validate_config(&payload) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":e.to_string()})),
        )
            .into_response();
    }
    if let Err(e) = crate::config::write_toml(&payload) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
    Json(serde_json::json!({ "success": true, "hot_reload": true })).into_response()
}

async fn get_env() -> Json<EnvData> {
    let private_key = String::new();
    let funder_address = std::env::var("FUNDER_ADDRESS").unwrap_or_default();
    Json(EnvData {
        private_key,
        funder_address,
    })
}

async fn restart() -> Json<serde_json::Value> {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Execute seamless process replacement (hot reboot in the same terminal)
        let exe = std::env::current_exe().unwrap_or_else(|_| "cargo".into());
        let args: Vec<String> = std::env::args().collect();

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            tracing::warn!("Executing seamless API reboot...");
            let err = std::process::Command::new(&exe).args(&args[1..]).exec();
            tracing::error!("Seamless API reboot failed: {}", err);
            std::process::exit(1);
        }
        #[cfg(not(unix))]
        {
            let _ = std::process::Command::new(&exe).args(&args[1..]).spawn();
            std::process::exit(0);
        }
    });
    Json(serde_json::json!({ "success": true }))
}

pub fn create_router(
    bot_state: Arc<RwLock<BotState>>,
    copy_ledger: Arc<tokio::sync::Mutex<crate::copy_ledger::CopyLedger>>,
) -> Router {
    create_router_with_client(bot_state, copy_ledger, None)
}
pub fn create_router_with_client(
    bot_state: Arc<RwLock<BotState>>,
    copy_ledger: Arc<tokio::sync::Mutex<crate::copy_ledger::CopyLedger>>,
    live_client: Option<crate::clients::AuthedClobClient>,
) -> Router {
    use tower_http::services::{ServeDir, ServeFile};

    let state = ApiState {
        live_client,
        redemption_lock: Arc::new(tokio::sync::Mutex::new(())),
        bot_state,
        copy_ledger,
    };

    let root_path = std::env::current_dir().unwrap().join("web/dist");
    let serve_dir =
        ServeDir::new(&root_path).fallback(ServeFile::new(root_path.join("index.html")));

    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/env", get(get_env))
        .route("/api/control", post(control))
        .route("/api/ledger", get(export_ledger))
        .route("/api/logs", get(logs))
        .route(
            "/api/metrics",
            get(|| async { Json(crate::latency::snapshot()) }),
        )
        .route("/api/redeem", post(redeem))
        .route("/api/redemption", get(redemption_status))
        .route(
            "/api/executions",
            get(|| async {
                match crate::execution_journal::snapshot() {
                    Ok(records) => Json(serde_json::json!(records)).into_response(),
                    Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }),
        )
        .route("/health", get(health))
        .route("/api/action/restart", post(restart))
        .with_state(state)
        .fallback_service(serve_dir)
        .layer(middleware::from_fn(authenticate))
}
