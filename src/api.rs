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
    extract::{Json, Query, State},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use base64::Engine;
use futures_util::StreamExt;
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
    if !paused && api.bot_state.read().await.paper_integrity_fault {
        return (
            StatusCode::CONFLICT,
            "Paper persistence failed; reconcile the account and restart before resuming",
        )
            .into_response();
    }
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
    api.control_revision
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Json(serde_json::json!({"success":true,"paused":paused})).into_response()
}

#[derive(Deserialize)]
struct PaperSellRequest {
    scope: String,
    token_id: Option<String>,
    source_wallet: Option<String>,
    shares: Option<Decimal>,
    order_type: String,
    limit_price: Option<Decimal>,
}

async fn paper_sell(
    State(api): State<ApiState>,
    Json(request): Json<PaperSellRequest>,
) -> Response {
    if api.bot_state.read().await.paper_integrity_fault {
        return (StatusCode::CONFLICT, "Paper account needs reconciliation").into_response();
    }
    if std::env::var("POLYCOPIER_MODE").as_deref() != Ok("simulation") {
        return (
            StatusCode::CONFLICT,
            "Manual dashboard sells currently require simulation mode",
        )
            .into_response();
    }
    if crate::execution_journal::active() {
        return (
            StatusCode::CONFLICT,
            "Paper sale endpoint cannot operate on live inventory",
        )
            .into_response();
    }
    let min_price = match request.order_type.as_str() {
        "market" if request.limit_price.is_none() => None,
        "limit" => match request.limit_price {
            Some(price) if (Decimal::new(1, 2)..=Decimal::new(99, 2)).contains(&price) => {
                Some(price)
            }
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    "Limit price must be between $0.01 and $0.99",
                )
                    .into_response()
            }
        },
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "Order type must be market or limit",
            )
                .into_response()
        }
    };
    if request.scope == "position" {
        let Some(token) = request.token_id.as_deref() else {
            return (StatusCode::BAD_REQUEST, "Position sale requires token_id").into_response();
        };
        match execute_paper_sale(
            &api,
            token,
            request.source_wallet.as_deref(),
            request.shares,
            min_price,
        )
        .await
        {
            Ok(result) => {
                Json(serde_json::json!({"success":true,"results":[result]})).into_response()
            }
            Err(error) => (StatusCode::CONFLICT, error).into_response(),
        }
    } else if request.scope == "all" || request.scope == "signals" {
        let tokens: Vec<String> = {
            let ledger = api.copy_ledger.lock().await;
            let state = api.bot_state.read().await;
            let mut tokens: Vec<String> = if request.scope == "signals" {
                ledger
                    .entries
                    .iter()
                    .filter(|entry| !entry.closed)
                    .map(|entry| entry.token_id.clone())
                    .collect()
            } else {
                state.positions.keys().cloned().collect()
            };
            tokens.sort();
            tokens.dedup();
            tokens
                .into_iter()
                .filter(|token| state.positions.contains_key(token))
                .collect()
        };
        if tokens.is_empty() {
            return (
                StatusCode::CONFLICT,
                "No copied positions are available to sell",
            )
                .into_response();
        }
        let mut results = Vec::with_capacity(tokens.len());
        for token in tokens {
            match execute_paper_sale(&api, &token, None, None, min_price).await {
                Ok(result) => results.push(result),
                Err(error) => results
                    .push(serde_json::json!({"token_id":token,"success":false,"reason":error})),
            }
        }
        Json(serde_json::json!({"success":results.iter().any(|item| item["success"]==true),"results":results})).into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            "Scope must be position, all, or signals",
        )
            .into_response()
    }
}

async fn execute_paper_sale(
    api: &ApiState,
    token: &str,
    source: Option<&str>,
    requested: Option<Decimal>,
    min_price: Option<Decimal>,
) -> Result<serde_json::Value, String> {
    let (available, position_size) = {
        let ledger = api.copy_ledger.lock().await;
        let state = api.bot_state.read().await;
        let position_size = state
            .positions
            .get(token)
            .ok_or_else(|| "Position is not held".to_string())?
            .size;
        let available = if let Some(source) = source {
            let source = source.to_ascii_lowercase();
            let size = ledger.active_size_for_source(token, &source);
            if size <= Decimal::ZERO {
                return Err("No open shares are attributed to this leader".into());
            }
            vec![(source, size)]
        } else {
            let mut sizes = std::collections::BTreeMap::<String, Decimal>::new();
            for entry in ledger
                .entries
                .iter()
                .filter(|entry| !entry.closed && entry.token_id == token)
            {
                let left = entry.filled_size.unwrap_or(entry.size);
                if left > Decimal::ZERO {
                    *sizes
                        .entry(entry.source_wallet.to_ascii_lowercase())
                        .or_default() += left;
                }
            }
            sizes
                .into_iter()
                .filter(|(_, size)| *size > Decimal::ZERO)
                .collect()
        };
        (available, position_size)
    };
    let total_available: Decimal = available.iter().map(|(_, shares)| *shares).sum();
    let shares =
        requested.unwrap_or_else(|| source.map(|_| total_available).unwrap_or(position_size));
    if shares <= Decimal::ZERO || shares > total_available || shares > position_size {
        return Err(format!(
            "Requested shares exceed the attributed holding (available {:.4})",
            total_available.min(position_size)
        ));
    }
    let mut fill = crate::simulation::paper_sell_ladder(token, shares, min_price)
        .await
        .map_err(|e| format!("Could not read bid book: {e}"))?
        .ok_or_else(|| "No bid liquidity at the selected price; no sale was made".to_string())?;
    let market = crate::markets::fetch(
        &reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?,
        token,
    )
    .await
    .map_err(|e| format!("Could not verify market fee schedule: {e}"))?;
    let fee_rate = crate::fees::rate(&market, token)
        .map_err(|e| format!("Could not verify market fee schedule: {e}"))?;
    let fee = crate::fees::taker_fee(fill.shares, fill.average_price, fee_rate);
    let mut remainder = fill.shares;
    let mut sales = Vec::new();
    for (wallet, owned) in &available {
        if remainder <= Decimal::ZERO {
            break;
        }
        let quantity = remainder.min(*owned);
        if quantity > Decimal::ZERO {
            sales.push((wallet.clone(), quantity));
            remainder -= quantity;
        }
    }
    if remainder > Decimal::new(1, 8) {
        return Err("Source ledger does not cover filled inventory; sale cancelled".into());
    }
    let mut ledger = api.copy_ledger.lock().await;
    let costs: Vec<_> = sales
        .iter()
        .map(|(wallet, quantity)| {
            ledger
                .sale_cost_basis(token, wallet, *quantity)
                .map(|basis| (wallet.clone(), *quantity, basis))
        })
        .collect();
    if costs.iter().any(Option::is_none) {
        return Err("Source ledger changed during sale; no sale was booked".into());
    }
    let total_basis: Decimal = costs.iter().flatten().map(|(_, _, basis)| *basis).sum();
    let mut state = api.bot_state.write().await;
    if state.paper_integrity_fault {
        return Err("Paper account needs reconciliation".into());
    }
    let outcome = crate::simulation::sell_with_cost_basis(
        &mut state,
        token,
        fill.shares,
        fill.average_price,
        fee,
        total_basis,
    )
    .map_err(|e| format!("Position changed before sale could be recorded: {e}"))?;
    fill.commit_reservation();
    for (wallet, quantity, _) in costs.into_iter().flatten() {
        if let Err(e) = ledger.record_source_sale(
            token,
            &wallet,
            quantity,
            fill.average_price,
            fee * quantity / fill.shares,
        ) {
            state.started = true;
            state.paper_integrity_fault = true;
            return Err(format!(
                "Paper inventory updated but source ledger needs review: {e}"
            ));
        }
    }
    if let Err(e) = crate::simulation::commit(&mut state) {
        state.started = true;
        state.paper_integrity_fault = true;
        return Err(format!("Sale completed but paper state save failed: {e}"));
    }
    if let (Some(pnl), Some(tx)) = (outcome, state.risk_outcome_tx.as_ref()) {
        let _ = tx.send(pnl);
    }
    Ok(
        serde_json::json!({"success":true,"token_id":token,"shares":fill.shares,"average_price":fill.average_price,"fee":fee,"proceeds":fill.shares*fill.average_price-fee,"sources":sales.iter().map(|(wallet, quantity)| serde_json::json!({"wallet":wallet,"shares":quantity})).collect::<Vec<_>>()}),
    )
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
    if std::env::var("POLYCOPIER_MODE").as_deref() == Ok("simulation")
        && api.bot_state.read().await.paper_integrity_fault
    {
        return (StatusCode::CONFLICT, "Paper account needs reconciliation").into_response();
    }
    let Ok(_request_guard) = api.redemption_lock.try_lock() else {
        return (
            StatusCode::CONFLICT,
            "A redemption request is already running",
        )
            .into_response();
    };
    if std::env::var("POLYCOPIER_MODE").as_deref() != Ok("simulation") {
        return redeem_live(&api, false).await;
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
    let mut lookups: Vec<_> = futures_util::stream::iter(tokens)
        .map(|token| {
            let client = client.clone();
            async move {
                let result = crate::redemption::resolved_payout(&client, &token).await;
                (token, result)
            }
        })
        .buffer_unordered(8)
        .collect()
        .await;
    lookups.sort_by(|a, b| a.0.cmp(&b.0));
    let mut resolved = Vec::new();
    let mut failed = Vec::new();
    for (token, result) in lookups {
        match result {
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
    let old_outcomes = state.paper_outcomes.clone();
    let old_ledger = ledger.clone();
    let mut settled = Vec::new();
    for (token, payout) in resolved {
        if let Some((proceeds, pnl)) = crate::redemption::settle_paper(&mut state, &token, payout) {
            if let Err(e) = ledger.settle_token(&token, payout) {
                *ledger = old_ledger;
                state.positions = old_positions;
                state.total_balance = old_balance;
                state.realized_pnl = old_pnl;
                state.paper_cycle_pnl = old_cycle_pnl;
                state.paper_outcomes = old_outcomes;
                tracing::error!("Paper ledger settlement attribution failed: {e}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            settled.push((token, proceeds, pnl, payout));
        }
    }
    if let Err(e) = ledger.try_save() {
        *ledger = old_ledger.clone();
        state.positions = old_positions;
        state.total_balance = old_balance;
        state.realized_pnl = old_pnl;
        state.paper_cycle_pnl = old_cycle_pnl;
        state.paper_outcomes = old_outcomes;
        tracing::error!("Paper settlement ledger persistence failed: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(e) = crate::simulation::commit(&mut state) {
        *ledger = old_ledger;
        ledger.save();
        state.positions = old_positions;
        state.total_balance = old_balance;
        state.realized_pnl = old_pnl;
        state.paper_cycle_pnl = old_cycle_pnl;
        state.paper_outcomes = old_outcomes;
        tracing::error!("Paper redemption persistence failed: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    for (token, _, pnl, _) in &settled {
        state.position_prices.remove(token);
        if let Some(tx) = &state.risk_outcome_tx {
            let _ = tx.send(*pnl);
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

async fn redeem_live(api: &ApiState, resume_after_confirmation: bool) -> Response {
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
    let control_revision = api
        .control_revision
        .load(std::sync::atomic::Ordering::SeqCst);
    let was_paused = {
        let state = api.bot_state.read().await;
        state.started
    };
    let tokens: Vec<_> = {
        let ledger = api.copy_ledger.lock().await;
        let state = api.bot_state.read().await;
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
    let resolved = match crate::redemption::resolved_payouts(&client, &tokens).await {
        Ok(resolved) => resolved,
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                "Resolution lookup failed; no redemption submitted",
            )
                .into_response()
        }
    };
    let mut markets = std::collections::BTreeMap::<String, Vec<String>>::new();
    let mut resolved_tokens: Vec<_> = resolved.into_keys().collect();
    resolved_tokens.sort();
    for token in resolved_tokens {
        match crate::live_redemption::market_id(&client, &token).await {
            Ok(id) => markets.entry(id).or_default().push(token),
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    "Market lookup failed; no redemption submitted",
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
    if markets.is_empty() {
        return Json(serde_json::json!({
            "mode": "live",
            "on_chain_transactions": 0,
            "receipts": []
        }))
        .into_response();
    }
    let mut receipts = Vec::new();
    let mut paused_for_redemption = false;
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
        if !paused_for_redemption {
            api.bot_state.write().await.started = true;
            paused_for_redemption = true;
        }
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
    if resume_after_confirmation
        && paused_for_redemption
        && !was_paused
        && api
            .control_revision
            .load(std::sync::atomic::Ordering::SeqCst)
            == control_revision
    {
        api.bot_state.write().await.started = false;
    }
    Json(serde_json::json!({"mode":"live","on_chain_transactions":receipts.len(),"receipts":receipts})).into_response()
}

/// Start automatic live redemption only after the operator has enabled it and
/// configured every required signer/Relayer credential. The same mutex is shared
/// with the dashboard action, and uncertain submissions remain paused for review.
fn start_live_redemption_worker(api: ApiState) {
    if std::env::var("POLYCOPIER_MODE").as_deref() != Ok("live")
        || !crate::live_redemption::configured()
    {
        return;
    }
    tokio::spawn(async move {
        tracing::info!(
            "Automatic live redemption enabled; checking resolved positions every 60 seconds"
        );
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let lock = api.redemption_lock.clone();
            let Ok(_guard) = lock.try_lock_owned() else {
                tracing::debug!(
                    "Automatic live redemption skipped because another redemption is running"
                );
                continue;
            };
            let response = redeem_live(&api, true).await;
            let status = response.status();
            if status.is_success() {
                tracing::info!("Automatic live redemption check completed with HTTP {status}");
            } else {
                tracing::warn!("Automatic live redemption check needs attention (HTTP {status})");
            }
        }
    });
}

async fn redemption_status() -> Response {
    match crate::live_redemption::records() {
        Ok(records) => Json(serde_json::json!({"live_configured":crate::live_redemption::configured(),"automatic_enabled":crate::live_redemption::configured(),"records":records})).into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "Cannot read redemption journal").into_response(),
    }
}

#[derive(Clone)]
pub struct ApiState {
    pub live_client: Option<crate::clients::AuthedClobClient>,
    pub redemption_lock: Arc<tokio::sync::Mutex<()>>,
    control_revision: Arc<std::sync::atomic::AtomicU64>,
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
            targets: TargetsConfig {
                wallets: vec![],
                ratings: vec![],
            },
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
pub struct TargetPerformance {
    pub address: String,
    pub name: String,
    pub copy_pct: Decimal,
    pub rating: u8,
    pub copied_entries: usize,
    pub closed_entries: usize,
    pub recent_signals: usize,
    pub recent_skips: usize,
    pub open_positions: usize,
    pub open_cost: Decimal,
    pub open_value: Decimal,
    pub open_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub total_pnl: Decimal,
    pub roi_pct: Option<Decimal>,
}

fn remaining_lot_size(entry: &crate::copy_ledger::CopyEntry) -> Decimal {
    // record_source_sale and settle_token already reduce filled_size by the
    // exited shares. realized_shares tracks history, not additional inventory.
    entry.filled_size.unwrap_or(entry.size).max(Decimal::ZERO)
}

type SourceAllocations = HashMap<String, HashMap<String, (Decimal, Decimal, Decimal)>>;

fn record_position_source_allocation(
    allocations: &mut SourceAllocations,
    entry: &crate::copy_ledger::CopyEntry,
    positions: &HashMap<String, Position>,
    prices: &HashMap<String, Decimal>,
) {
    if entry.closed || !positions.contains_key(&entry.token_id) {
        return;
    }
    let shares = remaining_lot_size(entry);
    if shares <= Decimal::ZERO {
        return;
    }
    let cost = shares * entry.entry_price;
    let allocation = allocations
        .entry(entry.token_id.clone())
        .or_default()
        .entry(entry.source_wallet.to_ascii_lowercase())
        .or_default();
    allocation.0 += shares;
    allocation.1 += cost;
    if let Some(price) = prices.get(&entry.token_id) {
        allocation.2 += shares * *price - cost;
    }
}

#[derive(Serialize)]
pub struct PositionSourceAllocation {
    pub address: String,
    pub name: String,
    pub shares: Decimal,
    pub cost_basis: Decimal,
    pub open_pnl: Option<Decimal>,
}

#[derive(Serialize)]
pub struct StateResponse {
    pub position_details: HashMap<String, crate::markets::Details>,
    pub position_prices: HashMap<String, Decimal>,
    pub prices_updated_at: u64,
    pub price_updated_at: HashMap<String, u64>,
    pub paused: bool,
    pub paper_integrity_fault: bool,
    pub paper_settlement_last_attempt: Option<i64>,
    pub paper_settlement_last_lookup: Option<i64>,
    pub paper_settlement_last_error: Option<String>,
    pub paper_settlement_market_failures: usize,
    pub mode: String,
    pub live_ready: bool,
    pub live_blockers: Vec<String>,
    pub daily_volume_usd: Decimal,
    pub positions: HashMap<String, Position>,
    pub live_feed: Vec<EvaluatedTrade>,
    pub total_balance: Decimal,
    pub starting_balance: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub open_cost_basis: Decimal,
    pub open_market_value: Option<Decimal>,
    pub equity: Option<Decimal>,
    pub total_pnl: Option<Decimal>,
    pub priced_positions: usize,
    pub target_positions: Vec<TargetPosition>,
    pub target_positions_total: usize,
    pub copies_executed: u32,
    pub filled_buy_orders: Option<usize>,
    pub trades_skipped: u32,
    pub skip_attempts: std::collections::BTreeMap<String, u64>,
    pub copied_count: usize,
    pub next_scan_secs: u64,
    pub pending_orders: std::collections::HashMap<String, crate::models::QueuedOrder>,
    pub active_orders: Vec<crate::models::ActiveApiOrder>,
    pub position_sources: HashMap<String, String>,
    pub position_source_allocations: HashMap<String, Vec<PositionSourceAllocation>>,
    pub target_performance: Vec<TargetPerformance>,
    pub performance_since: String,
}

async fn get_state(State(api_state): State<ApiState>) -> Json<StateResponse> {
    let ledger = api_state.copy_ledger.lock().await;
    let guard = api_state.bot_state.read().await;
    let is_sim = std::env::var("POLYCOPIER_MODE").as_deref() == Ok("simulation");
    let filled_buy_orders = if is_sim {
        Some(
            ledger
                .entries
                .iter()
                .filter(|entry| {
                    entry.copied_at >= ledger.performance_since
                        && entry.realized_shares + entry.filled_size.unwrap_or(entry.size)
                            > Decimal::ZERO
                })
                .count(),
        )
    } else {
        crate::live_fills::stored().ok().map(|fills| {
            fills
                .iter()
                .filter(|fill| {
                    fill.side == crate::models::TradeSide::BUY
                        && fill.block_timestamp >= ledger.performance_since.timestamp()
                })
                .map(|fill| fill.order_id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len()
        })
    };

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

    let mut target_performance = std::fs::read_to_string("config.toml")
        .ok()
        .and_then(|raw| toml::from_str::<BotConfig>(&raw).ok())
        .map(|config| {
            config
                .targets
                .wallets
                .iter()
                .filter_map(|entry| crate::config::parse_target_wallet(entry))
                .filter(|(_, scalar, _)| *scalar > Decimal::ZERO)
                .map(|(address, scalar, name)| TargetPerformance {
                    address,
                    name,
                    copy_pct: scalar * Decimal::from(100),
                    rating: 0,
                    copied_entries: 0,
                    closed_entries: 0,
                    recent_signals: 0,
                    recent_skips: 0,
                    open_positions: 0,
                    open_cost: Decimal::ZERO,
                    open_value: Decimal::ZERO,
                    open_pnl: Decimal::ZERO,
                    realized_pnl: Decimal::ZERO,
                    total_pnl: Decimal::ZERO,
                    roi_pct: None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let performance_index: HashMap<_, _> = target_performance
        .iter()
        .enumerate()
        .map(|(i, target)| (target.address.clone(), i))
        .collect();
    let mut target_open_tokens =
        vec![std::collections::HashSet::<String>::new(); target_performance.len()];
    let mut source_allocations = SourceAllocations::new();
    let mut invested_basis = vec![Decimal::ZERO; target_performance.len()];
    for entry in &ledger.entries {
        // Holdings keep their source attribution after a target is removed or
        // the performance baseline moves forward.
        record_position_source_allocation(
            &mut source_allocations,
            entry,
            positions,
            &guard.position_prices,
        );
        let Some(index) = performance_index.get(&entry.source_wallet.to_ascii_lowercase()) else {
            continue;
        };
        if entry.copied_at < ledger.performance_since {
            continue;
        }
        let stats = &mut target_performance[*index];
        stats.copied_entries += 1;
        stats.realized_pnl += entry.realized_pnl;
        invested_basis[*index] +=
            entry.entry_price * (entry.realized_shares + entry.filled_size.unwrap_or(entry.size));
        if entry.closed {
            stats.closed_entries += 1;
            continue;
        }
        // Open share attribution must exclude shares already closed by a
        // source-specific sell; otherwise the dashboard overstates our stake.
        let lot_size = remaining_lot_size(entry);
        if positions.contains_key(&entry.token_id) && lot_size > Decimal::ZERO {
            if target_open_tokens[*index].insert(entry.token_id.clone()) {
                stats.open_positions += 1;
            }
            let cost = lot_size * entry.entry_price;
            stats.open_cost += cost;
            if let Some(price) = guard.position_prices.get(&entry.token_id) {
                let value = lot_size * *price;
                stats.open_value += value;
                stats.open_pnl += value - cost;
            }
        }
    }
    for (index, stats) in target_performance.iter_mut().enumerate() {
        stats.total_pnl = stats.realized_pnl + stats.open_pnl;
        if invested_basis[index] > Decimal::ZERO {
            let roi = stats.total_pnl * Decimal::from(100) / invested_basis[index];
            stats.roi_pct = Some(roi);
            stats.rating = if stats.copied_entries == 0 {
                0
            } else if roi < Decimal::new(-20, 0) {
                1
            } else if roi < Decimal::ZERO {
                2
            } else if roi < Decimal::from(10) {
                3
            } else if roi < Decimal::from(25) {
                4
            } else {
                5
            };
        }
    }
    for trade in &guard.live_feed {
        let Some(index) =
            performance_index.get(&trade.original_event.taker_address.to_ascii_lowercase())
        else {
            continue;
        };
        let stats = &mut target_performance[*index];
        stats.recent_signals += 1;
        if !trade.validated {
            stats.recent_skips += 1;
        }
    }
    target_performance.sort_by(|left, right| {
        right
            .rating
            .cmp(&left.rating)
            .then_with(|| right.roi_pct.cmp(&left.roi_pct))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });

    let name_by_address: HashMap<_, _> = target_performance
        .iter()
        .map(|target| (target.address.to_ascii_lowercase(), target.name.clone()))
        .collect();
    let position_source_allocations: HashMap<_, _> = source_allocations
        .into_iter()
        .map(|(token, sources)| {
            let mut allocations: Vec<_> = sources
                .into_iter()
                .map(
                    |(address, (shares, cost_basis, pnl))| PositionSourceAllocation {
                        name: name_by_address
                            .get(&address)
                            .cloned()
                            .unwrap_or_else(|| address.clone()),
                        address,
                        shares,
                        cost_basis,
                        open_pnl: guard.position_prices.contains_key(&token).then_some(pnl),
                    },
                )
                .collect();
            allocations.sort_by_key(|allocation| allocation.name.to_lowercase());
            (token, allocations)
        })
        .collect();

    let open_cost_basis: Decimal = positions
        .values()
        .map(|position| position.size * position.average_entry_price)
        .sum();
    let priced_positions = positions
        .keys()
        .filter(|token| guard.position_prices.contains_key(*token))
        .count();
    let open_market_value = (priced_positions == positions.len()).then(|| {
        positions
            .values()
            .map(|position| {
                position.size
                    * guard
                        .position_prices
                        .get(&position.token_id)
                        .copied()
                        .unwrap_or_default()
            })
            .sum::<Decimal>()
    });
    let starting_balance = if std::env::var("POLYCOPIER_MODE").as_deref() == Ok("simulation") {
        std::env::var("PAPER_STARTING_BALANCE")
            .ok()
            .and_then(|value| value.parse().ok())
            .or_else(|| {
                std::env::args()
                    .collect::<Vec<_>>()
                    .windows(2)
                    .find(|args| args[0] == "--sim-balance")
                    .and_then(|args| args[1].parse().ok())
            })
            .unwrap_or(Decimal::from(10000))
    } else {
        Decimal::ZERO
    };
    let equity = open_market_value.map(|value| guard.total_balance + value);
    let total_pnl = equity.map(|value| value - starting_balance);

    // Convert current state to serializable DTO, stripping Instants
    let response = StateResponse {
        position_details: guard.position_details.clone(),
        position_prices: guard.position_prices.clone(),
        prices_updated_at: guard.prices_updated_at,
        price_updated_at: guard.price_updated_at.clone(),
        paused: guard.started,
        paper_integrity_fault: guard.paper_integrity_fault,
        paper_settlement_last_attempt: guard.paper_settlement_last_attempt,
        paper_settlement_last_lookup: guard.paper_settlement_last_lookup,
        paper_settlement_last_error: guard.paper_settlement_last_error.clone(),
        paper_settlement_market_failures: guard.paper_settlement_market_failures,
        mode: std::env::var("POLYCOPIER_MODE").unwrap_or_else(|_| "unknown".into()),
        live_ready: live_readiness(&api_state).is_empty(),
        live_blockers: live_readiness(&api_state),
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
        live_feed: guard.live_feed.iter().take(100).cloned().collect(),
        total_balance: guard.total_balance,
        starting_balance,
        unrealized_pnl: guard.unrealized_pnl,
        realized_pnl: if crate::execution_journal::active() {
            crate::live_fills::stored()
                .ok()
                .and_then(|fills| {
                    crate::live_fills::accounted(
                        &fills,
                        &crate::execution_journal::snapshot().ok()?,
                    )
                    .ok()
                })
                .map(|projection| projection.realized_pnl)
                .unwrap_or(guard.realized_pnl)
        } else {
            guard.realized_pnl
        },
        open_cost_basis,
        open_market_value,
        equity,
        total_pnl,
        priced_positions,
        target_positions: guard.target_positions.iter().take(50).cloned().collect(),
        target_positions_total: guard.target_positions.len(),
        copies_executed: guard.copies_executed,
        filled_buy_orders,
        trades_skipped: guard.trades_skipped,
        skip_attempts: guard.skip_attempts.clone(),
        copied_count: guard.copied_count,
        next_scan_secs: guard.next_scan_secs,
        pending_orders: guard.pending_orders.clone(),
        active_orders: guard.active_orders.clone(),
        position_sources,
        position_source_allocations,
        target_performance,
        performance_since: ledger.performance_since.to_rfc3339(),
    };

    Json(response)
}

fn live_readiness(api: &ApiState) -> Vec<String> {
    let mut blockers = Vec::new();
    if std::env::var("POLYCOPIER_MODE").as_deref() != Ok("live") {
        blockers.push(
            "Dashboard is connected to the simulation service; a separate live service is required"
                .to_string(),
        );
    }
    if api.live_client.is_none() {
        blockers.push("Authenticated Polymarket CLOB client is not active".to_string());
    }
    if !crate::config::is_valid_private_key_format(
        &std::env::var("PRIVATE_KEY").unwrap_or_default(),
    ) {
        blockers.push("Signer credential is missing or invalid".to_string());
    }
    if std::env::var("FUNDER_ADDRESS")
        .ok()
        .filter(|value| value.starts_with("0x") && value.len() == 42)
        .is_none()
    {
        blockers.push("Funder wallet is missing or invalid".to_string());
    }
    if !crate::live_redemption::configured() {
        blockers.push(
            "Automatic live redemption credentials or explicit enablement are missing".to_string(),
        );
    }
    blockers
}

#[derive(serde::Deserialize)]
struct TargetPageQuery {
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn get_target_page(
    State(api_state): State<ApiState>,
    Query(query): Query<TargetPageQuery>,
) -> Json<serde_json::Value> {
    let guard = api_state.bot_state.read().await;
    let total = guard.target_positions.len();
    let offset = query.offset.unwrap_or(0).min(total);
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let items: Vec<_> = guard
        .target_positions
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect();
    Json(serde_json::json!({"items":items,"total":total,"offset":offset,"limit":limit}))
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
    let hot_reload = std::env::var("HOT_RELOAD_ENABLED").as_deref() == Ok("true");
    Json(serde_json::json!({ "success": true, "hot_reload": hot_reload })).into_response()
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
        control_revision: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        bot_state,
        copy_ledger,
    };
    start_live_redemption_worker(state.clone());

    let root_path = std::env::current_dir().unwrap().join("web/dist");
    let serve_dir =
        ServeDir::new(&root_path).fallback(ServeFile::new(root_path.join("index.html")));

    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/targets", get(get_target_page))
        .route("/api/config", get(get_config).post(post_config))
        .route("/api/env", get(get_env))
        .route("/api/control", post(control))
        .route("/api/paper/sell", post(paper_sell))
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

#[cfg(test)]
mod performance_tests {
    use super::{record_position_source_allocation, remaining_lot_size, SourceAllocations};
    use crate::copy_ledger::CopyLedger;
    use crate::models::Position;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    #[test]
    fn dashboard_uses_remaining_shares_after_partial_source_sale() {
        let mut ledger = CopyLedger::default();
        ledger
            .record_filled_copy("token".into(), "source".into(), dec!(10), dec!(0.5))
            .unwrap();
        ledger
            .record_source_sale("token", "source", dec!(4), dec!(0.7), dec!(0))
            .unwrap();
        let lot = &ledger.entries[0];
        assert_eq!(lot.realized_shares, dec!(4));
        assert_eq!(remaining_lot_size(lot), dec!(6));
        assert_eq!(remaining_lot_size(lot) * lot.entry_price, dec!(3));
    }

    #[test]
    fn open_shares_keep_source_after_target_removal_or_baseline_change() {
        let mut ledger = CopyLedger::default();
        ledger
            .record_filled_copy("token".into(), "former-source".into(), dec!(10), dec!(0.5))
            .unwrap();
        ledger
            .record_source_sale("token", "former-source", dec!(4), dec!(0.7), dec!(0))
            .unwrap();
        let positions = HashMap::from([(
            "token".into(),
            Position {
                token_id: "token".into(),
                size: dec!(6),
                average_entry_price: dec!(0.5),
            },
        )]);
        let prices = HashMap::from([("token".into(), dec!(0.6))]);
        let mut allocations = SourceAllocations::new();
        record_position_source_allocation(
            &mut allocations,
            &ledger.entries[0],
            &positions,
            &prices,
        );
        assert_eq!(
            allocations["token"]["former-source"],
            (dec!(6), dec!(3), dec!(0.6))
        );
    }
}
