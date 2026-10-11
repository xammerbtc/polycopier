//! Risk engine — stateful pre-trade checks run by the strategy engine.
//!
//! ## Checks implemented
//!
//! | Check | Env var | Default | Gap |
//! |---|---|---|---|
//! | Micro-trade anti-spoofing | — | $1 minimum | original |
//! | Daily volume limit | `MAX_DAILY_VOLUME_USD` | 0 (disabled) | 12 |
//! | Consecutive-loss circuit breaker | `MAX_CONSECUTIVE_LOSSES` | 0 (disabled) | 12 |
//! | Rapid-flip guard | — | 60s cooldown per token | 12 |

use crate::config::Config;
use crate::models::{TradeEvent, TradeSide};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Clone)]
pub struct RiskEngine {
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

    // -- Rapid-flip guard (Gap 12) ----------------------------------------
    /// token_id → time of last BUY approved for that token.
    last_buy_at: HashMap<String, Instant>,
}

impl RiskEngine {
    fn persist(&self) {
        let Ok(path) = std::env::var("RISK_STATE_PATH") else {
            return;
        };
        let until = self.cooldown_until.map(|t| {
            chrono::Utc::now().timestamp()
                + t.saturating_duration_since(Instant::now()).as_secs() as i64
        });
        let raw = serde_json::json!({"date":self.daily_reset_date,"volume":self.daily_volume_usd,"losses":self.consecutive_losses,"cooldown_until":until});
        let result = std::fs::write(format!("{path}.tmp"), raw.to_string())
            .and_then(|_| std::fs::rename(format!("{path}.tmp"), path));
        if let Err(e) = result {
            tracing::error!("Risk state persistence failed: {e}");
        }
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
        preview.check_trade(event)
    }

    pub fn check_execution(&mut self, event: &TradeEvent) -> Result<(), String> {
        self.check_age(event)?;
        let losses = self.consecutive_losses;
        let min = self.config.scan_min_amount;
        let max = self.config.scan_max_amount;
        self.config.scan_min_amount = Decimal::ZERO;
        self.config.scan_max_amount = Decimal::MAX;
        let result = self.check_trade(event);
        if result.is_ok() && event.side == TradeSide::BUY && self.cooldown_until.is_none() {
            self.consecutive_losses = losses;
        }
        self.config.scan_min_amount = min;
        self.config.scan_max_amount = max;
        self.persist();
        result
    }
    pub fn update_config(&mut self, config: Config) {
        self.config = config;
    }
    fn check_age(&self, event: &TradeEvent) -> Result<(), String> {
        let age = chrono::Utc::now().timestamp() - event.timestamp;
        if age < -1 || age > self.config.max_delay_seconds {
            return Err(format!(
                "Stale signal: age {age}s exceeds allowed {}s",
                self.config.max_delay_seconds
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
            config,
            daily_volume_usd: Decimal::ZERO,
            daily_reset_date: chrono::Utc::now().date_naive(),
            consecutive_losses: 0,
            cooldown_until: None,
            last_buy_at: HashMap::new(),
        }
    }

    /// Run all risk checks against an incoming `TradeEvent`.
    ///
    /// Returns `Ok(())` if the trade may proceed, or `Err(reason)` to reject it.
    /// Internally updates daily-volume and consecutive-loss state.
    pub fn check_trade(&mut self, trade: &TradeEvent) -> Result<(), String> {
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
            if trade_value < Decimal::from(1) {
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

        // === 4. Rapid-flip guard (Gap 12): no re-entry within 60 s ===
        if trade.side == TradeSide::BUY {
            if let Some(&last) = self.last_buy_at.get(&trade.token_id) {
                let age_secs = last.elapsed().as_secs();
                if age_secs < 60 {
                    return Err(format!(
                        "Rapid-flip guard: token {} was entered {}s ago — cooldown 60s",
                        &trade.token_id[..trade.token_id.len().min(12)],
                        age_secs
                    ));
                }
            }
        }

        // === All checks passed — update state ===
        self.daily_volume_usd += trade_value;
        if trade.side == TradeSide::BUY {
            self.last_buy_at
                .insert(trade.token_id.clone(), Instant::now());
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
        self.persist();
    }
}
