use crate::clients::OrderSubmitter;
use crate::config::Config;
use crate::copy_ledger::CopyLedger;
use crate::models::{EvaluatedTrade, OrderRequest, SizingMode, TradeEvent, TradeSide};
use crate::risk::RiskEngine;
use crate::state::BotState;
use alloy::primitives::Address;
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::Client as DataClient;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::Duration;
use tracing::{debug, info, warn};

// -- Pure helpers (extracted for testability) ----------------------------------

/// Applies slippage to a price to produce the limit order price.
pub fn calculate_limit_price(price: Decimal, side: TradeSide, slippage_pct: Decimal) -> Decimal {
    // Most Polymarket markets use tick size 0.01 (2 decimal places).
    // A small number of high-liquidity markets use 0.001 (3 dp).
    // Rounding to 2 dp works for all markets: the CLOB accepts 2 dp
    // even when tick size is 0.001, but rejects 3 dp on 0.01-tick markets.
    let max_price = Decimal::new(99, 2); // 0.99
    let min_price = Decimal::new(1, 2); // 0.01
    match side {
        TradeSide::BUY => (price + price * slippage_pct)
            .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToNegativeInfinity)
            .clamp(min_price, max_price),
        TradeSide::SELL => (price - price * slippage_pct)
            .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToPositiveInfinity)
            .clamp(min_price, max_price),
    }
}

/// Caps entry size to max_trade_usd / price, returns the original size if within budget.
pub fn calculate_entry_size(size: Decimal, price: Decimal, max_trade_usd: Decimal) -> Decimal {
    let cost = size * price;
    if cost > max_trade_usd {
        max_trade_usd / price
    } else {
        size
    }
}

/// Minimum share count the Polymarket CLOB enforces per order.
/// Any order with size < 5 shares gets a 400: "Size (X) lower than the minimum: 5"
pub const MIN_ORDER_SHARES: Decimal = Decimal::from_parts(5, 0, 0, false, 0);

/// A sale follows the fraction of the leader's position that the signal exits.
/// `target_shares` is a scanner snapshot; it can be before or after the fill.
pub fn proportional_source_exit(
    our_source_shares: Decimal,
    sold_shares: Decimal,
    target_shares: Option<Decimal>,
    snapshot_after_signal: bool,
) -> Decimal {
    if our_source_shares <= Decimal::ZERO || sold_shares <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let Some(target_shares) = target_shares else {
        // If the scanner has no snapshot, never sell more than the signal's
        // share count. A subsequent confirmed full-exit sweep closes residuals.
        return our_source_shares.min(sold_shares);
    };
    let before = if snapshot_after_signal {
        target_shares + sold_shares
    } else {
        target_shares.max(sold_shares)
    };
    (our_source_shares * sold_shares / before).min(our_source_shares)
}

/// Dollar floor (secondary sanity guard Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ the real constraint is MIN_ORDER_SHARES).
pub const MIN_ORDER_USD: Decimal = Decimal::from_parts(1, 0, 0, false, 0);

/// The marketable BUY is signed at its limit price. A fee-inclusive USDC
/// budget must still imply at least five requested shares at that price;
/// any smaller execution can only be a partial fill of a valid order.
pub fn budget_meets_min_order(budget_usd: Decimal, limit_price: Decimal) -> bool {
    limit_price > Decimal::ZERO && budget_usd / limit_price >= MIN_ORDER_SHARES
}

/// A small USDC order can meet the five-share minimum at a cheaper ask even
/// when it cannot do so at the configured maximum price. Leave fee headroom
/// inside the budget and round down to the conservative cent tick.
pub fn affordable_buy_limit(budget_usd: Decimal, max_price: Decimal) -> Option<Decimal> {
    let fee_buffer = Decimal::new(102, 2);
    let affordable = (budget_usd / (MIN_ORDER_SHARES * fee_buffer))
        .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
    let limit = max_price.min(affordable);
    (limit >= Decimal::new(1, 2)).then_some(limit)
}

/// Compute the USD budget for a single BUY order according to the active [`SizingMode`].
///
/// | Mode | Formula |
/// |---|---|
/// | `Fixed` | `max_trade_usd` (constant) |
/// | `SelfPct` | `our_balance * copy_size_pct`, capped at `max_trade_usd` |
/// | `TargetUsd` | `target_notional` (exact $ the target bet), capped at `max_trade_usd` |
/// | `BankScaled` | up to $10 exact; above $10, `copy_size_pct` of source notional, capped |
///
/// Returns **`Decimal::ZERO`** when the computed budget is below `MIN_ORDER_USD` ($1).
/// Callers treat zero as "skip this order". This respects the user's configured
/// percentage exactly: 7% of $39 = $2.73 Р В Р вЂ Р Р†Р вЂљР’В°Р СћРЎвЂ™ $1 Р В Р вЂ Р Р†Р вЂљР’В Р Р†Р вЂљРІвЂћСћ order placed correctly.
pub fn compute_order_usd(
    our_balance: Decimal,
    sizing_mode: &SizingMode,
    copy_size_pct: Option<Decimal>,
    wallet_scalar: Decimal,
    mut max_trade_usd: Decimal,
    target_notional: Decimal,
) -> Decimal {
    let hard_trade_cap = max_trade_usd;
    // If the user's available balance is smaller than the hard max ceiling,
    // gracefully scale down the max ceiling to their available balance
    // minus an estimated 2% fee overhead buffer (divide by 1.02),
    // ensuring we still enter the trade with "all we have" rather than erroring out.
    let usable_balance = our_balance / Decimal::from_str("1.02").unwrap();
    if usable_balance < max_trade_usd {
        max_trade_usd = usable_balance;
    }

    let desired = match sizing_mode {
        SizingMode::Fixed => max_trade_usd,
        SizingMode::SelfPct => {
            let pct =
                copy_size_pct.unwrap_or_else(|| max_trade_usd / our_balance.max(Decimal::ONE));
            our_balance * pct
        }
        SizingMode::TargetUsd => target_notional,
        SizingMode::TargetScalar => target_notional * wallet_scalar,
        SizingMode::BankScaled => {
            let threshold = Decimal::from(10);
            if target_notional <= threshold {
                target_notional
            } else {
                // Never increase the configured source-notional percentage
                // just to meet the venue minimum; skip undersized signals.
                target_notional * copy_size_pct.unwrap_or(Decimal::ZERO)
            }
        }
    };
    if *sizing_mode == SizingMode::BankScaled && usable_balance < desired.min(hard_trade_cap) {
        // A cash shortage must not silently turn an intended 1:1 copy into a
        // smaller, unrelated bet. The strategy reports a skipped signal.
        return Decimal::ZERO;
    }
    let capped = desired.min(max_trade_usd);
    // Return ZERO to signal "skip" when below the CLOB minimum.
    // Never floor UP Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ that silently overrides the user's sizing config.
    if capped < MIN_ORDER_USD {
        Decimal::ZERO
    } else {
        capped
    }
}

// ---------------------------------------------------------------------------
// Debounce cache with TTL eviction (Gap 5)
// ---------------------------------------------------------------------------

/// Maximum number of entries the debounce cache may hold simultaneously.
/// At ~3 events/s per target Р В РІР‚СљР Р†Р вЂљРІР‚Сњ 4 targets, 512 covers >40 seconds of burst.
const DEBOUNCE_CACHE_CAP: usize = 512;
/// Entries older than this are evicted unconditionally.
const DEBOUNCE_STALE_SECS: u64 = 5;
/// Events for the same key within this window are coalesced (size accumulated).
const DEBOUNCE_WINDOW_SECS: i64 = 1;

struct DebounceCache {
    map: HashMap<String, (TradeEvent, Instant)>,
}

impl DebounceCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Evict all entries older than `DEBOUNCE_STALE_SECS`.
    fn purge_stale(&mut self) {
        self.map
            .retain(|_, (_, inserted)| inserted.elapsed().as_secs() < DEBOUNCE_STALE_SECS);
    }

    /// Insert or accumulate an event.  Returns `true` if the event was debounced
    /// (accumulated into an existing entry) so the caller should `continue`.
    fn insert_or_accumulate(&mut self, key: String, event: TradeEvent) -> bool {
        // Evict stale entries before checking capacity
        self.purge_stale();

        // Enforce capacity ceiling Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ evict oldest entry if at cap
        if self.map.len() >= DEBOUNCE_CACHE_CAP && !self.map.contains_key(&key) {
            // Remove the entry with the oldest insertion time
            if let Some(oldest_key) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest_key);
            }
        }

        if let Some((existing, inserted)) = self.map.get_mut(&key) {
            let age_secs = chrono::Utc::now().timestamp() - existing.timestamp;
            if age_secs < DEBOUNCE_WINDOW_SECS {
                // Accumulate size Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ still within the debounce window
                existing.size += event.size;
                debug!(
                    "Debounced fragmented fill for {}. New size: {}",
                    existing.token_id, existing.size
                );
                return true; // caller should `continue`
            } else {
                // Window expired Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ replace with fresh event
                *existing = event;
                *inserted = Instant::now();
                return false;
            }
        }

        self.map.insert(key, (event, Instant::now()));
        false
    }
}

// ---------------------------------------------------------------------------
// Live query cache (Gap 13)
// ---------------------------------------------------------------------------

/// Cache TTL in seconds for `holds_query` results.  Fresh enough that a trade
/// arriving 3s after a prior one for the same wallet re-uses the cached result,
/// but stale enough that a fast-moving market doesn't use a 10s-old snapshot.
const LIVE_QUERY_CACHE_TTL_SECS: u64 = 3;

struct LiveQueryCache {
    inner: HashMap<String, (bool, Instant)>,
}

impl LiveQueryCache {
    fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    fn get(&self, wallet: &str, token_id: &str) -> Option<bool> {
        let key = format!("{wallet}:{token_id}");
        self.inner.get(&key).and_then(|(result, inserted)| {
            if inserted.elapsed().as_secs() < LIVE_QUERY_CACHE_TTL_SECS {
                Some(*result)
            } else {
                None
            }
        })
    }

    fn set(&mut self, wallet: &str, token_id: &str, holds: bool) {
        let key = format!("{wallet}:{token_id}");
        self.inner.insert(key, (holds, Instant::now()));
    }

    /// Evict expired entries to keep memory bounded.
    fn evict_expired(&mut self) {
        self.inner
            .retain(|_, (_, t)| t.elapsed().as_secs() < LIVE_QUERY_CACHE_TTL_SECS * 4);
    }
}

// ---------------------------------------------------------------------------
// Injectable live position checker
// ---------------------------------------------------------------------------

/// Returns `Some(true)` if `wallet` holds `token_id`, `Some(false)` if not,
/// `None` on error or timeout.
///
/// Injected via [`start_strategy_engine`] so tests can provide a no-op
/// without touching the real network.
pub type HoldsQuery =
    Arc<dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Option<bool>> + Send>> + Send + Sync>;

/// Production implementation Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ makes a live Polymarket Data API call with a
/// 5-second timeout.
pub fn make_live_holds_query() -> HoldsQuery {
    let client = DataClient::default();
    Arc::new(move |wallet: String, token_id: String| {
        let client = client.clone();
        Box::pin(async move {
            let Ok(addr) = Address::from_str(&wallet) else {
                return None;
            };
            let req = PositionsRequest::builder().user(addr).build();
            match tokio::time::timeout(Duration::from_secs(5), client.positions(&req)).await {
                Ok(Ok(positions)) => {
                    Some(positions.iter().any(|p| p.asset.to_string() == token_id))
                }
                Ok(Err(e)) => {
                    warn!(
                        "Live position query failed for {}: {e}",
                        &wallet[..wallet.len().min(10)]
                    );
                    None
                }
                Err(_) => {
                    warn!(
                        "Live position query timed out for {}",
                        &wallet[..wallet.len().min(10)]
                    );
                    None
                }
            }
        })
    })
}

/// No-op implementation for use in integration tests.  Returns `None`
/// immediately, triggering the scanner-cache fallback in the engine.
pub fn make_no_op_holds_query() -> HoldsQuery {
    Arc::new(|_wallet: String, _token_id: String| Box::pin(async { None::<bool> }))
}

// ---------------------------------------------------------------------------
// Injectable market end-date checker
// ---------------------------------------------------------------------------

pub type EndDateQuery = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<chrono::DateTime<chrono::Utc>>> + Send>>
        + Send
        + Sync,
>;

pub fn make_live_end_date_query() -> EndDateQuery {
    let client = polymarket_client_sdk::gamma::Client::default();
    Arc::new(move |token_id: String| {
        let client = client.clone();
        Box::pin(async move {
            let Ok(u) = polymarket_client_sdk::types::U256::from_str(&token_id) else {
                return None;
            };
            let req = polymarket_client_sdk::gamma::types::request::MarketsRequest::builder()
                .clob_token_ids(vec![u])
                .build();
            match tokio::time::timeout(Duration::from_secs(5), client.markets(&req)).await {
                Ok(Ok(markets)) => markets.into_iter().next().and_then(|m| m.end_date),
                Ok(Err(e)) => {
                    warn!(
                        "Live end_date query failed for {}: {}",
                        &token_id[..token_id.len().min(10)],
                        e
                    );
                    None
                }
                Err(_) => {
                    warn!(
                        "Live end_date query timed out for {}",
                        &token_id[..token_id.len().min(10)]
                    );
                    None
                }
            }
        })
    })
}

pub fn make_no_op_end_date_query() -> EndDateQuery {
    Arc::new(|_token_id: String| Box::pin(async { None }))
}

// ---------------------------------------------------------------------------
// End Date Cache
// ---------------------------------------------------------------------------

struct EndDateCache {
    inner: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl EndDateCache {
    fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    async fn get_or_fetch(
        &mut self,
        token_id: &str,
        query: &EndDateQuery,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        if let Some(ed) = self.inner.get(token_id) {
            return Some(*ed);
        }
        if let Some(ed) = query(token_id.to_string()).await {
            if self.inner.len() >= 20000 {
                self.inner.clear();
            }
            self.inner.insert(token_id.to_string(), ed);
            return Some(ed);
        }
        None
    }
}

async fn reconcile_risk(risk: &mut RiskEngine, state: &Arc<RwLock<BotState>>, is_sim: bool) {
    let result = if is_sim {
        crate::simulation::sync_risk(risk, &*state.read().await)
    } else if crate::execution_journal::active() {
        crate::live_fills::sync_risk(risk)
    } else {
        Ok(())
    };
    if let Err(e) = result {
        let mut guard = state.write().await;
        if is_sim {
            guard.started = true;
        } else {
            guard.live_fills_ready = false;
        }
        tracing::error!("Risk reconciliation failed; execution blocked: {e}");
    }
}

#[allow(clippy::too_many_arguments)]
pub fn start_strategy_engine(
    rx: mpsc::Receiver<TradeEvent>,
    state: Arc<RwLock<BotState>>,
    risk_engine: RiskEngine,
    submitter: OrderSubmitter,
    config: Config,
    copy_ledger: Arc<Mutex<CopyLedger>>,
    holds_query: HoldsQuery,
    end_date_query: EndDateQuery,
) {
    start_strategy_engine_with_fees(
        rx,
        state,
        risk_engine,
        submitter,
        config,
        copy_ledger,
        holds_query,
        end_date_query,
        crate::fees::public_query(),
    );
}

#[allow(clippy::too_many_arguments)]
pub fn start_strategy_engine_with_fees(
    rx: mpsc::Receiver<TradeEvent>,
    state: Arc<RwLock<BotState>>,
    risk_engine: RiskEngine,
    submitter: OrderSubmitter,
    config: Config,
    copy_ledger: Arc<Mutex<CopyLedger>>,
    holds_query: HoldsQuery,
    end_date_query: EndDateQuery,
    fee_query: crate::fees::FeeQuery,
) {
    start_strategy_engine_with_queries(
        rx,
        state,
        risk_engine,
        submitter,
        config,
        copy_ledger,
        holds_query,
        end_date_query,
        fee_query,
        Arc::new(|token, size, floor| {
            Box::pin(async move { crate::simulation::paper_sell_ladder(&token, size, floor).await })
        }),
    );
}

pub type PaperSellQuery = Arc<
    dyn Fn(
            String,
            Decimal,
            Option<Decimal>,
        ) -> Pin<
            Box<
                dyn Future<Output = anyhow::Result<Option<crate::simulation::PaperBuyFill>>> + Send,
            >,
        > + Send
        + Sync,
>;

#[allow(clippy::too_many_arguments)]
pub fn start_strategy_engine_with_queries(
    mut rx: mpsc::Receiver<TradeEvent>,
    state: Arc<RwLock<BotState>>,
    mut risk_engine: RiskEngine,
    submitter: OrderSubmitter,
    mut config: Config,
    copy_ledger: Arc<Mutex<CopyLedger>>,
    holds_query: HoldsQuery,
    end_date_query: EndDateQuery,
    fee_query: crate::fees::FeeQuery,
    paper_sell_query: PaperSellQuery,
) {
    tokio::spawn(async move {
        info!("Strategy Engine Started. Monitoring edge cases (debouncing, closures...)");

        let mut debounce = DebounceCache::new();
        let mut live_cache = LiveQueryCache::new();
        let mut end_date_cache = EndDateCache::new();
        let mut fee_cache: HashMap<String, (Decimal, Instant)> = HashMap::new();
        // Periodic cache maintenance counter
        let mut event_count: u32 = 0;
        let (outcome_tx, mut outcome_rx) = mpsc::unbounded_channel::<Decimal>();
        state.write().await.risk_outcome_tx = Some(outcome_tx.clone());
        let mut risk_tick = tokio::time::interval(std::time::Duration::from_secs(2));

        loop {
            let event = tokio::select! {
                biased;
                _ = risk_tick.tick() => {
                    reconcile_risk(&mut risk_engine, &state, config.is_sim).await;
                    continue;
                }
                Some(pnl) = outcome_rx.recv() => {
                    if config.is_sim { reconcile_risk(&mut risk_engine, &state, true).await; }
                    else if pnl < Decimal::ZERO { risk_engine.record_loss(); }
                    else if pnl > Decimal::ZERO { risk_engine.record_win(); }
                    continue;
                }
                event = rx.recv() => match event { Some(event) => event, None => break },
            };
            while let Ok(pnl) = outcome_rx.try_recv() {
                if config.is_sim {
                    reconcile_risk(&mut risk_engine, &state, true).await;
                } else if pnl < Decimal::ZERO {
                    risk_engine.record_loss();
                } else if pnl > Decimal::ZERO {
                    risk_engine.record_win();
                }
            }
            config.hot_reload();
            // Paper mode keeps source-specific lots for multi-wallet attribution.
            // TargetScalar additionally scales each BUY to the source's share size.
            // Finish the preceding fill before sizing this token.
            let paper_source_lots = config.is_sim;
            let proportional_paper =
                paper_source_lots && config.sizing_mode == SizingMode::TargetScalar;
            if paper_source_lots {
                let deadline = Instant::now() + Duration::from_secs(30);
                while state
                    .read()
                    .await
                    .in_flight_tokens
                    .contains(&event.token_id)
                {
                    if Instant::now() >= deadline {
                        state.write().await.started = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
            if config.is_sim
                && state
                    .read()
                    .await
                    .paper_processed_signals
                    .contains_key(&crate::rtds::event_key(&event))
            {
                continue; // A committed paper fill must not execute again after reconnect/restart.
            }
            risk_engine.update_config(config.clone());
            reconcile_risk(&mut risk_engine, &state, config.is_sim).await;
            event_count += 1;
            // Evict expired live-query cache entries every 50 events (Gap 13)
            if event_count.is_multiple_of(50) {
                live_cache.evict_expired();
            }

            let mut eval = EvaluatedTrade {
                original_event: event.clone(),
                validated: true,
                reason: None,
            };

            if state.read().await.started {
                eval.validated = false;
                eval.reason = Some("Operator paused".to_string());
            }

            // 1. Is it a filled trade from the target wallet list?
            if crate::execution_journal::active() {
                let guard = state.read().await;
                if !guard.live_fills_ready
                    || chrono::Utc::now()
                        .timestamp()
                        .saturating_sub(guard.last_live_fill_sync)
                        > 60
                {
                    eval.validated = false;
                    eval.reason = Some("Live fills have not been reconciled".into());
                }
                if crate::execution_journal::blocked()
                    || guard.foreign_order_tokens.contains(&event.token_id)
                {
                    eval.validated = false;
                    eval.reason = Some(
                        "Uncertain execution or another bot's open order requires isolation".into(),
                    );
                }
            }
            // Locally generated exits refer to a ledger source, which may have
            // been removed from the current watch list after its BUY filled.
            if !event.transaction_hash.starts_with("sweep_")
                && !config.target_wallets.contains(&event.taker_address)
            {
                eval.validated = false;
                eval.reason = Some("Wallet mismatch".to_string());
            }

            // 2. Fragmented fill debounce (Gap 5 Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ bounded cache with TTL eviction)
            let cache_key = format!(
                "{}_{}_{:?}",
                event.taker_address, event.token_id, event.side
            );
            if eval.validated
                && !paper_source_lots
                && debounce.insert_or_accumulate(cache_key, event.clone())
            {
                continue; // accumulated Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ skip this event
            }

            // 3. Risk bounds
            if eval.validated {
                if let Err(reason) = risk_engine.check_signal(&event) {
                    eval.validated = false;
                    eval.reason = Some(reason);
                }
            }

            // -- Intent classification: live API + copy ledger ------------------
            //
            // Rules:
            //   ONE-POSITION-PER-TOKEN: once we hold a token (from any target),
            //   ignore BUY events from all other targets for that token.
            //
            //   SELL gating: only the target we originally copied from can trigger
            //   a close.  A SELL from a different target is irrelevant to our
            //   position.
            //
            //   LIVE VERIFICATION (Gap 13 Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ cached): for BUY events we query the
            //   target's wallet live; for SELL events we query OUR wallet live.
            //   Results are cached for LIVE_QUERY_CACHE_TTL_SECS seconds to reduce
            //   API calls and latency for burst activity.

            let mut resolved_end_date = None;

            if eval.validated {
                // Check market closing soon (before resolving holdings to save time if skipped)
                if let Some(skip_mins) = config.ignore_closing_in_mins {
                    if skip_mins > 0 && event.side == TradeSide::BUY {
                        let ed = end_date_cache
                            .get_or_fetch(&event.token_id, &end_date_query)
                            .await;
                        resolved_end_date = ed;
                        if let Some(end_date) = ed {
                            let cutoff =
                                chrono::Utc::now() + chrono::Duration::minutes(skip_mins as i64);
                            if end_date <= cutoff && event.side == TradeSide::BUY {
                                eval.validated = false;
                                eval.reason = Some(format!(
                                    "Market closes in < {} mins (at {})",
                                    skip_mins,
                                    end_date.format("%H:%M UTC")
                                ));
                                warn!("Trade skipped: {}", eval.reason.as_ref().unwrap());
                            }
                        } else {
                            eval.validated = false;
                            eval.reason = Some(
                                "Market end time unavailable; closing-time risk cannot be verified"
                                    .into(),
                            );
                        }
                    }
                }
            }

            if eval.validated {
                // --- Resolve live state for this token ---

                // Our position: check cache first, then live API
                let cache_we_hold = {
                    let guard = state.read().await;
                    guard.positions.contains_key(&event.token_id)
                };
                let cache_target_holds = {
                    let guard = state.read().await;
                    guard.target_positions.iter().any(|p| {
                        p.token_id == event.token_id && p.source_wallet == event.taker_address
                    })
                };

                // Check live-query cache before making API calls (Gap 13)
                let our_cached = if config.is_sim {
                    Some(cache_we_hold)
                } else {
                    live_cache.get(&config.funder_address, &event.token_id)
                };
                let target_cached = live_cache.get(&event.taker_address, &event.token_id);

                let (live_we_hold, live_target_holds) = if our_cached.is_some()
                    && target_cached.is_some()
                {
                    // Both are cached Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ no API call needed
                    (
                        our_cached.unwrap_or(cache_we_hold),
                        target_cached.unwrap_or(cache_target_holds),
                    )
                } else {
                    // Run whichever queries are needed in parallel
                    let (live_we_hold_opt, live_target_holds_opt) = tokio::join!(
                        async {
                            if let Some(cached) = our_cached {
                                Some(cached)
                            } else {
                                holds_query(config.funder_address.clone(), event.token_id.clone())
                                    .await
                            }
                        },
                        async {
                            if let Some(cached) = target_cached {
                                Some(cached)
                            } else {
                                holds_query(event.taker_address.clone(), event.token_id.clone())
                                    .await
                            }
                        },
                    );

                    let we_hold = live_we_hold_opt.unwrap_or(cache_we_hold);
                    let target_holds = live_target_holds_opt.unwrap_or(cache_target_holds);

                    // Populate cache with fresh results
                    live_cache.set(&config.funder_address, &event.token_id, we_hold);
                    live_cache.set(&event.taker_address, &event.token_id, target_holds);

                    (we_hold, target_holds)
                };

                // Ledger lookup for this token (who we copied it from, if anyone).
                let ledger_entry = {
                    let ledger = copy_ledger.lock().await;
                    ledger.find_active_for_token(&event.token_id).cloned()
                };
                let already_in_token = ledger_entry.is_some();
                let untracked_holding =
                    live_we_hold || state.read().await.positions.contains_key(&event.token_id);
                if crate::execution_journal::active() && !already_in_token && untracked_holding {
                    state.write().await.push_evaluated_trade(EvaluatedTrade {
                        original_event: event.clone(),
                        validated: false,
                        reason: Some("Wallet holding is outside this copier's ledger".into()),
                    });
                    continue;
                }

                let skip_reason: Option<String> = match event.side {
                    // ---- BUY -----------------------------------------------
                    TradeSide::BUY => {
                        if !paper_source_lots && !already_in_token && live_we_hold {
                            Some(
                                "BUY skipped: wallet holding is outside this copier's ledger"
                                    .into(),
                            )
                        } else {
                            None // Each distinct fill may add another owned source lot.
                        }
                    }
                    // ---- SELL ----------------------------------------------
                    TradeSide::SELL => {
                        if live_we_hold {
                            let source_owns_shares = {
                                let ledger = copy_ledger.lock().await;
                                ledger.active_size_for_source(&event.token_id, &event.taker_address)
                                    > Decimal::ZERO
                            };
                            match (&ledger_entry, source_owns_shares) {
                                (Some(_), true) => {
                                    None // Correct source is selling Р В Р вЂ Р Р†Р вЂљР’В Р Р†Р вЂљРІвЂћСћ close
                                }
                                (Some(entry), false) => Some(format!(
                                    "SELL skipped: {} sold but we copied from {} Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ \
                                     keeping position",
                                    &event.taker_address[..event.taker_address.len().min(10)],
                                    &entry.source_wallet[..entry.source_wallet.len().min(10)],
                                )),
                                (None, _) => {
                                    warn!(
                                        "SELL: we hold token {} with no ledger entry Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ \
                                         closing defensively.",
                                        &event.token_id[..event.token_id.len().min(12)]
                                    );
                                    None
                                }
                            }
                        } else if let Some(entry) = &ledger_entry {
                            warn!(
                                "SELL: ledger shows active copy of {} from {} but we no longer \
                                 hold it Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ marking closed.",
                                &event.token_id[..event.token_id.len().min(12)],
                                &entry.source_wallet[..entry.source_wallet.len().min(10)],
                            );
                            let mut ledger = copy_ledger.lock().await;
                            if !crate::execution_journal::active() {
                                ledger.record_close(&event.token_id, &entry.source_wallet.clone());
                            }
                            Some(
                                "SELL skipped: position already closed (ledger synced)".to_string(),
                            )
                        } else {
                            if live_target_holds {
                                Some(
                                    "SELL skipped: target closing long we never entered"
                                        .to_string(),
                                )
                            } else {
                                Some(
                                    "SELL skipped: target opening short (not supported)"
                                        .to_string(),
                                )
                            }
                        }
                    }
                };

                if let Some(reason) = skip_reason {
                    warn!("{}", reason);
                    {
                        let mut guard = state.write().await;
                        guard
                            .rejection_reasons
                            .insert(event.token_id.clone(), reason.clone());
                    }
                    eval.validated = false;
                    eval.reason = Some(reason);
                }
            }

            // Update TUI feed for early validations
            {
                let mut guard = state.write().await;
                guard.push_evaluated_trade(eval.clone());
            }

            if eval.validated {
                info!("Trade Validated: {:?}", eval.original_event);

                let cached_fee_rate = if config.is_sim {
                    fee_cache
                        .get(&event.token_id)
                        .filter(|(_, at)| at.elapsed().as_secs() < 60)
                        .map(|(rate, _)| *rate)
                } else {
                    None
                };
                // Start the independent fee lookup now. For paper BUYs it runs while
                // the CLOB order-book request is in flight instead of adding a second
                // network round trip to the signal-to-order path.
                let mut pending_fee_rate = if config.is_sim && cached_fee_rate.is_none() {
                    let query = fee_query.clone();
                    let token = event.token_id.clone();
                    Some(tokio::spawn(async move { query(token).await }))
                } else {
                    None
                };
                let mut fee_rate = cached_fee_rate.unwrap_or_else(|| {
                    if config.is_sim {
                        Decimal::ZERO // replaced by the pending paper lookup before accounting
                    } else {
                        // Reserve the current highest published live taker coefficient.
                        Decimal::new(7, 2)
                    }
                });

                let is_closing = event.side == TradeSide::SELL;
                // This opt-in is set only for the VPS paper experiment. Tests and
                // other simulation runs keep the regular target-price behavior.
                let fixed_budget_buy = event.side == TradeSide::BUY
                    && matches!(
                        config.sizing_mode,
                        SizingMode::Fixed | SizingMode::BankScaled
                    );
                let paper_buy_cap = if config.is_sim && event.side == TradeSide::BUY {
                    std::env::var("PAPER_COPY_MAX_PRICE")
                        .ok()
                        .and_then(|value| Decimal::from_str(&value).ok())
                        .filter(|price| (Decimal::new(1, 2)..=Decimal::new(99, 2)).contains(price))
                        .or_else(|| fixed_budget_buy.then_some(config.max_entry_price))
                } else {
                    None
                };

                // Determine limit price: rounded to 2dp (CLOB tick), capped to [0.01, 0.99]
                let mut limit_price = paper_buy_cap
                    .or_else(|| fixed_budget_buy.then_some(config.max_entry_price))
                    .unwrap_or_else(|| {
                        calculate_limit_price(event.price, event.side, config.max_slippage_pct)
                    });
                let is_sweep_exit =
                    paper_source_lots && event.transaction_hash.starts_with("sweep_");
                let is_profit_exit =
                    is_sweep_exit && event.transaction_hash.starts_with("sweep_profit_");
                let allowed = is_sweep_exit
                    || paper_buy_cap.is_some()
                    || fixed_budget_buy
                    || match event.side {
                        TradeSide::BUY => {
                            limit_price <= event.price * (Decimal::ONE + config.max_slippage_pct)
                        }
                        TradeSide::SELL => {
                            limit_price >= event.price * (Decimal::ONE - config.max_slippage_pct)
                        }
                    };
                if !allowed {
                    state.write().await.push_evaluated_trade(EvaluatedTrade {
                        original_event: event.clone(),
                        validated: false,
                        reason: Some("No valid limit price within configured slippage".into()),
                    });
                    continue;
                }

                // Keep the simulated book reservation provisional until the
                // paper submitter accepts the order. Any later skip or error
                // drops it and returns that liquidity to the paper book.
                let mut paper_reservation: Option<crate::simulation::PaperBuyFill> = None;
                let order: Option<(OrderRequest, Decimal)> = if is_closing {
                    // Sell only the copied allocation of this leader. The $5
                    // budget applies to entries, never to exits.
                    let our_held_size = {
                        let guard = state.read().await;
                        guard
                            .positions
                            .get(&event.token_id)
                            .map(|p| p.size)
                            .unwrap_or(Decimal::ZERO)
                    };

                    let our_held_size = if crate::execution_journal::active() {
                        let ledger = copy_ledger.lock().await;
                        let owned: Decimal = ledger
                            .entries
                            .iter()
                            .filter(|e| !e.closed && e.token_id == event.token_id)
                            .map(|e| e.filled_size.unwrap_or(e.size))
                            .sum();
                        our_held_size.min(owned)
                    } else {
                        our_held_size
                    };
                    let source_shares = {
                        let ledger = copy_ledger.lock().await;
                        ledger.active_size_for_source(&event.token_id, &event.taker_address)
                    };
                    let desired = if is_sweep_exit {
                        // The sweep represents a confirmed full source exit, so
                        // close this source's remaining lot.
                        source_shares.min(our_held_size)
                    } else if proportional_paper {
                        let scalar = config
                            .target_scalars
                            .get(&event.taker_address)
                            .copied()
                            .unwrap_or(Decimal::ONE);
                        (event.size * scalar).min(source_shares).min(our_held_size)
                    } else if source_shares > Decimal::ZERO {
                        let guard = state.read().await;
                        let target_shares = guard
                            .target_positions
                            .iter()
                            .find(|p| {
                                p.token_id == event.token_id
                                    && p.source_wallet == event.taker_address
                            })
                            .map(|p| p.size);
                        let signal_age = chrono::Utc::now()
                            .timestamp()
                            .saturating_sub(event.timestamp)
                            .max(0) as u64;
                        let snapshot_after_signal = guard
                            .last_scan_at
                            .is_some_and(|at| at.elapsed().as_secs() < signal_age);
                        proportional_source_exit(
                            source_shares,
                            event.size,
                            target_shares,
                            snapshot_after_signal,
                        )
                        .min(our_held_size)
                    } else {
                        // Legacy untracked live holdings keep their defensive
                        // buffer until they can be reconciled to a source.
                        our_held_size * config.sell_fee_buffer
                    };
                    let truncated_size = desired.trunc_with_scale(2);
                    if truncated_size <= rust_decimal::Decimal::ZERO {
                        let msg = format!("Dust fractional balance {:.4} truncates to 0.00 Р В Р вЂ Р В РІР‚С™Р Р†Р вЂљРЎСљ skipping limit order logic.", our_held_size);
                        tracing::warn!("{}", msg);
                        let mut guard = state.write().await;
                        guard
                            .rejection_reasons
                            .insert(event.token_id.clone(), msg.clone());
                        guard.push_evaluated_trade(EvaluatedTrade {
                            original_event: event.clone(),
                            validated: false,
                            reason: Some(msg),
                        });
                        None
                    } else if config.is_sim {
                        // Paper SELLs must face the same bid liquidity as live
                        // orders. Source exits use the signal's limit floor;
                        // protective sweeps keep their existing floor policy.
                        let floor = if is_sweep_exit {
                            is_profit_exit.then(|| {
                                (event.price * (Decimal::ONE - config.max_slippage_pct))
                                    .round_dp_with_strategy(
                                        2,
                                        rust_decimal::RoundingStrategy::ToZero,
                                    )
                                    .max(Decimal::new(1, 2))
                            })
                        } else {
                            Some(limit_price)
                        };
                        match paper_sell_query(event.token_id.clone(), truncated_size, floor).await
                        {
                            Ok(Some(fill)) => {
                                info!("Paper SELL: filled {:.4}/{:.4} shares against public bids at average ${:.4}", fill.shares, truncated_size, fill.average_price);
                                let order = Some((
                                    OrderRequest {
                                        token_id: event.token_id.clone(),
                                        price: fill.average_price,
                                        size: fill.shares,
                                        side: event.side,
                                        budget_usd: None,
                                    },
                                    Decimal::ZERO,
                                ));
                                paper_reservation = Some(fill);
                                order
                            }
                            Ok(None) => {
                                state.write().await.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(
                                        "Paper SELL not filled: no public bids available".into(),
                                    ),
                                });
                                None
                            }
                            Err(error) => {
                                state.write().await.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(format!("Paper SELL book unavailable: {error}")),
                                });
                                None
                            }
                        }
                    } else {
                        Some((
                            OrderRequest {
                                token_id: event.token_id.clone(),
                                price: limit_price,
                                size: truncated_size,
                                side: event.side,
                                budget_usd: None,
                            },
                            Decimal::ZERO,
                        ))
                    }
                } else {
                    // -- BUY: size according to the active mode and venue minimum --
                    let current_balance = {
                        let guard = state.read().await;
                        guard.total_balance
                    };
                    // target_notional = what the target just bet in dollar terms
                    let target_notional = event.size * event.price;
                    let wallet_scalar = config
                        .target_scalars
                        .get(&event.taker_address)
                        .cloned()
                        .unwrap_or(Decimal::ONE);
                    let budget_usd = compute_order_usd(
                        current_balance,
                        &config.sizing_mode,
                        config.copy_size_pct,
                        wallet_scalar,
                        config.max_trade_size_usd,
                        target_notional,
                    );
                    if budget_usd <= Decimal::ZERO {
                        let reason = if config.sizing_mode == SizingMode::BankScaled {
                            let coefficient = config.copy_size_pct.unwrap_or(Decimal::ZERO);
                            let desired = if target_notional <= Decimal::from(10) {
                                target_notional
                            } else {
                                target_notional * coefficient
                            };
                            if desired < MIN_ORDER_USD {
                                format!(
                                    "BUY skipped: source ${target_notional:.2} gives ${desired:.2} at the configured copy ratio, below the ${MIN_ORDER_USD} order minimum"
                                )
                            } else {
                                format!(
                                    "BUY skipped: ${desired:.2} intended copy exceeds available cash (including fee headroom)"
                                )
                            }
                        } else {
                            format!(
                                "BUY skipped: source notional ${target_notional:.2} cannot be copied within the available cash and order minimum"
                            )
                        };
                        state.write().await.push_evaluated_trade(EvaluatedTrade {
                            original_event: event.clone(),
                            validated: false,
                            reason: Some(reason),
                        });
                        continue;
                    }
                    if fixed_budget_buy {
                        match affordable_buy_limit(budget_usd, limit_price) {
                            Some(affordable_limit) => limit_price = affordable_limit,
                            None => {
                                state.write().await.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(format!(
                                        "BUY skipped: ${budget_usd:.2} budget cannot request the Polymarket minimum of 5 shares at any valid price"
                                    )),
                                });
                                continue;
                            }
                        }
                    }
                    // Budget exactly reflects the sizing engine's mathematical intent.
                    // (TargetUSD mirrors natively, Scalars scale natively, Fixed/SelfPct override natively)
                    let raw_size = if proportional_paper {
                        // Copy shares exactly; slippage and fees change cash spent,
                        // never silently shrink the requested fraction of the fill.
                        (event.size * wallet_scalar).min(config.max_trade_size_usd / limit_price)
                    } else {
                        // For a fixed budget this is only a conservative risk
                        // estimate. Paper sweeps asks and live signs a USDC
                        // market amount; neither uses this as an order size.
                        budget_usd / limit_price
                    };
                    let share_precision = if proportional_paper { 6 } else { 2 };
                    let mut buy_size = raw_size.round_dp_with_strategy(
                        share_precision,
                        rust_decimal::RoundingStrategy::ToZero,
                    );
                    let mut buy_price = limit_price;

                    if proportional_paper && buy_size <= Decimal::ZERO {
                        let msg = format!(
                            "BUY skipped: copied size rounds below 0.000001 shares (raw {:.8}; target scale {:.3}%)",
                            raw_size,
                            wallet_scalar * Decimal::from(100)
                        );
                        state.write().await.push_evaluated_trade(EvaluatedTrade {
                            original_event: event.clone(),
                            validated: false,
                            reason: Some(msg),
                        });
                        continue;
                    }

                    // Match the CLOB's five-share minimum in paper mode too. A
                    // smaller proportional lot would appear filled in simulation
                    // while the identical live order is rejected by Polymarket.
                    if !fixed_budget_buy && buy_size < MIN_ORDER_SHARES {
                        let msg = format!(
                            "BUY skipped: {:.6} shares below Polymarket minimum of 5 shares (budget=${:.2})",
                            buy_size, budget_usd
                        );
                        warn!("{}", msg);
                        let mut guard = state.write().await;
                        guard
                            .rejection_reasons
                            .insert(event.token_id.clone(), msg.clone());
                        guard.push_evaluated_trade(EvaluatedTrade {
                            original_event: event.clone(),
                            validated: false,
                            reason: Some(msg),
                        });
                        continue;
                    }

                    if paper_buy_cap.is_some() && !proportional_paper {
                        if let Some(task) = pending_fee_rate.take() {
                            match task.await {
                                Ok(Ok(rate)) => {
                                    fee_rate = rate;
                                    if fee_cache.len() >= 20000 {
                                        fee_cache.clear();
                                    }
                                    fee_cache
                                        .insert(event.token_id.clone(), (rate, Instant::now()));
                                }
                                Ok(Err(e)) => {
                                    state.write().await.push_evaluated_trade(EvaluatedTrade {
                                        original_event: event.clone(),
                                        validated: false,
                                        reason: Some(format!(
                                            "Paper fee schedule unavailable: {e}"
                                        )),
                                    });
                                    continue;
                                }
                                Err(e) => {
                                    state.write().await.push_evaluated_trade(EvaluatedTrade {
                                        original_event: event.clone(),
                                        validated: false,
                                        reason: Some(format!("Paper fee lookup failed: {e}")),
                                    });
                                    continue;
                                }
                            }
                        }
                    }

                    if paper_buy_cap.is_some() {
                        let max_price = limit_price;
                        let paper_fill = if proportional_paper {
                            crate::simulation::paper_buy_ladder(
                                &event.token_id,
                                buy_size,
                                max_price,
                            )
                            .await
                        } else {
                            crate::simulation::paper_buy_budget_ladder(
                                &event.token_id,
                                budget_usd,
                                max_price,
                                fee_rate,
                            )
                            .await
                        };
                        match paper_fill {
                            Ok(Some(fill)) => {
                                buy_size = fill.shares;
                                buy_price = fill.average_price;
                                info!(
                                    "Paper FAK ladder: filled {:.2} shares for a ${:.2} budget, average ${:.4}, cap ${:.2}; remainder canceled",
                                    fill.shares, budget_usd, fill.average_price, max_price
                                );
                                paper_reservation = Some(fill);
                            }
                            Ok(None) => {
                                state.write().await.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(format!("Paper BUY skipped: no asks available at or below ${max_price:.2}")),
                                });
                                continue;
                            }
                            Err(e) => {
                                state.write().await.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(format!("Paper order book unavailable: {e}")),
                                });
                                continue;
                            }
                        }
                    }

                    // A partial fill may leave fewer than five shares, but the
                    // submitted order itself must satisfy the minimum above.
                    if buy_size <= Decimal::ZERO {
                        let msg =
                            format!("BUY skipped: no shares filled (budget=${:.2})", budget_usd);
                        warn!("{}", msg);
                        let mut guard = state.write().await;
                        guard
                            .rejection_reasons
                            .insert(event.token_id.clone(), msg.clone());
                        guard.push_evaluated_trade(EvaluatedTrade {
                            original_event: event.clone(),
                            validated: false,
                            reason: Some(msg),
                        });
                        None
                    } else {
                        if let Some(task) = pending_fee_rate.take() {
                            match task.await {
                                Ok(Ok(rate)) => {
                                    fee_rate = rate;
                                    if fee_cache.len() >= 20000 {
                                        fee_cache.clear();
                                    }
                                    fee_cache
                                        .insert(event.token_id.clone(), (rate, Instant::now()));
                                }
                                Ok(Err(e)) => {
                                    state.write().await.push_evaluated_trade(EvaluatedTrade {
                                        original_event: event.clone(),
                                        validated: false,
                                        reason: Some(format!(
                                            "Paper fee schedule unavailable: {e}"
                                        )),
                                    });
                                    continue;
                                }
                                Err(e) => {
                                    state.write().await.push_evaluated_trade(EvaluatedTrade {
                                        original_event: event.clone(),
                                        validated: false,
                                        reason: Some(format!("Paper fee lookup failed: {e}")),
                                    });
                                    continue;
                                }
                            }
                        }
                        // Pre-check balance taking the maximum CTF fee overhead into account
                        // Reserve paper's current market fee / conservative live overhead.
                        let p = buy_price;
                        let max_ctf_fee = crate::fees::taker_fee(buy_size, p, fee_rate);
                        let order_cost = buy_size * p;
                        let total_cost = if fixed_budget_buy && !config.is_sim {
                            // The live FAK builder signs a USDC amount and keeps
                            // taker fees inside this total budget.
                            budget_usd
                        } else {
                            order_cost + max_ctf_fee
                        };
                        if fixed_budget_buy && config.is_sim && total_cost > budget_usd {
                            state.write().await.push_evaluated_trade(EvaluatedTrade {
                                original_event: event.clone(),
                                validated: false,
                                reason: Some("Paper fill exceeds the fee-inclusive budget".into()),
                            });
                            continue;
                        }

                        if current_balance < total_cost {
                            let msg = format!(
                                "Insufficient USDC balance: have ${:.2}, need ${:.2}",
                                current_balance, total_cost
                            );
                            warn!("{}", msg);
                            let mut guard = state.write().await;
                            guard
                                .rejection_reasons
                                .insert(event.token_id.clone(), msg.clone());
                            guard.push_evaluated_trade(EvaluatedTrade {
                                original_event: event.clone(),
                                validated: false,
                                reason: Some(msg),
                            });
                            None
                        } else {
                            // Check whether we already have a pending GTC order for this token.
                            let already_pending = {
                                let guard = state.read().await;
                                guard.pending_orders.contains_key(&event.token_id)
                            };
                            if already_pending {
                                let msg = "BUY skipped: live GTC order already exists".to_string();
                                warn!("{}", msg);
                                let mut guard = state.write().await;
                                guard
                                    .rejection_reasons
                                    .insert(event.token_id.clone(), msg.clone());
                                guard.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(msg),
                                });
                                None
                            } else {
                                Some((
                                    OrderRequest {
                                        token_id: event.token_id.clone(),
                                        price: buy_price,
                                        size: buy_size,
                                        side: event.side,
                                        budget_usd: (fixed_budget_buy && !config.is_sim)
                                            .then_some(budget_usd),
                                    },
                                    total_cost,
                                ))
                            }
                        }
                    }
                };

                if let Some(task) = pending_fee_rate.take() {
                    match task.await {
                        Ok(Ok(rate)) => {
                            fee_rate = rate;
                            if fee_cache.len() >= 20000 {
                                fee_cache.clear();
                            }
                            fee_cache.insert(event.token_id.clone(), (rate, Instant::now()));
                        }
                        Ok(Err(e)) => {
                            if event.side == TradeSide::SELL {
                                warn!("Paper SELL fee schedule unavailable; applying zero fee and recording the fee uncertainty: {e}");
                            } else {
                                state.write().await.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(format!("Paper fee schedule unavailable: {e}")),
                                });
                                continue;
                            }
                        }
                        Err(e) => {
                            if event.side == TradeSide::SELL {
                                warn!("Paper SELL fee lookup failed; applying zero fee and recording the fee uncertainty: {e}");
                            } else {
                                state.write().await.push_evaluated_trade(EvaluatedTrade {
                                    original_event: event.clone(),
                                    validated: false,
                                    reason: Some(format!("Paper fee lookup failed: {e}")),
                                });
                                continue;
                            }
                        }
                    }
                }

                if let Some((order, cost_to_deduct)) = order {
                    let mut execution = event.clone();
                    execution.price = order.price;
                    execution.size = order.size;
                    if let Err(reason) = risk_engine.check_execution(&execution) {
                        state.write().await.push_evaluated_trade(EvaluatedTrade {
                            original_event: event.clone(),
                            validated: false,
                            reason: Some(reason),
                        });
                        continue;
                    }
                    // Register token as pending BEFORE spawning so any concurrent
                    // events for the same token are blocked immediately.
                    {
                        let mut guard = state.write().await;
                        if guard.started {
                            tracing::info!("Execution stopped by operator pause before submission");
                            continue;
                        }
                        guard.in_flight_tokens.insert(order.token_id.clone());
                        guard.pending_orders.insert(
                            order.token_id.clone(),
                            crate::models::QueuedOrder {
                                token_id: order.token_id.clone(),
                                price: order.price,
                                size: order.size,
                                side: order.side,
                                event_end_date: resolved_end_date,
                            },
                        );

                        // Eagerly secure the margin requirement from our balance!
                        // This prevents rapid-fire sequential trades from overlapping
                        // against the same stale balance and throwing CLOB 400 bounds!
                        if cost_to_deduct > Decimal::ZERO {
                            guard.total_balance -= cost_to_deduct;
                        }
                    }

                    let submitter_clone = submitter.clone();
                    let state_clone = state.clone();
                    let token_id_clone = order.token_id.clone();
                    let source_wallet_clone = event.taker_address.clone();
                    let source_notional_usd = event.size * event.price;
                    let is_closing = event.side == TradeSide::SELL;
                    let order_size = order.size;
                    let order_price = order.price;
                    let ledger_clone = copy_ledger.clone();

                    // Capture entry_price from ledger BEFORE spawning for realized PnL.
                    let entry_price = if is_closing {
                        let ledger = copy_ledger.lock().await;
                        ledger.average_price_for_source(&event.token_id, &event.taker_address)
                    } else {
                        None
                    };

                    let is_sim = config.is_sim;
                    let outcome_tx = outcome_tx.clone();
                    let latency_event = event.clone();

                    tokio::spawn(async move {
                        if crate::execution_journal::active() {
                            crate::execution_journal::remember_source(
                                &token_id_clone,
                                &source_wallet_clone,
                            );
                        }
                        crate::latency::prepare(&latency_event);
                        let submitted = submitter_clone(order).await;
                        crate::latency::clear_prepared(&token_id_clone);
                        crate::execution_journal::clear_source(&token_id_clone);
                        match submitted {
                            Ok(()) => {
                                if let Some(fill) = &mut paper_reservation {
                                    fill.commit_reservation();
                                }
                                crate::latency::accepted(&latency_event);
                                let mut ledger = ledger_clone.lock().await;
                                if is_sim {
                                    let mut guard = state_clone.write().await;
                                    let mut closed_outcome = None;
                                    let mut accounted = true;
                                    if is_closing {
                                        let fee = crate::fees::taker_fee(
                                            order_size,
                                            order_price,
                                            fee_rate,
                                        );
                                        let source_basis = ledger.sale_cost_basis(
                                            &token_id_clone,
                                            &source_wallet_clone,
                                            order_size,
                                        );
                                        let sale_result = if let Some(source_basis) = source_basis {
                                            crate::simulation::sell_with_cost_basis(
                                                &mut guard,
                                                &token_id_clone,
                                                order_size,
                                                order_price,
                                                fee,
                                                source_basis,
                                            )
                                        } else {
                                            Err(anyhow::anyhow!(
                                                "source lots do not cover paper SELL"
                                            ))
                                        };
                                        match sale_result {
                                            Ok(outcome) => {
                                                if let Err(e) = ledger.record_source_sale(
                                                    &token_id_clone,
                                                    &source_wallet_clone,
                                                    order_size,
                                                    order_price,
                                                    fee,
                                                ) {
                                                    guard.started = true;
                                                    guard.paper_integrity_fault = true;
                                                    accounted = false;
                                                    tracing::error!("Paper source PnL attribution failed; execution paused: {e}");
                                                }
                                                if let Some(pnl) = outcome {
                                                    closed_outcome = Some(pnl);
                                                }
                                            }
                                            Err(e) => {
                                                guard.started = true;
                                                accounted = false;
                                                tracing::error!("Paper fill accounting failed; execution paused: {e}");
                                            }
                                        }
                                    } else {
                                        // The reservation includes the BUY fee; carry that full
                                        // basis through later partial exits and redemption.
                                        let previous =
                                            guard.positions.get(&token_id_clone).cloned();
                                        let total_size = order_size
                                            + previous
                                                .as_ref()
                                                .map(|p| p.size)
                                                .unwrap_or(Decimal::ZERO);
                                        let total_basis = cost_to_deduct
                                            + previous
                                                .as_ref()
                                                .map(|p| p.size * p.average_entry_price)
                                                .unwrap_or(Decimal::ZERO);
                                        let average_entry_price = total_basis / total_size;
                                        guard.positions.insert(
                                            token_id_clone.clone(),
                                            crate::models::Position {
                                                token_id: token_id_clone.clone(),
                                                size: total_size,
                                                average_entry_price,
                                            },
                                        );
                                        if previous.is_none() {
                                            guard.paper_cycle_pnl.remove(&token_id_clone);
                                        }
                                        if let Err(e) = ledger.record_filled_copy_with_source(
                                            token_id_clone.clone(),
                                            source_wallet_clone.clone(),
                                            order_size,
                                            cost_to_deduct / order_size,
                                            source_notional_usd,
                                        ) {
                                            guard.started = true;
                                            guard.paper_integrity_fault = true;
                                            accounted = false;
                                            tracing::error!("Paper BUY ledger persistence failed; execution paused: {e}");
                                        }
                                    }
                                    guard.pending_orders.remove(&token_id_clone);
                                    guard.unrealized_pnl = guard
                                        .positions
                                        .values()
                                        .filter_map(|p| {
                                            guard.position_prices.get(&p.token_id).map(|price| {
                                                (*price - p.average_entry_price) * p.size
                                            })
                                        })
                                        .sum();
                                    if accounted {
                                        let cutoff = chrono::Utc::now().timestamp() - 600;
                                        guard
                                            .paper_processed_signals
                                            .retain(|_, timestamp| *timestamp >= cutoff);
                                        guard.paper_processed_signals.insert(
                                            crate::rtds::event_key(&latency_event),
                                            latency_event.timestamp,
                                        );
                                    }
                                    let saved = if guard.paper_integrity_fault {
                                        Err(anyhow::anyhow!("Paper ledger needs reconciliation"))
                                    } else if ledger.is_persistent() {
                                        crate::simulation::commit(&mut guard)
                                    } else {
                                        guard.paper_committed_outcomes = guard.paper_outcomes.len();
                                        Ok(())
                                    };
                                    if let Err(e) = saved {
                                        guard.started = true;
                                        guard.paper_integrity_fault = true;
                                        tracing::error!(
                                            "Paper fill persistence failed; execution paused: {e}"
                                        );
                                    } else if let Some(pnl) = closed_outcome {
                                        let _ = outcome_tx.send(pnl);
                                    }
                                } else if is_closing && crate::execution_journal::active() {
                                    info!("Live SELL accepted; ledger and realized PnL await confirmed fills");
                                } else if is_closing {
                                    ledger.record_close(&token_id_clone, &source_wallet_clone);
                                    info!(
                                        "Ledger: closed {} from {}",
                                        &token_id_clone[..token_id_clone.len().min(12)],
                                        &source_wallet_clone[..source_wallet_clone.len().min(10)],
                                    );
                                    // Accumulate realized PnL
                                    if let Some(avg_entry) = entry_price {
                                        let pnl = (order_price - avg_entry) * order_size;
                                        let fee = Decimal::ZERO;
                                        let mut guard = state_clone.write().await;
                                        guard.realized_pnl += pnl - fee;
                                        // Subtract this token's contribution from unrealized;
                                        // price refresh corrects the full sum within 20s.
                                        let old_unrealized = (order_price - avg_entry) * order_size;
                                        guard.unrealized_pnl -= old_unrealized;
                                    }
                                } else {
                                    if !crate::execution_journal::active()
                                        || !ledger.has_any_active(&token_id_clone)
                                    {
                                        ledger.record_copy(
                                            token_id_clone.clone(),
                                            source_wallet_clone.clone(),
                                            order_size,
                                            order_price,
                                        );
                                    }
                                    info!(
                                        "Ledger: recorded copy of {} from {}",
                                        &token_id_clone[..token_id_clone.len().min(12)],
                                        &source_wallet_clone[..source_wallet_clone.len().min(10)],
                                    );
                                }
                            }
                            Err(e) => {
                                if e.is::<crate::execution_journal::UncertainExecution>() {
                                    state_clone.write().await.started = true;
                                    tracing::error!("{e}; pending margin remains reserved");
                                    return;
                                }
                                // Remove from pending on failure so the order can be retried.
                                let mut guard = state_clone.write().await;
                                guard.pending_orders.remove(&token_id_clone);

                                // RESTORE the unused eagerly-deducted margin cap!
                                if cost_to_deduct > Decimal::ZERO {
                                    guard.total_balance += cost_to_deduct;
                                }

                                tracing::error!("Execution failed: {}", e);
                            }
                        }
                        state_clone
                            .write()
                            .await
                            .in_flight_tokens
                            .remove(&token_id_clone);
                    });
                }
            } else {
                let reason = eval.reason.unwrap_or_default();
                warn!("Skipped trade: {}", reason);
                // Gap 15: If the event was originated by the scanner but failed strategy/risk validations,
                // securely sink the rejection reason back into BotState so the dashboard UI surfaces it,
                // instead of silently preserving "Monitoring" status infinitely.
                if eval.original_event.transaction_hash.starts_with("scan_") {
                    let mut guard = state.write().await;
                    guard
                        .rejection_reasons
                        .insert(eval.original_event.token_id.clone(), reason);
                }
            }
        }
    });
}

#[cfg(test)]
mod proportional_copy_size_tests {
    use super::*;

    #[test]
    fn paper_copy_preserves_sub_cent_share_precision_without_changing_live_precision() {
        let paper_size = Decimal::new(54_945, 7)
            .round_dp_with_strategy(6, rust_decimal::RoundingStrategy::ToZero);
        let live_size = Decimal::new(54_945, 7)
            .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
        assert_eq!(paper_size, Decimal::new(5_494, 6));
        assert_eq!(live_size, Decimal::ZERO);
    }
}
