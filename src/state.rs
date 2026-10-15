use crate::models::{ActiveApiOrder, EvaluatedTrade, Position, QueuedOrder, TargetPosition};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap, VecDeque};

fn skip_category(reason: Option<&str>) -> &'static str {
    let reason = reason.unwrap_or("").to_ascii_lowercase();
    if reason.contains("minimum") || reason.contains("below $1") {
        "minimum_order"
    } else if reason.contains("no asks") || reason.contains("no bids") {
        "no_liquidity"
    } else if reason.contains("book unavailable") || reason.contains("order book unavailable") {
        "book_unavailable"
    } else if reason.contains("opening short") || reason.contains("not held") {
        "unowned_sell"
    } else if reason.contains("stale signal") || reason.contains("too old") {
        "stale_signal"
    } else if reason.contains("cash") || reason.contains("balance") || reason.contains("funds") {
        "insufficient_cash"
    } else if reason.contains("fee lookup")
        || reason.contains("fee schedule")
        || reason.contains("fee rate")
    {
        "fee_lookup"
    } else if reason.contains("slippage") || reason.contains("price") {
        "price_limit"
    } else {
        "other"
    }
}
use std::time::Instant;

pub struct BotState {
    pub paper_processed_signals: HashMap<String, i64>,
    /// Realized exits for each still-open paper position, carried into its final risk outcome.
    pub paper_cycle_pnl: HashMap<String, Decimal>,
    /// Completed paper cycles persisted with inventory before risk notification.
    pub paper_outcomes: Vec<(String, Decimal, i64)>,
    pub paper_committed_outcomes: usize,
    /// A failed paper persistence operation requires a restart and reconciliation.
    pub paper_integrity_fault: bool,
    /// Ephemeral health of the automatic paper payout lookup. No position IDs or
    /// upstream response bodies are exposed through the dashboard.
    pub paper_settlement_last_attempt: Option<i64>,
    pub paper_settlement_last_lookup: Option<i64>,
    pub paper_settlement_last_error: Option<String>,
    pub paper_settlement_market_failures: usize,
    pub in_flight_tokens: std::collections::HashSet<String>,
    pub live_fills_ready: bool,
    pub last_live_fill_sync: i64,
    pub copier_positions: HashMap<String, Position>,
    pub foreign_order_tokens: std::collections::HashSet<String>,
    pub risk_outcome_tx: Option<tokio::sync::mpsc::UnboundedSender<Decimal>>,
    pub positions: HashMap<String, Position>,
    pub position_prices: HashMap<String, Decimal>,
    pub price_updated_at: HashMap<String, u64>,
    pub position_details: HashMap<String, crate::markets::Details>,
    pub prices_updated_at: u64,
    pub live_feed: VecDeque<EvaluatedTrade>,
    pub active_orders: Vec<ActiveApiOrder>,
    pub total_balance: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub started: bool,
    pub target_positions: Vec<TargetPosition>,
    pub copies_executed: u32,
    pub trades_skipped: u32,
    /// Categorized skipped attempts since tracking began; unlike trades_skipped,
    /// successful retries do not erase the earlier diagnostic attempt.
    pub skip_attempts: BTreeMap<String, u64>,
    pub rejection_reasons: HashMap<String, String>,

    /// Number of positions WE currently hold that the TARGET also holds.
    /// Set by a dedicated background task that queries both wallets via the API
    /// every 30 seconds -- never inferred from local scanner state.
    pub copied_count: usize,
    /// When the position scanner last completed a full cycle (wall clock).
    /// None until the first scan finishes.
    pub last_scan_at: Option<Instant>,
    /// How many seconds until the next scan is scheduled (set just before sleeping).
    pub next_scan_secs: u64,
    /// When target_positions.cur_price was last refreshed via the dedicated price
    /// refresh task (runs every 20s, independent of scanner urgency).
    pub last_price_refresh_at: Option<Instant>,
    /// Token IDs for which we have a live GTC order in the CLOB that has NOT
    /// yet been filled. Seeded from open CLOB orders at boot, updated by the
    /// strategy engine on submission and by the order watcher on cancellation.
    /// The scanner uses this alongside `positions` to prevent duplicate orders
    /// across bot restarts.
    pub pending_orders: HashMap<String, QueuedOrder>,
    /// When the order watcher last completed a cycle.
    pub last_watcher_run_at: Option<Instant>,
}

impl BotState {
    pub fn reported_positions(&self) -> &HashMap<String, Position> {
        if crate::execution_journal::active() {
            &self.copier_positions
        } else {
            &self.positions
        }
    }
    pub fn new(is_sim: bool, sim_balance: Option<Decimal>) -> Self {
        // Initialize balance to $10,000 for simulation (or override)
        let initial_balance = if is_sim {
            sim_balance.unwrap_or(Decimal::from(10000))
        } else {
            Decimal::from(0)
        };

        Self {
            paper_processed_signals: HashMap::new(),
            paper_cycle_pnl: HashMap::new(),
            paper_outcomes: Vec::new(),
            paper_committed_outcomes: 0,
            paper_integrity_fault: false,
            paper_settlement_last_attempt: None,
            paper_settlement_last_lookup: None,
            paper_settlement_last_error: None,
            paper_settlement_market_failures: 0,
            in_flight_tokens: std::collections::HashSet::new(),
            live_fills_ready: false,
            last_live_fill_sync: 0,
            copier_positions: HashMap::new(),
            foreign_order_tokens: std::collections::HashSet::new(),
            risk_outcome_tx: None,
            positions: HashMap::new(),
            position_prices: HashMap::new(),
            price_updated_at: HashMap::new(),
            position_details: HashMap::new(),
            prices_updated_at: 0,
            live_feed: VecDeque::with_capacity(100),
            active_orders: Vec::new(),
            total_balance: initial_balance,
            unrealized_pnl: Decimal::from(0),
            realized_pnl: Decimal::from(0),
            started: false,
            target_positions: Vec::new(),
            copies_executed: 0,
            trades_skipped: 0,
            skip_attempts: BTreeMap::new(),
            rejection_reasons: HashMap::new(),
            copied_count: 0,
            last_scan_at: None,
            next_scan_secs: 0,
            last_price_refresh_at: None,
            pending_orders: HashMap::new(),
            last_watcher_run_at: None,
        }
    }

    pub fn push_evaluated_trade(&mut self, trade: EvaluatedTrade) {
        // Anti-Spam: The bot evaluates target positions aggressively every ~5 seconds.
        // If a trade is skipped (e.g., insufficient balance, limits), we do NOT want
        // to infinitely increment the skips counter or flood the UI feed with IDENTICAL
        // rejection events every single cycle.
        let event_key = crate::rtds::event_key(&trade.original_event);
        let is_duplicate = self.live_feed.iter().any(|existing| {
            crate::rtds::event_key(&existing.original_event) == event_key
                && existing.validated == trade.validated
                && existing.original_event.token_id == trade.original_event.token_id
                && existing.original_event.side == trade.original_event.side
                && existing.reason == trade.reason
        });

        if !is_duplicate {
            if trade.validated {
                // A transient rejection (for example a Gamma 429) may be retried
                // from the other signal path. Once the same fill is accepted,
                // remove its stale rejection and correct the cumulative counter.
                let before = self.live_feed.len();
                self.live_feed.retain(|existing| {
                    existing.validated
                        || crate::rtds::event_key(&existing.original_event) != event_key
                });
                let resolved_rejections = before - self.live_feed.len();
                self.trades_skipped = self
                    .trades_skipped
                    .saturating_sub(resolved_rejections as u32);
                self.copies_executed += 1;
            } else {
                self.trades_skipped += 1;
                let category = skip_category(trade.reason.as_deref());
                let count = self.skip_attempts.entry(category.to_string()).or_default();
                *count = count.saturating_add(1);
            }

            if self.live_feed.len() == 100 {
                self.live_feed.pop_back();
            }
            self.live_feed.push_front(trade);
        }
    }
}

impl Default for BotState {
    fn default() -> Self {
        Self::new(false, None)
    }
}

#[cfg(test)]
mod skip_category_tests {
    use super::{skip_category, BotState};
    use crate::models::{EvaluatedTrade, TradeEvent, TradeSide};
    use rust_decimal::Decimal;

    #[test]
    fn classifies_operational_and_expected_skips() {
        assert_eq!(
            skip_category(Some("below the $1 order minimum")),
            "minimum_order"
        );
        assert_eq!(
            skip_category(Some("Paper BUY skipped: no asks")),
            "no_liquidity"
        );
        assert_eq!(
            skip_category(Some("Paper SELL book unavailable")),
            "book_unavailable"
        );
        assert_eq!(
            skip_category(Some("SELL skipped: target opening short")),
            "unowned_sell"
        );
        assert_eq!(skip_category(Some("Stale signal: age 23s")), "stale_signal");
        assert_eq!(
            skip_category(Some(
                "BUY skipped: intended copy exceeds available cash (including fee headroom)"
            )),
            "insufficient_cash"
        );
        assert_eq!(
            skip_category(Some("Paper fee schedule unavailable")),
            "fee_lookup"
        );
    }

    #[test]
    fn deduplicates_skip_attempts_but_retains_retry_diagnostic() {
        let event = TradeEvent {
            transaction_hash: "0xskip".into(),
            maker_address: "maker".into(),
            taker_address: "leader".into(),
            token_id: "token".into(),
            price: Decimal::ONE,
            size: Decimal::ONE,
            side: TradeSide::BUY,
            timestamp: 1,
        };
        let skipped = EvaluatedTrade {
            original_event: event.clone(),
            validated: false,
            reason: Some("no asks at limit".into()),
        };
        let mut state = BotState::default();
        state.push_evaluated_trade(skipped.clone());
        state.push_evaluated_trade(skipped);
        assert_eq!(state.trades_skipped, 1);
        assert_eq!(state.skip_attempts["no_liquidity"], 1);
        state.push_evaluated_trade(EvaluatedTrade {
            original_event: event,
            validated: true,
            reason: None,
        });
        assert_eq!(state.trades_skipped, 0);
        assert_eq!(state.skip_attempts["no_liquidity"], 1);
    }
}
