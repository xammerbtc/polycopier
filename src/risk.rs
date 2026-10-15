//! Risk engine — stateful pre-trade checks run by the strategy engine.
//!
//! ## Checks implemented
//!
//! | Check | Env var | Default | Gap |
//! |---|---|---|---|
//! | Micro-trade anti-spoofing | — | $1 minimum | original |
//! | Daily volume limit | `MAX_DAILY_VOLUME_USD` | 0 (disabled) | 12 |
//! | Consecutive-loss circuit breaker | `MAX_CONSECUTIVE_LOSSES` | 0 (disabled) | 12 |

use crate::config::Config;
use crate::models::{TradeEvent, TradeSide};
use rust_decimal::Decimal;
use std::time::Instant;

#[derive(Clone)]
pub struct RiskEngine {
    live_outcome_snapshot: Option<Vec<(String, Decimal, i64)>>,
    live_snapshot_limits: Option<(u32, u64)>,
    seen_live_outcomes: std::collections::BTreeSet<String>,
    config: Config,

    // -- Daily volume tracker (Gap 12) ------------------------------------
    /// Total USD notional traded today (UTC day).
    daily_volume_usd: Decimal,
    /// UTC date when `daily_volume_usd` was last reset.
    daily_reset_date: chrono::NaiveDate,

    // -- Consecutive-loss circuit breaker (Gap 12) ------------------------
    /// How many consecutive BUY-side trade evaluations have been flagged as losses.
    /// A "loss" is defined as the risk engine rejecting a trade for a non-spoofing reason
    /// (i.e., the engine itself prevents placing it).  Reset on any successful approval.
    consecutive_losses: u32,
    /// If `Some(until)`, all new trades are blocked until `Instant::now() >= until`.
    cooldown_until: Option<Instant>,
}

impl RiskEngine {
    fn persist(&self) {
        if let Err(e) = self.persist_result() {
            tracing::error!("Risk state persistence failed: {e}");
        }
    }
    fn persist_result(&self) -> anyhow::Result<()> {
        use std::io::Write;
        let Ok(path) = std::env::var("RISK_STATE_PATH") else {
            return Ok(());
        };
        let until = self.cooldown_until.map(|t| {
            chrono::Utc::now().timestamp()
                + t.saturating_duration_since(Instant::now()).as_secs() as i64
        });
        let raw = serde_json::json!({"date":self.daily_reset_date,"volume":self.daily_volume_usd,"losses":self.consecutive_losses,"cooldown_until":until,"seen_live_outcomes":self.seen_live_outcomes,"live_outcome_snapshot":self.live_outcome_snapshot,"live_snapshot_limits":self.live_snapshot_limits});
        let mut file = std::fs::File::create(format!("{path}.tmp"))?;
        file.write_all(raw.to_string().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(format!("{path}.tmp"), &path)?;
        #[cfg(unix)]
        std::fs::File::open(
            std::path::Path::new(&path)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new(".")),
        )?
        .sync_all()?;
        Ok(())
    }

    pub fn restore_persistent(&mut self) -> anyhow::Result<()> {
        let Ok(path) = std::env::var("RISK_STATE_PATH") else {
            return Ok(());
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let v: serde_json::Value = serde_json::from_str(&raw)?;
        self.live_outcome_snapshot = v
            .get("live_outcome_snapshot")
            .map(|s| serde_json::from_value(s.clone()))
            .transpose()?
            .flatten();
        self.live_snapshot_limits = v
            .get("live_snapshot_limits")
            .map(|s| serde_json::from_value(s.clone()))
            .transpose()?
            .flatten();
        if let Some(ids) = v.get("seen_live_outcomes") {
            self.seen_live_outcomes = serde_json::from_value(ids.clone())?;
        }
        let date: chrono::NaiveDate = serde_json::from_value(v["date"].clone())?;
        if date == chrono::Utc::now().date_naive() {
            self.daily_volume_usd = serde_json::from_value(v["volume"].clone())?;
            self.consecutive_losses = serde_json::from_value(v["losses"].clone())?;
            if let Some(until) = v["cooldown_until"].as_i64() {
                let remaining = until - chrono::Utc::now().timestamp();
                if remaining > 0 {
                    self.cooldown_until =
                        Some(Instant::now() + std::time::Duration::from_secs(remaining as u64));
                }
            }
        }
        Ok(())
    }
    /// Evaluate target-specific checks without charging target notional to our budget.
    pub fn check_signal(&self, event: &TradeEvent) -> Result<(), String> {
        self.check_age(event)?;
        let mut preview = self.clone();
        preview.config.max_daily_volume_usd = Decimal::ZERO;
        preview.check_trade_with_signal_policy(
            event,
            matches!(
                preview.config.sizing_mode,
                crate::config::SizingMode::Fixed | crate::config::SizingMode::BankScaled
            ),
        )
    }

    pub fn check_execution(&mut self, event: &TradeEvent) -> Result<(), String> {
        self.check_age(event)?;
        let losses = self.consecutive_losses;
        let min = self.config.scan_min_amount;
        let max = self.config.scan_max_amount;
        self.config.scan_min_amount = Decimal::ZERO;
        self.config.scan_max_amount = Decimal::MAX;
        let allow_partial_fixed_paper_fill = self.config.is_sim
            && matches!(
                self.config.sizing_mode,
                crate::config::SizingMode::Fixed | crate::config::SizingMode::BankScaled
            );
        let result = self.check_trade_with_signal_policy(event, allow_partial_fixed_paper_fill);
        if result.is_ok() && event.side == TradeSide::BUY && self.cooldown_until.is_none() {
            self.consecutive_losses = losses;
        }
        self.config.scan_min_amount = min;
        self.config.scan_max_amount = max;
        if crate::execution_journal::active() && result.is_ok() {
            self.persist_result()
                .map_err(|_| "Risk state persistence failed".to_string())?;
        } else {
            self.persist();
        }
        result
    }
    pub fn update_config(&mut self, config: Config) {
        self.config = config;
    }
    fn check_age(&self, event: &TradeEvent) -> Result<(), String> {
        // Position sweeps are synthesized locally from a successful API snapshot;
        // they may wait behind a busy event queue and are not stale target prints.
        if event.transaction_hash.starts_with("sweep_") {
            return Ok(());
        }
        let age = chrono::Utc::now().timestamp() - event.timestamp;
        let max_age = if event.side == TradeSide::BUY {
            self.config
                .max_buy_delay_seconds
                .unwrap_or(self.config.max_delay_seconds)
        } else {
            self.config.max_delay_seconds
        };
        if age < -1 || age > max_age {
            return Err(format!(
                "Stale signal: age {age}s exceeds allowed {}s",
                max_age
            ));
        }
        Ok(())
    }
    pub fn record_win(&mut self) {
        self.consecutive_losses = 0;
        self.persist();
    }
    pub fn new(config: Config) -> Self {
        Self {
            live_outcome_snapshot: None,
            live_snapshot_limits: None,
            seen_live_outcomes: std::collections::BTreeSet::new(),
            config,
            daily_volume_usd: Decimal::ZERO,
            daily_reset_date: chrono::Utc::now().date_naive(),
            consecutive_losses: 0,
            cooldown_until: None,
        }
    }

    /// Run all risk checks against an incoming `TradeEvent`.
    ///
    /// Returns `Ok(())` if the trade may proceed, or `Err(reason)` to reject it.
    /// Internally updates daily-volume and consecutive-loss state.
    pub fn check_trade(&mut self, trade: &TradeEvent) -> Result<(), String> {
        self.check_trade_with_signal_policy(trade, false)
    }

    fn check_trade_with_signal_policy(
        &mut self,
        trade: &TradeEvent,
        fixed_copy_signal: bool,
    ) -> Result<(), String> {
        // === 0. Reset daily volume at UTC midnight ===
        let today = chrono::Utc::now().date_naive();
        if today != self.daily_reset_date {
            self.daily_volume_usd = Decimal::ZERO;
            self.daily_reset_date = today;
            self.consecutive_losses = 0; // fresh day resets loss streak too
            tracing::info!("Risk: daily volume/loss counters reset for {today}.");
        }

        let trade_value = trade.size * trade.price;

        // Only apply entry restrictions (spoofing, cooldowns) to BUY events.
        // We MUST always allow closing trades to proceed, regardless of size or cooldown.
        if trade.side == crate::models::TradeSide::BUY {
            // === 1. Anti-spoofing: minimum $1 notional ===
            if trade_value < Decimal::from(1)
                && !fixed_copy_signal
                && !(self.config.is_sim
                    && self.config.sizing_mode == crate::models::SizingMode::TargetScalar)
            {
                return Err("Trade value is too small (spoofing protection)".to_string());
            }

            // === 1.5. Target Size bounds ===
            if trade_value < self.config.scan_min_amount
                || trade_value > self.config.scan_max_amount
            {
                return Err(format!(
                    "Trade USD value ({}) falls outside allowed bounds ({} - {})",
                    trade_value, self.config.scan_min_amount, self.config.scan_max_amount
                ));
            }

            // === 2. Consecutive-loss cooldown (Gap 12) ===
            if let Some(until) = self.cooldown_until {
                if std::time::Instant::now() < until {
                    let remaining = until.duration_since(std::time::Instant::now()).as_secs();
                    return Err(format!(
                        "Risk cooldown active ({remaining}s remaining) after {} consecutive losses",
                        self.config.max_consecutive_losses
                    ));
                } else {
                    // Cooldown expired — reset
                    self.cooldown_until = None;
                    self.consecutive_losses = 0;
                    tracing::info!("Risk: consecutive-loss cooldown expired — resuming.");
                }
            }
        }

        // === 3. Daily volume limit (Gap 12) ===
        if self.config.max_daily_volume_usd > Decimal::ZERO
            && self.daily_volume_usd + trade_value > self.config.max_daily_volume_usd
        {
            return Err(format!(
                "Daily volume limit ${:.2} would be exceeded (used ${:.2}, trade ${:.2})",
                self.config.max_daily_volume_usd, self.daily_volume_usd, trade_value
            ));
        }

        // === All checks passed — update state ===
        self.daily_volume_usd += trade_value;
        if trade.side == TradeSide::BUY {
            // Successful BUY approval resets consecutive-loss counter.
            self.consecutive_losses = 0;
        }

        Ok(())
    }

    /// Record that a trade we approved ultimately resulted in a loss
    /// (called by the order-watcher or strategy engine when an order is
    /// cancelled due to the target's position dropping past max_copy_loss_pct).
    ///
    /// Increments the consecutive-loss counter and triggers a cooldown if
    /// the configured threshold is reached.
    pub fn record_loss(&mut self) {
        self.apply_loss();
        self.persist();
    }
    fn apply_loss(&mut self) {
        if self.config.max_consecutive_losses == 0 {
            return; // feature disabled
        }
        self.consecutive_losses += 1;
        tracing::warn!(
            "Risk: consecutive losses = {} / {}",
            self.consecutive_losses,
            self.config.max_consecutive_losses
        );
        if self.consecutive_losses >= self.config.max_consecutive_losses {
            let until =
                Instant::now() + std::time::Duration::from_secs(self.config.loss_cooldown_secs);
            self.cooldown_until = Some(until);
            tracing::warn!(
                "Risk: consecutive-loss limit hit — cooling down for {}s.",
                self.config.loss_cooldown_secs
            );
        }
    }
    /// Rebuild from the complete, chain-ordered live book. Late finalization may
    /// insert an older outcome before a win we have already observed.
    pub fn sync_live_snapshot(
        &mut self,
        outcomes: &[(String, Decimal, i64)],
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let limits = (
            self.config.max_consecutive_losses,
            self.config.loss_cooldown_secs,
        );
        if self.daily_reset_date == now.date_naive()
            && self.live_outcome_snapshot.as_deref() == Some(outcomes)
            && self.live_snapshot_limits == Some(limits)
        {
            return Ok(());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut previous = i64::MIN;
        let mut losses: u32 = 0;
        let mut until: Option<i64> = None;
        for (id, pnl, timestamp) in outcomes {
            anyhow::ensure!(
                ids.insert(id.clone()) && *timestamp >= previous && *timestamp <= now.timestamp(),
                "Invalid or unordered outcome snapshot"
            );
            previous = *timestamp;
            let at = chrono::DateTime::from_timestamp(*timestamp, 0)
                .ok_or_else(|| anyhow::anyhow!("Invalid realized outcome timestamp"))?;
            if at.date_naive() != now.date_naive() {
                continue;
            }
            if until.is_some_and(|end| end <= *timestamp) {
                losses = 0;
                until = None;
            }
            if *pnl < Decimal::ZERO && limits.0 > 0 {
                losses = losses.saturating_add(1);
                if losses >= limits.0 {
                    until = Some(timestamp.saturating_add(limits.1 as i64));
                }
            } else if *pnl > Decimal::ZERO {
                losses = 0;
            }
        }
        if let Some(previous) = &self.live_outcome_snapshot {
            anyhow::ensure!(
                previous.iter().all(|(id, _, _)| ids.contains(id)),
                "Live outcome snapshot lost a settled trade"
            );
        }
        if until.is_some_and(|end| end <= now.timestamp()) {
            losses = 0;
            until = None;
        }
        let mut next = self.clone();
        if next.daily_reset_date != now.date_naive() {
            next.daily_volume_usd = Decimal::ZERO;
        }
        next.daily_reset_date = now.date_naive();
        next.consecutive_losses = losses;
        next.cooldown_until = until.map(|end| {
            Instant::now() + std::time::Duration::from_secs((end - now.timestamp()) as u64)
        });
        next.seen_live_outcomes.extend(ids);
        next.live_outcome_snapshot = Some(outcomes.to_vec());
        next.live_snapshot_limits = Some(limits);
        next.persist_result()?;
        *self = next;
        Ok(())
    }

    pub fn sync_live_outcomes(
        &mut self,
        outcomes: &[(String, Decimal, i64)],
    ) -> anyhow::Result<()> {
        let mut next = self.clone();
        let now = chrono::Utc::now();
        if next.daily_reset_date != now.date_naive() {
            next.daily_reset_date = now.date_naive();
            next.daily_volume_usd = Decimal::ZERO;
            next.consecutive_losses = 0;
        }
        let mut changed = next.daily_reset_date != self.daily_reset_date;
        for (id, pnl, timestamp) in outcomes {
            if !next.seen_live_outcomes.insert(id.clone()) {
                continue;
            }
            changed = true;
            let at = chrono::DateTime::from_timestamp(*timestamp, 0)
                .ok_or_else(|| anyhow::anyhow!("Invalid realized outcome timestamp"))?;
            if at.date_naive() != now.date_naive() {
                continue;
            }
            if *pnl < Decimal::ZERO {
                next.apply_loss();
                if next.cooldown_until.is_some() {
                    let remaining = timestamp.saturating_add(next.config.loss_cooldown_secs as i64)
                        - now.timestamp();
                    next.cooldown_until = if remaining > 0 {
                        Some(Instant::now() + std::time::Duration::from_secs(remaining as u64))
                    } else {
                        None
                    };
                }
            } else if *pnl > Decimal::ZERO {
                next.consecutive_losses = 0;
            }
        }
        if changed {
            next.persist_result()?;
            *self = next;
        }
        Ok(())
    }
}
